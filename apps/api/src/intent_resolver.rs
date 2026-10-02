//! Natural-language resolution for the command centre (docs/requests/REQ-032, slice 4).
//!
//! The palette's box does three things with what is typed into it: it opens screens, it searches,
//! and — since this slice — it *reads*. This module is the reading: a phrase goes in, a structured
//! intent comes out, and nothing else happens. Resolution never executes anything; the panel shows
//! what was understood and the operator decides.
//!
//! Two readings, in one order:
//!
//! 1. **The model**, when the installation has one connected: `POST /ai/chat` with a closed
//!    vocabulary (the providers the index answers for, the commands *this caller* may run) and a
//!    strict JSON answer. The answer is **normalised against the platform's own tables** — a
//!    command id the registry does not hold, a provider the index does not answer for, a count
//!    outside the sane range are all dropped rather than trusted — so a wrong answer from a model
//!    degrades to a reading the platform can stand behind.
//! 2. **The grammar** ([`omnion_search::intent`]), always: it is what answers when no model is
//!    connected, when the model is slow (a bounded wait, [`MODEL_TIMEOUT`]) or when its answer
//!    could not be used. The reader never waits on a model to have a usable box.
//!
//! Everything the two readings produce is permission-aware: a command the caller lacks the key for
//! is not named, a domain they cannot read is not offered, and an interpretation with nowhere to
//! go says so in plain words rather than pretending to be runnable.

use std::time::Duration;

use omnion_ai_hub::{ChatMessage, ChatRequest, ChatRole, ProviderTarget, resolve as resolve_model};
use omnion_permissions::EffectivePermissions;
use omnion_search::commands;
use omnion_search::intent::{self, Alternative, Intent, IntentKind};
use serde_json::Value;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// How long the model gets before the deterministic reading answers instead. A palette that waits
/// on a provider is a palette that does not work; six seconds is long enough for a small answer
/// and short enough that a stuck provider costs the reader one breath.
pub const MODEL_TIMEOUT: Duration = Duration::from_secs(6);

/// Answer budget for one interpretation. Generous on purpose: a reasoning model spends tokens
/// before it prints the JSON, and a cut-off answer is an unusable one.
const MAX_TOKENS: u32 = 1200;

/// How many results an interpreted count may ask for at most.
const MAX_LIMIT: u32 = 50;

/// One phrase, read — with everything the panel needs to present and act on it.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// What was understood.
    pub intent: Intent,
    /// `local` (the grammar) or `model` (an AI Hub model read it).
    pub source: &'static str,
    /// `true` when a model was configured but could not answer, so the grammar stood in.
    pub degraded: bool,
    /// One line in plain words when the reading needs a caveat ("Tickets is not something this
    /// panel indexes yet").
    pub note: Option<String>,
    /// Where running it lands: a screen for a navigation command, the results screen for a search.
    /// `None` for an action command (the panel runs it through the run endpoint) and for anything
    /// that is not runnable.
    pub route: Option<String>,
    /// `true` when the interpretation has a destination the caller may open and enough confidence
    /// to act on. The panel shows Run for exactly these and nothing else.
    pub runnable: bool,
    /// What else the words could have meant — screens and searches only, never actions.
    pub alternatives: Vec<Alternative>,
    /// `provider/model` that read the phrase, when a model did.
    pub model: Option<String>,
}

impl Resolution {
    /// The reading without a model: the grammar, permission-filtered, with its own caveats.
    #[must_use]
    pub fn from_grammar(query: &str, allows: &dyn Fn(&str) -> bool) -> Self {
        let mut intent = intent::parse(query);
        intent.keep_only_commands(allows);
        Self::finish(intent, "local", false, None, allows)
    }

    /// Fill in everything derived from an intent, so both readings answer the same shape.
    fn finish(
        intent: Intent,
        source: &'static str,
        degraded: bool,
        model: Option<String>,
        allows: &dyn Fn(&str) -> bool,
    ) -> Self {
        let runnable = intent.is_runnable(allows);
        let route = if runnable {
            match intent.kind {
                // A navigation command opens its screen; an action command is run by the panel
                // through the run endpoint, so it has no route of its own to hand back.
                IntentKind::Command => {
                    if intent.runs_through_a_service() {
                        None
                    } else {
                        intent.command_route().map(str::to_owned)
                    }
                }
                IntentKind::Search => Some(intent.search_url()),
                IntentKind::Unclear => None,
            }
        } else {
            None
        };
        let note = caveat(&intent);
        let alternatives = intent::alternatives(&intent, allows);

        Self {
            intent,
            source,
            degraded,
            note,
            route,
            runnable,
            alternatives,
            model,
        }
    }
}

