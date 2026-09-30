//! `/api/v1/ai/workflows` — the draft console's surface (docs/requests/REQ-046, slice 3).
//!
//! Slice 1 wrote the store and the generation as **library functions**; nothing on the wire
//! could reach them, so `POST /api/v1/ai/chat` was the only AI route in the platform and the
//! draft table had exactly one writer that was not a handler. This module is that wire.
//!
//! Three decisions are load-bearing and each is a decision that could have gone the other way:
//!
//! * **The generation answers as `text/event-stream`, not as a POST that returns 201.** The
//!   console's spec asks for a progress panel (`plan → validate → repair?`), and a progress
//!   panel over a request that answers once is a progress panel that invents its own steps.
//!   Frames are `stage` (what the platform is doing), `done` (the draft's id, so the console
//!   can open it) and `error` (a stable code and a message for a person). The frames carry
//!   **no prose** — the model's answer is validated *before* the `done` frame, so a client
//!   that renders deltas would be rendering a definition that may still be refused.
//! * **Permissions are borrowed, not invented.** Reading is `workflows.read`, spending is
//!   `ai.chat` (the same key the console's own generate button sits behind), and deciding is
//!   `workflows.manage` — the decision *is* the materialisation of a workflow, and `approve`
//!   is where a disabled workflow appears. `ai.chat` rather than a new `ai.workflows.*` key
//!   for the same reason slice 2 gave for `ai.prompt`: a key no role carries means "nobody"
//!   until an administrator visits the catalogue.
//! * **Tenancy is the store's `where`, not a check afterwards.** A draft of another tenant is
//!   *absent*, so a direct id fetch answers `404` and a list answers empty — a handler that
//!   distinguished "no such draft" from "not yours" would leak which ids exist.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_ai_hub::resolve;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::workflows::site_in_scope;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

use omnion_module_ai as ai;

/// How many stage frames may queue in front of a client before the stream waits for it.
pub(crate) const STREAM_BUFFER: usize = 8;

/// Longest a prompt may be, in **characters**.
///
/// The column's check counts characters too, so the two agree; the console's live counter
/// reads this number from the same place, and a client-side limit of 2000 against a column of
/// 4000 would make the form refuse prompts the platform accepts — the kind of difference that
/// looks like a bug in one of the two and is really a difference between two vocabularies.
const MAX_PROMPT_CHARS: usize = ai::MAX_PROMPT_LEN;

/// Shortest a prompt may be once trimmed.
const MIN_PROMPT_CHARS: usize = 10;

/// Rows a page of the console list returns when the caller does not ask for a size.
const LIST_PAGE_DEFAULT: i64 = 20;

/// Most rows a page returns.
const LIST_PAGE_MAX: i64 = 100;

/// Worked examples the empty state offers as click-to-fill prompts.
const EXAMPLES: &[(&str, &str, &str)] = &[
    (
        "Chase overdue invoices",
        "If an invoice is 7 days overdue, email the customer; if 14 days overdue, create a \
         task for the sales owner.",
        "The request's own example. Two branches on one age field, one external step each.",
    ),
    (
        "Publish every morning",
        "Every weekday at 8:00, take the latest draft article, mark it published and notify \
         the editors in chat.",
        "A schedule trigger with a chained step that reads the previous step's output.",
    ),
    (
        "Tag new signups",
        "When a new user signs up, tag them as new and send them the welcome email.",
        "An event trigger, the shortest form a rule can take.",
    ),
];

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One draft as the console list renders it.
///
/// `definition` is deliberately **absent** here even though the row has it: a list of fifty
/// drafts would carry fifty definitions the list never draws, and the review screen fetches
/// the one it shows. The list carries what it draws and the detail carries the rest — the same
/// split `WorkflowBody::build` makes with `step_count` versus `steps`.
#[derive(Debug, Serialize)]
pub struct DraftSummary {
    /// Draft id.
    pub id: Uuid,
    /// Title the model gave it.
    pub title: String,
    /// Lifecycle state.
    pub status: String,
    /// The model that wrote it, frozen at generation.
    pub model_key: Option<String>,
    /// Who asked for it.
    pub created_by: Option<Uuid>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// The workflow it became, once it has.
    pub workflow_id: Option<Uuid>,
    /// Whether a validated definition is stored.
    pub has_definition: bool,
    /// Why generation failed, when it did. The list shows it so a `failed` row is not a row
    /// with a status and no cause — a failure only visible in a log is a failure the operator
    /// cannot see from where they hit it.
    pub error: Option<String>,
    /// What the whole draft cost, in tokens.
    pub tokens: i64,
}