/// Read a phrase into an intent — through the model when there is one, through the grammar always.
///
/// The order is deliberate: the model is asked first and given a bounded wait; whatever it answers
/// is normalised against the registry and the provider table; and an answer that cannot be used
/// leaves the grammar's own reading standing, flagged as such. Nothing here executes anything.
pub async fn resolve(
    state: &AppState,
    current: &CurrentSession,
    query: &str,
    permissions: &EffectivePermissions,
) -> Result<Resolution, ApiError> {
    // The permission set itself travels between the awaits rather than a borrowed closure: a
    // `&dyn Fn` is not `Send`, and a future that holds one across an await cannot be a handler.
    let fallback = Resolution::from_grammar(query, &|key| permissions.allows(key));

    match read_with_a_model(state, current, query, permissions).await? {
        ModelAttempt::Read { intent, model } => Ok(Resolution::finish(
            intent,
            "model",
            false,
            Some(model),
            &|key| permissions.allows(key),
        )),
        // No model is connected: the grammar *is* the reader, and nothing is degraded.
        ModelAttempt::NoModel => Ok(fallback),
        // A model was there and did not answer: the reading stands, and the panel says where it
        // came from.
        ModelAttempt::Failed => Ok(Resolution {
            degraded: true,
            note: Some(with_note(
                fallback.note,
                "The model did not answer in time, so this reading is local.",
            )),
            ..fallback
        }),
        // It answered, but with nothing the registry or the index recognises: the grammar's own
        // reading stands, so the card never shows a reading nobody can act on.
        ModelAttempt::Unusable => Ok(Resolution {
            degraded: true,
            note: Some(with_note(
                fallback.note,
                "The model's answer could not be used, so this reading is local.",
            )),
            ..fallback
        }),
    }
}

/// What the model did with the phrase.
enum ModelAttempt {
    /// It read it, and the reading survived normalisation.
    Read { intent: Intent, model: String },
    /// No model is connected to the installation.
    NoModel,
    /// A model is connected but did not answer in time, or failed outright.
    Failed,
    /// A model answered, and the answer carried no reading the platform can use.
    Unusable,
}

/// Ask the installation's model what the phrase means, within [`MODEL_TIMEOUT`].
async fn read_with_a_model(
    state: &AppState,
    current: &CurrentSession,
    query: &str,
    permissions: &EffectivePermissions,
) -> Result<ModelAttempt, ApiError> {
    let resolved = match resolve_model(state.db().pool(), None).await {
        Ok(resolved) => resolved,
        Err(error) => {
            // Nothing connected, or nothing routable: the grammar reads the phrase. A routing
            // problem is not the reader's problem — the box still has to work.
            tracing::debug!(%error, "no model can read this phrase; the grammar answers instead");
            return Ok(ModelAttempt::NoModel);
        }
    };

    let request = ChatRequest {
        model: resolved.model.model_key.clone(),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: system_prompt(&|key| permissions.allows(key)),
            },
            ChatMessage {
                role: ChatRole::User,
                content: query.trim().to_owned(),
            },
        ],
        // A reading is not a creative act: the same phrase should read the same way twice.
        temperature: Some(0.0),
        max_tokens: Some(MAX_TOKENS),
    };

    let target = ProviderTarget::from_provider(&resolved.provider);
    let model_id = resolved.id();

    // The wait is bounded here rather than at the provider: a model that is thinking for six
    // seconds has already lost the argument with a box somebody is typing into. The call is
    // abandoned when the timeout wins — the reader gets the grammar's reading instead.
    let answered = tokio::time::timeout(MODEL_TIMEOUT, ask(&target, &request)).await;
    let content = match answered {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            tracing::warn!(%error, model = %model_id, "the model could not read a palette phrase");
            audit(state, current, query, None, "failed").await;
            return Ok(ModelAttempt::Failed);
        }
        Err(_) => {
            audit(state, current, query, None, "timeout").await;
            return Ok(ModelAttempt::Failed);
        }
    };
    match normalize(&content, &|key| permissions.allows(key)) {
        Some(intent) => Ok(ModelAttempt::Read {
            intent,
            model: model_id,
        }),
        None => {
            audit(state, current, query, None, "unusable").await;
            Ok(ModelAttempt::Unusable)
        }
    }
}