impl DraftSummary {
    pub(crate) fn build(draft: &ai::AiWorkflowDraft) -> Self {
        let mut tokens = ai::DraftTokens::default();
        tokens.add(
            draft.tokens_input.map(i64::from),
            draft.tokens_output.map(i64::from),
        );
        Self {
            id: draft.id,
            title: draft.title.clone(),
            status: draft.status.clone(),
            model_key: draft.model_key.clone(),
            created_by: draft.created_by,
            created_at: draft.created_at,
            updated_at: draft.updated_at,
            workflow_id: draft.workflow_id,
            has_definition: draft.has_definition(),
            error: draft.error.clone(),
            tokens: tokens.total(),
        }
    }
}

/// One draft as the review screen renders it.
#[derive(Debug, Serialize)]
pub struct DraftBody {
    /// Draft id.
    pub id: Uuid,
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// Site it is scoped to, when it is.
    pub site_id: Option<Uuid>,
    /// Title.
    pub title: String,
    /// The prompt it answers.
    pub prompt: String,
    /// The model's explanation, markdown.
    pub rationale: Option<String>,
    /// The validated definition.
    pub definition: Option<Value>,
    /// Lifecycle state.
    pub status: String,
    /// The workflow it materialised.
    pub workflow_id: Option<Uuid>,
    /// The model that wrote it.
    pub model_key: Option<String>,
    /// Input tokens across every answer.
    pub tokens_input: i64,
    /// Output tokens across every answer.
    pub tokens_output: i64,
    /// Why generation failed.
    pub error: Option<String>,
    /// The last revision note an operator sent.
    pub revision_note: Option<String>,
    /// How many revision round-trips this draft has had.
    pub revision_count: i32,
    /// Who asked for it.
    pub created_by: Option<Uuid>,
    /// Who decided.
    pub decided_by: Option<Uuid>,
    /// Why it was rejected.
    pub decision_reason: Option<String>,
    /// The read-only step list the review screen draws, straight from the definition.
    pub steps: Vec<StepSummary>,
    /// Whether there is something to decide.
    ///
    /// **This field exists because the review screen's approval bar is derived from it, and
    /// the client cannot derive it itself.** The bar's `approvable` reads
    /// `draft.has_definition`, and the TypeScript type declared the field as a `boolean`
    /// while the wire never sent it — so the value was `undefined` at runtime, `approvable`
    /// was permanently `false`, and **every button in the decision bar was disabled on a
    /// draft that was perfectly approvable**. TypeScript was the only thing that disagreed
    /// with the compiler here: a declared type is an assertion about the wire, and nothing
    /// checks it against the server except a person clicking the screen.
    #[serde(default)]
    pub has_definition: bool,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// When a person decided.
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
}

/// One step of a draft's definition.
///
/// Derived from the stored definition rather than from a second table, and the reason is
/// load-bearing: an operator reviews *the definition* (slice 1's promise — a generated
/// workflow is an ordinary definition), so a step list the engine can disagree with would be
/// the one part of the review screen that is not the thing being approved.
#[derive(Debug, Serialize)]
pub struct StepSummary {
    /// Position, counting from 1 — the same number `{{steps.1.output.x}}` uses.
    pub position: i32,
    /// The step's own name.
    pub name: String,
    /// Step kind; `task` for every action a definition can carry today.
    pub kind: String,
    /// The action it runs, when it is a task.
    pub action: Option<String>,
    /// The parameters, as written.
    pub params: Value,
}