/// One answer from the provider, in one piece.
async fn ask(
    target: &ProviderTarget,
    request: &ChatRequest,
) -> Result<String, omnion_ai_hub::AiHubError> {
    Ok(omnion_ai_hub::chat(target, request).await?.content)
}

/// Write one `command.resolve` entry — only for readings a **model** produced, which are the ones
/// worth reviewing. The grammar's reading is reproducible from the words themselves.
///
/// No row of content travels with the entry, exactly as the request asks: the interpreted intent
/// (domain, command, confidence) and the words are enough to see what a model understood.
async fn audit(
    state: &AppState,
    current: &CurrentSession,
    query: &str,
    outcome: Option<&Intent>,
    status: &'static str,
) {
    let entry = omnion_audit::NewAuditEntry::by_user(current.user.id, "command.resolve")
        .organization(current.user.organization_id)
        .target("command_center", "resolve")
        .metadata(serde_json::json!({
            "status": status,
            "query": query.chars().take(intent::MAX_QUERY_CHARS).collect::<String>(),
            "entity": outcome.and_then(|intent| intent.entity.clone()),
            "provider": outcome.and_then(|intent| intent.provider),
            "command_id": outcome.and_then(|intent| intent.command_id),
            "confidence": outcome.map(|intent| intent.confidence),
        }));
    if let Err(error) = omnion_audit::record(state.db().pool(), entry).await {
        tracing::warn!(%error, "the palette resolution audit row could not be written");
    }
}

/// The closed vocabulary the model is allowed to answer in.
fn system_prompt(allows: &dyn Fn(&str) -> bool) -> String {
    let providers = omnion_search::PROVIDERS
        .iter()
        .filter(|spec| allows(spec.permission))
        .map(|spec| format!("{} ({})", spec.key, spec.title))
        .collect::<Vec<_>>()
        .join(", ");
    let commands = commands::visible(allows)
        .into_iter()
        .map(|spec| format!("{} — {}", spec.id, spec.title))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You read one phrase an operator typed into a platform's command palette and answer with \
         their intent as JSON. Answer with JSON only: no prose, no code fences, no explanation.\n\
         \n\
         Shape:\n\
         {{\"kind\":\"search\"|\"command\"|\"unclear\",\"entity\":string|null,\
         \"provider\":string|null,\"command_id\":string|null,\"query\":string,\
         \"assignee\":string|null,\"limit\":number|null,\"sort\":\"newest\"|\"oldest\"|\"title\"|null,\
         \"confidence\":number}}\n\
         \n\
         The searchable domains (use one as \"provider\", otherwise null): {providers}\n\
         \n\
         The commands that may be named in \"command_id\" (otherwise null):\n{commands}\n\
         \n\
         Rules:\n\
         - \"query\" holds the words that are neither filters nor a command, as the operator wrote \
         them; never invent words.\n\
         - \"assignee\" holds a person's name only when the phrase is about one.\n\
         - \"limit\" holds a count only when the phrase asks for one (\"last 10\"), at most 50.\n\
         - A phrase that asks for an order or a count is a search, not a command.\n\
         - \"confidence\" is your own certainty, 0 to 1. A phrase you cannot read gets a low one.\n\
         - Prefer \"kind\":\"unclear\" over guessing."
    )
}

/// Read one model answer into an intent, keeping only what the platform really has.
///
/// Every field is checked against the same tables the grammar uses: an unknown command id, a
/// provider the index does not answer for (or one this caller cannot read), a sort order that is
/// not one of the three, a count past the cap — all dropped. `None` means the answer carried no
/// usable reading at all, and the caller falls back to the grammar.
#[must_use]
pub fn normalize(content: &str, allows: &dyn Fn(&str) -> bool) -> Option<Intent> {
    let value = extract_json(content)?;
    let object = value.as_object()?;

    let command_id = object
        .get("command_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .and_then(|id| {
            commands::visible(allows)
                .into_iter()
                .find(|spec| spec.id == id)
                .map(|spec| spec.id)
        });

    let provider = object
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .and_then(|key| {
            omnion_search::provider(key)
                .filter(|spec| allows(spec.permission))
                .map(|spec| spec.key)
        });

    let entity = object
        .get("entity")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|word| !word.is_empty() && word.chars().count() <= 40)
        .map(str::to_owned);

    let query = object
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(intent::MAX_QUERY_CHARS)
        .collect::<String>();

    let mut filters = Vec::new();
    if let Some(assignee) = object
        .get("assignee")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.chars().count() <= 60)
    {
        filters.push(intent::IntentFilter {
            key: "assignee",
            label: "assignee",
            value: assignee.to_owned(),
        });
    }

    let limit = object
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, u64::from(MAX_LIMIT)) as u32);

    let sort = object
        .get("sort")
        .and_then(Value::as_str)
        .map(str::trim)
        .and_then(|value| match value {
            "newest" => Some("newest"),
            "oldest" => Some("oldest"),
            "title" => Some("title"),
            _ => None,
        });

    let confidence = object
        .get("confidence")
        .and_then(Value::as_f64)
        .map_or(0.5, |value| (value as f32).clamp(0.0, 0.95));

    let mut intent = Intent {
        kind: IntentKind::Unclear,
        entity,
        provider,
        command_id,
        query,
        filters,
        sort,
        limit,
        confidence,
    };
    intent.kind = if intent.command_id.is_some() {
        IntentKind::Command
    } else if provider.is_some()
        || !intent.filters.is_empty()
        || sort.is_some()
        || limit.is_some()
        || intent.entity.is_some()
        || !intent.query.trim().is_empty()
    {
        IntentKind::Search
    } else {
        // Nothing survived the checks above: an answer that named only a command the platform does
        // not hold is no reading at all, and the grammar reads the phrase instead.
        IntentKind::Unclear
    };

    // The permission rule is the platform's, not the model's: a command outside the caller's keys
    // is dropped here even if a model named it, exactly as the grammar drops it.
    intent.keep_only_commands(allows);

    // A reading the platform cannot use is no reading: the caller falls back to the grammar
    // instead of showing a card that says nothing.
    (intent.kind != IntentKind::Unclear).then_some(intent)
}

/// The JSON object inside a model answer, fences and prose around it included.
#[must_use]
pub fn extract_json(content: &str) -> Option<Value> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&content[start..=end]).ok()
}

/// Join two caveats into one line, whichever of them exists.
fn with_note(existing: Option<String>, and_this: &str) -> String {
    match existing {
        Some(note) => format!("{and_this} {note}"),
        None => and_this.to_owned(),
    }
}