impl DraftBody {
    pub(crate) fn build(draft: &ai::AiWorkflowDraft) -> Self {
        let steps = draft
            .definition()
            .and_then(|definition| definition.get("steps"))
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| StepSummary {
                        position: i32::try_from(index + 1).unwrap_or(i32::MAX),
                        name: step
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        kind: step
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("task")
                            .to_owned(),
                        action: step
                            .get("action")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        params: step.get("params").cloned().unwrap_or_else(|| json!({})),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            id: draft.id,
            organization_id: draft.organization_id,
            site_id: draft.site_id,
            title: draft.title.clone(),
            prompt: draft.prompt.clone(),
            rationale: draft.rationale.clone(),
            definition: draft.definition().cloned(),
            status: draft.status.clone(),
            workflow_id: draft.workflow_id,
            model_key: draft.model_key.clone(),
            tokens_input: i64::from(draft.tokens_input.unwrap_or_default().max(0)),
            tokens_output: i64::from(draft.tokens_output.unwrap_or_default().max(0)),
            error: draft.error.clone(),
            revision_note: draft.revision_note.clone(),
            revision_count: draft.revision_count,
            created_by: draft.created_by,
            decided_by: draft.decided_by,
            decision_reason: draft.decision_reason.clone(),
            steps,
            // The decision bar is drawn from this; see the field's doc comment.
            has_definition: draft.has_definition(),
            created_at: draft.created_at,
            updated_at: draft.updated_at,
            decided_at: draft.decided_at,
        }
    }
}

/// The list, plus the vocabulary a form needs to render itself.
#[derive(Debug, Serialize)]
pub struct DraftListBody {
    /// The page's rows, newest first.
    pub drafts: Vec<DraftSummary>,
    /// How many rows match the filter, ignoring the page window — the count above the pager
    /// must come from the same `where` as the rows, or a filtered list says "12" above one row.
    pub total: i64,
    /// Every status a draft may hold, for the multi-select.
    pub statuses: Vec<&'static str>,
    /// How many rows a page holds by default, so the client does not have to know.
    pub page_size: i64,
}

/// One author's row count, for the "created by" select.
#[derive(Debug, Serialize)]
pub struct AuthorBody {
    /// The author.
    pub id: Uuid,
    /// How many drafts of this organization they have.
    pub drafts: i64,
}

/// One worked example, for the empty state's click-to-fill prompts.
#[derive(Debug, Serialize)]
pub struct ExampleBody {
    /// The example's name.
    pub title: &'static str,
    /// The prompt to fill the form with.
    pub prompt: &'static str,
    /// What it demonstrates.
    pub note: &'static str,
}

/// `GET /api/v1/ai/workflows/examples` — worked examples and the action vocabulary.
#[derive(Debug, Serialize)]
pub struct VocabularyBody {
    /// The click-to-fill examples.
    pub examples: Vec<ExampleBody>,
    /// The closed action registry, **read from `omnion_workflows::actions`** rather than
    /// written out here: this endpoint is what makes "the vocabulary is closed" checkable
    /// from outside the platform, and a copy in a route file is a copy that drifts.
    pub actions: Vec<ActionBody>,
}