/// One caveat in plain words, when the reading needs it.
fn caveat(intent: &Intent) -> Option<String> {
    match intent.kind {
        IntentKind::Unclear => Some(
            "I could not read a task in these words — here is what else they could mean."
                .to_owned(),
        ),
        _ => {
            let entity = intent.entity.clone()?;
            if intent.provider.is_some() {
                return None;
            }
            Some(format!(
                "“{entity}” is not something this panel indexes yet — Edit as search looks for the words instead."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller holding exactly `keys`.
    fn allows(keys: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |key: &str| keys.contains(&key)
    }

    #[test]
    fn an_answer_wrapped_in_prose_still_reads() {
        let answer = "Sure — here you go:\n```json\n{\"kind\":\"search\",\"provider\":\"pages\"}\n```\nDone.";
        let value = extract_json(answer).expect("the object is in there");
        assert_eq!(value["provider"], "pages");

        assert!(extract_json("no object here").is_none());
        assert!(extract_json("{broken").is_none());
    }

    #[test]
    fn a_command_the_registry_does_not_hold_is_no_reading_at_all() {
        let answer = r#"{"kind":"command","command_id":"nav.teleport","confidence":0.9}"#;
        // The answer's only content was a command the platform does not have, so nothing usable is
        // left: the grammar reads the phrase instead of the card showing a reading nobody can act on.
        assert!(normalize(answer, &allows(&["search.read"])).is_none());
    }

    #[test]
    fn a_command_the_caller_may_not_run_is_dropped_too() {
        let answer = r#"{"kind":"command","command_id":"nav.sites","query":"acme release notes","confidence":0.9}"#;
        let intent = normalize(answer, &allows(&["search.read"])).expect("a reading");
        assert_eq!(intent.command_id, None, "sites.read is not held");
        assert_eq!(intent.kind, IntentKind::Search, "the words still search");
        assert_eq!(intent.query, "acme release notes");

        let allowed =
            normalize(answer, &allows(&["search.read", "sites.read"])).expect("a reading");
        assert_eq!(allowed.command_id, Some("nav.sites"));
        assert_eq!(allowed.kind, IntentKind::Command);
    }

    #[test]
    fn a_provider_the_index_does_not_answer_for_is_dropped() {
        let answer = r#"{"kind":"search","provider":"tickets","query":"billing export"}"#;
        let intent = normalize(answer, &allows(&["search.read"])).expect("a reading");
        assert_eq!(intent.provider, None, "no tickets provider exists");
        assert_eq!(intent.query, "billing export");
        assert_eq!(intent.kind, IntentKind::Search);
    }

    #[test]
    fn a_provider_the_caller_cannot_read_is_dropped() {
        let answer = r#"{"kind":"search","provider":"pages","query":"release notes"}"#;
        let intent = normalize(answer, &allows(&["search.read"])).expect("a reading");
        assert_eq!(intent.provider, None, "content.pages.read is not held");

        let allowed =
            normalize(answer, &allows(&["search.read", "content.pages.read"])).expect("a reading");
        assert_eq!(allowed.provider, Some("pages"));
    }

    #[test]
    fn counts_and_confidence_are_clamped_and_orders_are_closed() {
        let answer =
            r#"{"kind":"search","provider":"pages","limit":900,"sort":"sideways","confidence":4}"#;
        let intent = normalize(answer, &allows(&["content.pages.read"])).expect("a reading");
        assert_eq!(intent.limit, Some(MAX_LIMIT));
        assert_eq!(intent.sort, None);
        assert!(intent.confidence <= 0.95);
    }

    #[test]
    fn the_shapes_kind_follows_what_the_answer_really_carried() {
        // A model that says "command" without naming one is a search of its own words.
        let answer = r#"{"kind":"command","query":"the newest pages","provider":"pages"}"#;
        let intent = normalize(answer, &allows(&["content.pages.read"])).expect("a reading");
        assert_eq!(intent.kind, IntentKind::Search);

        // An answer with nothing usable in it is no reading at all.
        assert!(normalize(r#"{"kind":"search"}"#, &allows(&[])).is_none());
        assert!(normalize("{}", &allows(&[])).is_none());
    }

    #[test]
    fn a_reading_that_cannot_be_run_says_why_in_plain_words() {
        let resolution =
            Resolution::from_grammar("Open Mehmet's last 10 tickets", &allows(&["search.read"]));
        assert!(!resolution.runnable);
        assert!(resolution.route.is_none());
        assert!(
            resolution
                .note
                .unwrap_or_default()
                .contains("not something this panel indexes"),
            "a domain with no screen says so"
        );
        assert!(!resolution.alternatives.is_empty());

        let pages = Resolution::from_grammar("newest pages", &allows(&["content.pages.read"]));
        assert!(pages.runnable);
        assert_eq!(
            pages.route.as_deref(),
            Some("/search?q=pages&type=pages&sort=newest")
        );
        assert!(pages.note.is_none());

        let member = Resolution::from_grammar("newest pages", &allows(&["search.read"]));
        assert!(
            !member.runnable,
            "a domain the caller cannot read is not runnable"
        );
    }

    #[test]
    fn an_action_command_has_no_route_and_never_runs_from_the_reading() {
        let resolution =
            Resolution::from_grammar("rebuild the search index", &allows(&["search.manage"]));
        assert!(resolution.runnable);
        assert!(
            resolution.route.is_none(),
            "the panel runs it, it does not open it"
        );
        assert!(resolution.intent.runs_through_a_service());
    }

    #[test]
    fn the_prompt_offers_only_what_the_caller_may_reach() {
        let member = system_prompt(&allows(&["search.read", "content.pages.read"]));
        assert!(member.contains("pages (Pages)"));
        assert!(member.contains("nav.pages — Open pages"));
        assert!(!member.contains("nav.sites — Open sites"));
        assert!(!member.contains("sites (Sites)"));
        assert!(member.contains("The commands that may be named"));
    }
}