/// One action in the closed registry.
#[derive(Debug, Serialize)]
pub struct ActionBody {
    /// `action` — the key a step names.
    pub action: String,
    /// What it does, in the engine's own words.
    pub summary: String,
    /// Whether the engine runs it or the host does.
    ///
    /// The console draws this, and it is worth drawing: `ai.prompt` is a host action, so a
    /// reviewer looking at a draft can see that the step leaves the process before approving a
    /// rule that will fire on a schedule.
    pub host: bool,
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// What a list asks for. Every field is optional and every field is in the URL, so a reload
/// and the QA walkthrough land on the same view — the console's own requirement, and the
/// reason the pagination is an offset rather than a cursor.
#[derive(Debug, Default, Deserialize)]
pub struct DraftListQuery {
    /// Comma-separated statuses to keep; empty means every status.
    pub status: Option<String>,
    /// Free text over title and prompt.
    pub q: Option<String>,
    /// Restrict to one author.
    pub by: Option<Uuid>,
    /// Site scope.
    pub site_id: Option<Uuid>,
    /// Organization, for a platform account.
    pub organization_id: Option<Uuid>,
    /// Rows to skip.
    pub offset: Option<i64>,
    /// Rows to return.
    pub limit: Option<i64>,
}

impl DraftListQuery {
    /// The store's filter, with the bounds the console's own pager uses.
    ///
    /// The `limit` clamp lives here rather than in the store because the store is also
    /// called by handlers that pass their own numbers: clamping once, at the edge, is where a
    /// caller asking for a million rows is refused instead of served.
    fn into_filter(self) -> ai::DraftFilter {
        let statuses = self
            .status
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();

        ai::DraftFilter {
            statuses,
            query: self.q,
            created_by: self.by,
            offset: self.offset.unwrap_or_default().max(0),
            limit: self.limit.unwrap_or(LIST_PAGE_DEFAULT).clamp(1, LIST_PAGE_MAX),
        }
    }
}

/// The generate form's body.
#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    /// The operator's sentence.
    pub prompt: String,
    /// `provider/model`, or absent for the installation's default.
    pub model: Option<String>,
    /// Site scope, when the rule is about one site.
    pub site_id: Option<Uuid>,
    /// Organization, for a platform account.
    pub organization_id: Option<Uuid>,
    /// One spare row for a title a human typed; the model's own wins when it sent one.
    pub title: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/workflows/drafts` — one organization's drafts.
pub async fn list_drafts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DraftListQuery>,
) -> Result<Json<DraftListBody>, ApiError> {
    let organization_id = organization_of(&current, query.organization_id)?;
    if let Some(site_id) = query.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let page = ai::list_drafts(state.db().pool(), organization_id, &query.into_filter())
        .await
        .map_err(store_error)?;

    Ok(Json(DraftListBody {
        drafts: page.drafts.iter().map(DraftSummary::build).collect(),
        total: page.total,
        statuses: ai::STATUSES.to_vec(),
        page_size: LIST_PAGE_DEFAULT,
    }))
}

/// `GET /api/v1/ai/workflows/drafts/{id}` — one draft, for the review screen.
pub async fn get_draft(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(draft_id): Path<Uuid>,
) -> Result<Json<DraftBody>, ApiError> {
    let draft = draft_in_scope(&state, &current, draft_id).await?;
    Ok(Json(DraftBody::build(&draft)))
}

/// `GET /api/v1/ai/workflows/drafts/authors` — the "created by" select's options.
///
/// Its own route rather than a field on the list: the list is re-fetched on every filter
/// change, and the roster changes far less often than the page does.
pub async fn list_authors(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<OrganizationQuery>,
) -> Result<Json<Vec<AuthorBody>>, ApiError> {
    let organization_id = organization_of(&current, query.organization_id)?;
    let authors = ai::list_authors(state.db().pool(), organization_id)
        .await
        .map_err(store_error)?;

    Ok(Json(
        authors
            .into_iter()
            .map(|(id, drafts)| AuthorBody { id, drafts })
            .collect(),
    ))
}

/// `GET /api/v1/ai/workflows/examples` — the empty state's prompts and the action vocabulary.
///
/// Read-only and free of a database round-trip on purpose: it is the one endpoint the console
/// loads **before** it knows whether a provider is connected, because the no-provider state is
/// one of the three things the empty state has to be able to say. The action list comes from
/// the same registry the generation prompt is built from, so the vocabulary the console shows
/// and the vocabulary the model is told are the same list by construction.
pub async fn examples() -> Result<Json<VocabularyBody>, ApiError> {
    let examples = EXAMPLES
        .iter()
        .map(|(title, prompt, note)| ExampleBody {
            title,
            prompt,
            note,
        })
        .collect();
    let actions = omnion_workflows::actions::keys()
        .into_iter()
        .map(|action| ActionBody {
            summary: omnion_workflows::actions::ACTIONS
                .iter()
                .chain(omnion_workflows::actions::HOST_ACTIONS)
                .find(|def| def.key == action)
                .map_or_else(String::new, |def| def.description.to_owned()),
            host: omnion_workflows::actions::is_host_action(action),
            action: action.to_owned(),
        })
        .collect();

    Ok(Json(VocabularyBody { examples, actions }))
}

/// `POST /api/v1/ai/workflows/generate` — a prompt becomes a draft, streamed.
///
/// The row is written **before** the provider is called, exactly as slice 1 decided: a
/// generation that dies mid-flight leaves a `failed` row with its reason rather than nothing
/// at all. That is why this handler answers a stream and not a `201` with a body — the
/// failure has to land somewhere the operator can read it, and the console's error state reads
/// the row.
pub async fn generate(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<GenerateBody>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let organization_id = organization_of(&current, body.organization_id)?;
    if let Some(site_id) = body.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let prompt = read_prompt(&body.prompt)?;
    // Resolution happens **before** the row is written and before the stream opens, so "no
    // provider is connected" is a `409` with a message the console can render — a draft row
    // that can only say "the model failed" is a worse place to learn that nothing is
    // connected than the form is.
    let resolved = resolve(state.db().pool(), body.model.as_deref())
        .await
        .map_err(provider_error)?;
    let model_id = resolved.id();

    // The title a human typed is a title the model did not choose, so it is the *fallback*:
    // `apply_answer` writes the model's own when the answer carries one.
    let placeholder = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(|title| title.chars().take(ai::MAX_TITLE_LEN).collect::<String>())
        .unwrap_or_else(|| fallback_title(&prompt));

    let draft = ai::insert_draft(
        state.db().pool(),
        ai::NewDraft {
            organization_id,
            site_id: body.site_id,
            title: placeholder,
            prompt: prompt.clone(),
            rationale: None,
            definition: None,
            status: ai::NEW_DRAFT_STATUS.to_owned(),
            model_key: Some(model_id.clone()),
            tokens_input: None,
            tokens_output: None,
            error: None,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(store_error)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.created")
            .organization(organization_id)
            .target("ai_workflow_draft", draft.id.to_string())
            .metadata(json!({ "model": model_id, "site_id": body.site_id }))
            .ip_address(address.as_text()),
    )
    .await?;

    let pool = state.db().pool().clone();
    let draft_id = draft.id;
    let request = ai::GenerationRequest {
        model: Some(model_id.clone()),
        prompt,
        revision: None,
        previous: None,
    };
    let user_id = current.user.id;
    let ip_address = address.as_text();
    let organization_for_event = organization_id;

    let (frames, receiver) = mpsc::channel::<Frame>(STREAM_BUFFER);
    tokio::spawn(async move {
        // `plan` is sent first and is not invented: the generation resolves the model, builds
        // the prompt from the closed registry and then calls the provider. The console shows
        // what the platform is doing, and the two facts it can know are those.
        if frames.send(Frame::Stage("plan".into())).await.is_err() {
            return;
        }

        let outcome = ai::generate(&resolved, &request).await;
        match outcome {
            Ok(answer) => {
                if frames.send(Frame::Stage("validate".into())).await.is_err() {
                    return;
                }
                if answer.repaired {
                    // Sent only when it is true, because a panel that shows "repair?" next to
                    // every generation teaches the operator to ignore it.
                    if frames
                        .send(Frame::Stage("repair".into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                let stored = ai::apply_answer(
                    &pool,
                    draft_id,
                    &answer.title,
                    answer.rationale.as_deref(),
                    &answer.definition,
                    &answer.model_key,
                    Some(clamp_tokens(answer.tokens.input)),
                    Some(clamp_tokens(answer.tokens.output)),
                )
                .await;

                match stored {
                    Ok(Some(row)) => {
                        emit(
                            &pool,
                            NewEvent::new("ai.workflow_draft.generated")
                                .organization(organization_for_event)
                                .actor(user_id)
                                .payload(json!({
                                    "draft_id": row.id,
                                    "title": row.title,
                                    "model_key": row.model_key,
                                    "repaired": answer.repaired,
                                    "attempts": answer.attempts,
                                })),
                        )
                        .await;
                        let _ = frames
                            .send(Frame::Done {
                                draft_id: row.id,
                                status: row.status,
                                repaired: answer.repaired,
                                attempts: answer.attempts,
                                tokens: answer.tokens.total(),
                            })
                            .await;
                    }
                    // A lost race (the row stopped being `generating` while the provider was
                    // answering) is a real outcome, not a bug: somebody re-generated on the
                    // same draft, and this answer is not the one that is stored. The client is
                    // told rather than left waiting for a `done` that will not come.
                    Ok(None) => {
                        let _ = frames
                            .send(Frame::Failed {
                                code: "draft_not_generating",
                                message: "another generation finished first — reload the draft".into(),
                            })
                            .await;
                    }
                    Err(error) => {
                        let _ = frames
                            .send(Frame::Failed {
                                code: "ai_workflow_store_error",
                                message: error.to_string(),
                            })
                            .await;
                    }
                }
            }
            Err(error) => {
                // A provider failure, an answer nobody could produce, or a database refusal:
                // all three land on the row, because the row is the only place the console's
                // error state reads.
                let message = error.to_string();
                let code = if error.is_provider_failure() {
                    "ai_provider_error"
                } else {
                    error.code()
                };
                if let Ok(Some(row)) = ai::apply_failure(&pool, draft_id, &message).await {
                    emit(
                        &pool,
                        NewEvent::new("ai.workflow_draft.failed")
                            .organization(organization_for_event)
                            .actor(user_id)
                            .payload(json!({ "draft_id": row.id, "reason": message })),
                    )
                    .await;
                }
                let _ = frames
                    .send(Frame::Failed {
                        code,
                        message: message.clone(),
                    })
                    .await;
            }
        }

        let entry = NewAuditEntry::by_user(user_id, "ai.workflow_draft.generated")
            .organization(organization_for_event)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "model": model_id }))
            .ip_address(ip_address);
        if let Err(error) = omnion_audit::record(&pool, entry).await {
            tracing::warn!(%error, "the AI workflow draft audit row could not be written");
        }
    });

    Ok(Sse::new(ReceiverStream::new(receiver).map(Frame::event)).keep_alive(KeepAlive::default()))
}

/// `DELETE /api/v1/ai/workflows/drafts/{id}` — remove a draft.
///
/// Never the workflow it produced: the two carry separate permissions, and a delete that
/// switched a running rule off would be a delete with a second, invisible effect.
pub async fn delete_draft(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let draft = draft_in_scope(&state, &current, draft_id).await?;
    if draft.workflow_id.is_some() {
        // Refusing here rather than deleting: the draft is the record of a decision somebody
        // made, and a workflow that is running keeps that record. The message names the
        // workflow so the operator can go and look at it.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "draft_has_workflow",
            format!(
                "this draft already produced workflow {} — delete the workflow itself if you \
                 mean to remove the rule",
                draft.workflow_id.expect("checked above")
            ),
        ));
    }

    let deleted = ai::delete_draft(state.db().pool(), draft_id)
        .await
        .map_err(store_error)?;
    if !deleted {
        return Err(draft_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.deleted")
            .organization(draft.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "title": draft.title, "status": draft.status }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Stream frames
// ---------------------------------------------------------------------------------------------

/// One frame of a generation, before it becomes an SSE event.
enum Frame {
    /// What the platform is doing: `plan`, `validate` or `repair`.
    Stage(String),
    /// The draft is stored and can be opened.
    Done {
        /// The draft the console navigates to.
        draft_id: Uuid,
        /// Its status (`draft`, always — a stored answer is a draft).
        status: String,
        /// Whether the single repair round-trip was spent.
        repaired: bool,
        /// How many provider calls the generation took.
        attempts: usize,
        /// What it cost, in tokens.
        tokens: i64,
    },
    /// Generation stopped. The draft exists and carries the same message.
    Failed {
        /// Stable machine-readable code.
        code: &'static str,
        /// Message for a person.
        message: String,
    },
}

impl Frame {
    /// The SSE event a client reads.
    ///
    /// `data` on every frame rather than the event name alone: the console's reader keys off
    /// `event` but the payload is what carries the draft id, and a frame whose type is the
    /// whole message is a frame a debugger cannot show.
    fn event(self) -> Result<Event, Infallible> {
        let event = match self {
            Self::Stage(stage) => Event::default()
                .event("stage")
                .data(json!({ "stage": stage }).to_string()),
            Self::Done {
                draft_id,
                status,
                repaired,
                attempts,
                tokens,
            } => Event::default()
                .event("done")
                .data(
                    json!({
                        "draft_id": draft_id,
                        "status": status,
                        "repaired": repaired,
                        "attempts": attempts,
                        "tokens": tokens,
                    })
                    .to_string(),
                ),
            Self::Failed { code, message } => Event::default()
                .event("error")
                .data(json!({ "code": code, "message": message }).to_string()),
        };
        Ok(event)
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Organization, for a platform account; a scoped account's own.
#[derive(Debug, Default, Deserialize)]
pub struct OrganizationQuery {
    /// Organization to read.
    pub organization_id: Option<Uuid>,
}

/// The organization this request runs in.
///
/// A platform account (`organization_id IS NULL`) must **name** the tenant, because a draft
/// belongs to one and there is no "all tenants" console; a scoped account's own id is used
/// and a name that disagrees is refused rather than ignored. Ignoring it would let a request
/// say "organization B" and get organization A's drafts — a mismatch nobody can see, because
/// the response looks correct for the session that asked.
fn organization_of(current: &CurrentSession, requested: Option<Uuid>) -> Result<Uuid, ApiError> {
    match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(current, requested)?;
            Ok(own)
        }
        None => requested.ok_or_else(|| {
            ApiError::bad_request(
                "organization_required",
                "this account is not attached to an organization — name one to read its drafts",
            )
        }),
    }
}

/// Load a draft and refuse it when its organization is out of the caller's scope.
async fn draft_in_scope(
    state: &AppState,
    current: &CurrentSession,
    draft_id: Uuid,
) -> Result<ai::AiWorkflowDraft, ApiError> {
    let organization_id = organization_of(current, None)?;
    ai::find_draft_in(state.db().pool(), organization_id, draft_id)
        .await
        .map_err(store_error)?
        .ok_or_else(draft_not_found)
}

/// The 404 a missing and an out-of-scope draft both answer.
///
/// One function because the two must be indistinguishable: a handler that refused an
/// out-of-scope draft with `403` would tell the caller the id exists somewhere.
fn draft_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "ai_workflow_draft_not_found",
        "no such workflow draft",
    )
}

/// Read and bound a prompt, with the console's own message for each failure.
fn read_prompt(raw: &str) -> Result<String, ApiError> {
    let prompt = raw.trim();
    let length = prompt.chars().count();
    if length < MIN_PROMPT_CHARS {
        return Err(ApiError::bad_request(
            "prompt_too_short",
            format!(
                "describe the workflow in at least {MIN_PROMPT_CHARS} characters — a sentence \
                 the model can act on"
            ),
        ));
    }
    if length > MAX_PROMPT_CHARS {
        return Err(ApiError::bad_request(
            "prompt_too_long",
            format!("a prompt is at most {MAX_PROMPT_CHARS} characters; this one is {length}"),
        ));
    }
    Ok(prompt.to_owned())
}

/// A title for the row before the model has answered.
///
/// The prompt's own first clause, not a placeholder string: the row exists during generation
/// and the console's list shows titles, so a draft that reads "New workflow" for twenty seconds
/// and then changes is a list that lies while it loads. Cut at a word boundary so a long
/// sentence does not end mid-word.
fn fallback_title(prompt: &str) -> String {
    const MAX: usize = 60;
    let first = prompt.lines().next().unwrap_or_default().trim();
    let cut: String = first.chars().take(MAX).collect();
    let cut = match first.char_indices().nth(MAX) {
        Some((index, _)) => cut[..index].trim_end().to_owned(),
        None => cut,
    };
    if cut.is_empty() {
        "New workflow".to_owned()
    } else {
        cut
    }
}

/// A token count a `integer` column admits.
fn clamp_tokens(count: i64) -> i32 {
    count.clamp(0, i64::from(i32::MAX)) as i32
}

/// Turn a store failure into a `500` with the module's own code.
pub(crate) fn store_error(error: ai::AiWorkflowError) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        error.code(),
        error.to_string(),
    )
}

/// Turn a routing failure into the status the console's states are written for.
///
/// `409` rather than `503`: nothing is wrong with the platform, the installation has no model
/// this request could use, and the console's remedy is a link to `/ai` — which is what
/// "no provider connected" *is* in the spec. The code says which, so a client can tell a
/// missing provider from a refused key.
fn provider_error(error: omnion_ai_hub::AiHubError) -> ApiError {
    let (status, code) = match error {
        omnion_ai_hub::AiHubError::ProviderNotFound
        | omnion_ai_hub::AiHubError::ModelNotFound
        | omnion_ai_hub::AiHubError::NoDefaultModel
        | omnion_ai_hub::AiHubError::ProviderDisabled(_) => {
            (StatusCode::CONFLICT, "ai_no_model_available")
        }
        _ => (StatusCode::BAD_GATEWAY, "ai_provider_error"),
    };
    ApiError::new(status, code, error.to_string())
}

/// Write an audit row, and let its failure be a `500` like any other write's.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Publish an event, and never let it break the operation that published it.
///
/// The events fan out to the organization's webhook endpoints, and a webhook that is down is
/// not a reason for an approval to answer `500` — the decision is stored either way. The
/// warning is the record.
async fn emit(pool: &sqlx::PgPool, event: NewEvent) {
    if let Err(error) = bus::emit(pool, event).await {
        tracing::warn!(%error, "an AI workflow draft event could not be published");
    }
}
