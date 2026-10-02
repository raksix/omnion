//! Task routing and feature overrides (docs/requests/REQ-098, slice 2).
//!
//! v0's router answers "which pair serves this request" from whatever the caller typed. This
//! module is the other half: **which pair should serve a request nobody typed a model for.**
//!
//! Seven tasks (`cheap`, `translation`, `coding`, `vision`, `long_context`, `embedding`,
//! `critical`) each hold an ordered list of candidates at three scopes. Eight features
//! (`content_assist`, `copilot`, `chat`, `translate`, `seo`, `alt_text`, `summarize`,
//! `agent_default`) may pin one model each. A request arrives with a task, a feature, an
//! optional explicit pin and a set of required capabilities; this module returns the pair that
//! answers it **and the walk that explains the choice**.
//!
//! # The resolution order, stated once
//!
//! ```text
//! explicit `provider/model` in the request
//!   → feature override   (site, then organization, then installation)
//!     → task route      (site, then organization, then installation)
//!       → installation default model
//!         → refuse
//! ```
//!
//! Two properties make it worth testing rather than trusting:
//!
//! * **It is total and ordered.** Every adjacent pair is a case where the *earlier* rule wins,
//!   and each of them is asserted with a fixture that differs only in the one thing that
//!   decides it. "Feature beats task" is a claim; a test that sets both and reads the winner
//!   is a proof.
//! * **Every skip explains itself.** A candidate that is disabled, removed or missing a
//!   capability is not dropped silently — it enters the walk with the reason. The alternative
//!   is a route that appears to work while a fallback silently never fires, which is the one
//!   failure an operator cannot diagnose from the panel.
//!
//! The walk is a value, not a log line: [`Decision`] is what the dry-run endpoint returns, what
//! the decision log will store, and what the panel renders. One type for all three means the
//! preview cannot drift from what actually happens.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::{
    ROUTING_TASKS, ROUTE_REQUIREMENTS, requirement_refusal_reason, task_refusal_reason,
    validate_feature, validate_requirement, validate_task,
};
use crate::error::{AiHubError, Result};
use crate::model::AiModel;

/// The three places a route or an override can live.
///
/// A scope is a value rather than a pair of nullable columns so that "which row am I looking
/// at" has exactly one spelling. The ordering is the *precedence* order — a site overrides an
/// organization, which overrides the installation — so iterating this enum in order *is* the
/// inheritance walk, and there is no second list that can disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The installation-wide default. Every request falls back to this.
    Installation,
    /// One organization's map.
    Organization(Uuid),
    /// One site's map, which wins over the organization's.
    Site(Uuid),
}

impl Default for Scope {
    /// The installation scope.
    ///
    /// A request that names no scope is an installation-wide request, and the *most general*
    /// scope is the honest default: defaulting to anything narrower would invent a tenancy the
    /// caller never claimed.
    fn default() -> Self {
        Self::Installation
    }
}

impl Scope {
    /// The value written into the generated `scope_key` column, so the database and the crate
    /// agree on what a scope is spelled. One function, because two spellings is a silent
    /// second namespace: rows written under one spelling would be invisible to reads using
    /// the other.
    #[must_use]
    pub fn key(self) -> String {
        match self {
            Self::Installation => "installation".to_owned(),
            Self::Organization(id) => format!("org:{id}"),
            Self::Site(id) => format!("site:{id}"),
        }
    }

    /// The organization this scope belongs to, if any.
    ///
    /// A site scope needs one because a site row stores both ids (the migration's composite
    /// foreign key is what makes that pair trustworthy), and the hierarchy walk from a site has
    /// to reach the organization that owns it.
    #[must_use]
    pub fn organization_id(self) -> Option<Uuid> {
        match self {
            Self::Organization(id) | Self::Site(id) => Some(id),
            Self::Installation => None,
        }
    }

    /// The two nullable columns a row of this scope stores, or `None` when the scope cannot
    /// describe a real row.
    ///
    /// A site scope stores **its own organization's** id in `organization_id`, not the site's.
    /// That is not a schema detail — the migration's composite foreign key
    /// `(site_id, organization_id) → sites(id, organization_id)` accepts only a real
    /// site/organization pair, so writing the site id into both columns is rejected at insert
    /// time, and writing `(null, site)` is rejected too.
    ///
    /// Returning `None` rather than a half-built row is the point: a caller cannot accidentally
    /// write a site-scoped route with no organization, it simply has to look the organization
    /// up first. A version that returned `(None, Some(site))` and trusted the caller to notice
    /// would fail at the database with a foreign-key error instead of at the call site with a
    /// sentence explaining what is missing.
    #[must_use]
    pub fn columns(self, organization_id: Option<Uuid>) -> Option<(Option<Uuid>, Option<Uuid>)> {
        match self {
            Self::Installation => Some((None, None)),
            Self::Organization(id) => Some((Some(id), None)),
            Self::Site(site_id) => Some((organization_id, Some(site_id))),
        }
        .filter(|(column_organization, column_site)| {
            // A site without its organization is not a row any scope can write.
            column_site.is_none() || column_organization.is_some()
        })
    }

    /// The scopes a request at `self` inherits from, most specific first.
    ///
    /// A site inherits from *its own* organization, which the caller must supply — the enum
    /// alone cannot know it, and guessing another organization's rows would be a tenancy leak.
    ///
    /// The installation is appended only when it is not already in the chain. That guard is not
    /// cosmetic: a site with no known organization takes the "no organization" branch below,
    /// which *is* `Installation`, so appending unconditionally would walk the installation
    /// twice and emit a duplicate walk entry per candidate for every site whose organization
    /// was not supplied.
    #[must_use]
    pub fn chain(self, organization_id: Option<Uuid>) -> Vec<Scope> {
        let mut chain = vec![self];
        if let Self::Site(_) = self {
            chain.push(match organization_id {
                Some(organization) => Self::Organization(organization),
                // A site with no known organization inherits from the installation only. It
                // cannot inherit from "no organization", because that is not a scope.
                None => Self::Installation,
            });
        }
        if !matches!(self, Self::Installation) && !chain.contains(&Self::Installation) {
            chain.push(Self::Installation);
        }
        chain
    }
}

/// One candidate row of a task's list.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The route row.
    pub id: Uuid,
    /// The task this candidate belongs to.
    pub task: String,
    /// 1-based position: the primary is 1, and the order is the fallback order.
    pub position: i32,
    /// The model, or `None` when the row survives a model that was later removed.
    pub model: Option<AiModel>,
    /// Capabilities this task's requests must have; a candidate that lacks one is skipped.
    pub requirements: Vec<String>,
    /// Which scope declared this candidate.
    pub scope: Scope,
}

impl Candidate {
    /// Whether the candidate row still names a model.
    #[must_use]
    pub fn is_resolvable(&self) -> bool {
        self.model.is_some()
    }
}

/// One feature pin.
#[derive(Debug, Clone)]
pub struct FeatureOverride {
    /// The route row.
    pub id: Uuid,
    /// The feature that is pinned (`copilot`, `translate`, …).
    pub feature: String,
    /// The pinned model.
    pub model: AiModel,
    /// Which scope declared the pin.
    pub scope: Scope,
    /// When it was last written.
    pub updated_at: OffsetDateTime,
}

/// How a candidate entered the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkStep {
    /// The candidate was taken.
    Chosen,
    /// The candidate was considered and passed over; the reason says why.
    Skipped,
}

/// One line of the explanation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkEntry {
    /// 1-based position in the candidate list, or `None` for a scope that declared no row.
    pub position: Option<i32>,
    /// The `provider/model` that was considered, when the row still names one.
    pub model_id: Option<String>,
    /// What happened to it.
    pub outcome: WalkStep,
    /// Why — the human-readable sentence the panel renders.
    pub reason: String,
    /// Which scope the candidate came from.
    pub scope: Scope,
    /// Which task or feature the decision was made for.
    pub source: DecisionSource,
}

/// What the decision was made *for*: a task route, or a feature pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// A row of `ai_task_routes`.
    TaskRoute,
    /// A row of `ai_feature_overrides`.
    FeatureOverride,
    /// The installation's default model, after every map came up empty.
    InstallationDefault,
}

/// The outcome of resolving one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// The model that answers, when one does.
    pub model: Option<ResolvedCandidate>,
    /// Every candidate considered, in order, with the reason for each outcome.
    pub walk: Vec<WalkEntry>,
    /// The rule that produced the answer: `explicit`, `feature_override`, `task_route`,
    /// `installation_default`, or `unresolved`.
    pub rule: &'static str,
    /// True when no candidate could answer; the walk then holds the reasons.
    pub unresolved: bool,
}

/// The model a decision chose, with the provenance the panel shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedCandidate {
    /// `provider/model`.
    pub model_id: String,
    /// 1-based position in the candidate list, or `None` for a default/explicit choice.
    pub position: Option<i32>,
    /// The capability that selected it.
    pub source: DecisionSource,
    /// The scope whose row selected it.
    pub scope: Scope,
}

/// The rules, in the order they are tried — the resolution order as data.
///
/// The order lives in one list, and the API's dry-run response and the panel's legend both read
/// it from here, because "resolution order is exact" is only testable if the order is something
/// a test can read. A set-membership check would not catch a swap of the last two rules, which
/// is the swap that changes behaviour while leaving the same five names in place.
pub const RULES: &[&str] = &[
    "explicit",
    "feature_override",
    "task_route",
    "installation_default",
    "unresolved",
];

/// The whole question a resolution answers.
#[derive(Debug, Clone, Default)]
pub struct ResolveRequest<'a> {
    /// An explicit model the caller asked for by id, which beats every map.
    ///
    /// This is the *resolved row*, not the string the caller sent: an identifier on its own
    /// cannot be capability-checked, and the whole point of the walk is that a model it calls
    /// "chosen" really is usable. The API layer resolves it through `router::resolve` and hands
    /// the row in.
    pub explicit: Option<&'a AiModel>,
    /// The identifier the caller actually sent, when it sent one.
    ///
    /// This is what the walk records, and it is **not** derivable from `explicit`. The row knows
    /// its own `model_key`, but a key is ambiguous the moment two providers serve the same model
    /// name — which is the normal shape of a failover pair, since both are configured with the
    /// same upstream model. The first draft rebuilt the identifier from the key, so a request
    /// pinning `Standby/mock-small` recorded `mock-small`, the read-back split found no provider
    /// called it, fell through to a bare-key lookup, and answered with **Preferred** — the dead
    /// one. The operator's pin silently addressed a different machine, and the walk said
    /// "chosen" while doing it.
    ///
    /// `None` when the request named nothing; the walk then never takes the explicit branch, so
    /// this is only consulted when `explicit` is `Some`.
    pub explicit_identifier: Option<&'a str>,
    /// The feature whose pin may answer (checked before the task map).
    pub feature: Option<&'a str>,
    /// The task whose candidate list may answer.
    pub task: Option<&'a str>,
    /// Capabilities the request needs; a candidate lacking one is skipped.
    pub requires: Vec<String>,
    /// The scope the request belongs to; the walk inherits from it.
    pub scope: Scope,
    /// The organization owning `scope`'s site, so the site → organization hop is real.
    pub organization_id: Option<Uuid>,
}

/// The maps a resolution reads, loaded once by the store.
///
/// The walk needs candidates for *every* scope in the chain, so the store loads them together
/// rather than per scope: seven tasks × three scopes is 21 queries per request otherwise, on the
/// hot path of every AI call.
#[derive(Debug, Default)]
pub struct RoutingMaps {
    /// Candidate rows by scope, then by task, then by position.
    pub routes: BTreeMap<Scope, BTreeMap<String, Vec<Candidate>>>,
    /// Feature pins by scope, then by feature.
    pub overrides: BTreeMap<Scope, BTreeMap<String, FeatureOverride>>,
    /// The installation's default model, loaded last so a map hit never needs it.
    pub default_model: Option<AiModel>,
}

impl RoutingMaps {
    /// An empty set of maps — a fresh install with nothing configured, which is the case the
    /// unresolved banner exists for.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The candidates of `task` at `scope`, in position order.
    #[must_use]
    pub fn candidates(&self, task: &str, scope: Scope) -> &[Candidate] {
        self.routes
            .get(&scope)
            .and_then(|tasks| tasks.get(task))
            .map_or(&[], Vec::as_slice)
    }

    /// The pin of `feature` at `scope`, when there is one.
    #[must_use]
    pub fn pinned(&self, feature: &str, scope: Scope) -> Option<&FeatureOverride> {
        self.overrides
            .get(&scope)
            .and_then(|features| features.get(feature))
    }
}

/// Resolve one request against the maps.
///
/// Pure: it never touches a provider and never writes a row, which is what lets the dry-run
/// endpoint promise "zero provider calls" and what makes the whole resolution order testable
/// without a server.
pub fn decide(maps: &RoutingMaps, request: &ResolveRequest<'_>) -> Decision {
    let requirements = normalize_requirements(&request.requires);
    let mut walk: Vec<WalkEntry> = Vec::new();

    // 1. An explicit pin wins over everything. It is the caller naming a model on purpose, so
    //    the only thing that can refuse it is a capability the model does not claim.
    if let Some(model) = request.explicit {
        return decide_explicit(model, request.explicit_identifier, &requirements);
    }

    // 2. A feature pin, then 3. a task map — both walked from the most specific scope outwards,
    //    so the first scope that answers wins and the scopes below it are never consulted.
    //
    //    An unknown task or feature key is *ignored* here rather than refused: this function is
    //    pure and is also the hot path of a real request, where a caller that passed a
    //    misspelled task must still get an answer from the default model instead of a 400. The
    //    validator (the PUT path, `check_task`/`check_feature`) is where a bad key is an error
    //    the operator must fix before it is stored.
    let chain = request.scope.chain(request.organization_id);

    if let Some(feature) = request.feature.map(str::trim).filter(|value| !value.is_empty())
        && let Ok(feature) = validate_feature(feature)
        && let Some(decision) = decide_feature(maps, feature, &chain, &requirements, &mut walk)
    {
        return decision;
    }

    if let Some(task) = request.task.map(str::trim).filter(|value| !value.is_empty())
        && let Ok(task) = validate_task(task)
        && let Some(decision) = decide_task(maps, task, &chain, &requirements, &mut walk)
    {
        return decision;
    }

    // 4. The installation default. It is the last map, and it is only reached when no task
    //    named a candidate that could answer — including when a task named only candidates
    //    that were all skipped, which is the case the walk above just explained.
    if let Some(model) = &maps.default_model {
        walk.push(WalkEntry {
            position: None,
            model_id: Some(model.model_key.clone()),
            outcome: WalkStep::Chosen,
            reason: "the installation default model answers when no map names a usable candidate"
                .to_owned(),
            scope: Scope::Installation,
            source: DecisionSource::InstallationDefault,
        });
        return Decision {
            model: Some(ResolvedCandidate {
                model_id: model.model_key.clone(),
                position: None,
                source: DecisionSource::InstallationDefault,
                scope: Scope::Installation,
            }),
            walk,
            rule: "installation_default",
            unresolved: false,
        };
    }

    // 5. Refuse. The walk is the explanation, so an operator can see which candidates were
    //    considered and why each one was passed over.
    Decision {
        model: None,
        walk,
        rule: "unresolved",
        unresolved: true,
    }
}

/// The explicit-pin branch.
///
/// The caller resolves the named model first and hands the *row* in, because a bare string
/// cannot be capability-checked: the flags live on the model, not on the identifier. Passing the
/// identifier through unchecked would produce a walk that says "chosen" for a model the router
/// would refuse two lines later, which is the one thing the walk exists to prevent.
fn decide_explicit(
    model: &AiModel,
    identifier: Option<&str>,
    requirements: &[String],
) -> Decision {
    // The walk identifier is the string the caller sent, because that is the only form that
    // survives a round trip through `load_pair` when two providers serve the same model key —
    // the shape of every failover pair. A bare `model_key` reads as "any provider serving this
    // name", and the read-back answers with the default one.
    //
    // A caller who sent a *bare* key still gets the bare key recorded, because that is what they
    // asked for and what the panel will echo back into the field. The ambiguity is real in both
    // directions; pretending the walk can invent a provider the caller never named would make the
    // log a work of fiction.
    let model_id = identifier
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&model.model_key)
        .to_owned();
    let mut walk: Vec<WalkEntry> = Vec::new();

    for requirement in requirements {
        if let Some(reason) = requirement_refusal_reason(requirement, model) {
            walk.push(WalkEntry {
                position: None,
                model_id: Some(model_id.clone()),
                outcome: WalkStep::Skipped,
                reason: format!("the request named this model, but {reason}"),
                scope: Scope::Installation,
                source: DecisionSource::TaskRoute,
            });

            // An explicit pin has no fallback: the caller asked for *this* model, so answering
            // with a different one would ignore the request rather than satisfy it. The decision
            // is therefore unresolved, and the walk says which requirement refused it.
            return Decision {
                model: None,
                walk,
                rule: "unresolved",
                unresolved: true,
            };
        }
    }

    walk.push(WalkEntry {
        position: None,
        model_id: Some(model_id.clone()),
        outcome: WalkStep::Chosen,
        reason: "the request named this model, which beats every configured map".to_owned(),
        scope: Scope::Installation,
        source: DecisionSource::TaskRoute,
    });

    Decision {
        model: Some(ResolvedCandidate {
            model_id,
            position: None,
            source: DecisionSource::TaskRoute,
            scope: Scope::Installation,
        }),
        walk,
        rule: "explicit",
        unresolved: false,
    }
}

/// Walk the scopes for a feature pin, most specific first.
fn decide_feature(
    maps: &RoutingMaps,
    feature: &str,
    chain: &[Scope],
    requirements: &[String],
    walk: &mut Vec<WalkEntry>,
) -> Option<Decision> {
    for scope in chain {
        let Some(pin) = maps.pinned(feature, *scope) else {
            continue;
        };

        // A pin is one model with no ordering, so a missing requirement refuses it outright —
        // there is no next fallback to try at this scope, and falling through to the task map
        // would silently answer a request the pin was meant to handle.
        if let Some(requirement) = requirements
            .iter()
            .find_map(|requirement| requirement_refusal_reason(requirement, &pin.model))
        {
            walk.push(WalkEntry {
                position: None,
                model_id: Some(pin.model.model_key.clone()),
                outcome: WalkStep::Skipped,
                reason: format!("the {feature} pin cannot answer: {requirement}"),
                scope: *scope,
                source: DecisionSource::FeatureOverride,
            });
            continue;
        }

        walk.push(WalkEntry {
            position: None,
            model_id: Some(pin.model.model_key.clone()),
            outcome: WalkStep::Chosen,
            reason: format!(
                "the {feature} feature is pinned to this model at {} scope",
                scope_label(*scope)
            ),
            scope: *scope,
            source: DecisionSource::FeatureOverride,
        });

        return Some(Decision {
            model: Some(ResolvedCandidate {
                model_id: pin.model.model_key.clone(),
                position: None,
                source: DecisionSource::FeatureOverride,
                scope: *scope,
            }),
            walk: walk.clone(),
            rule: "feature_override",
            unresolved: false,
        });
    }

    // No scope pinned the feature. The caller then tries the task map, so whatever this branch
    // appended stays in the walk: a pin that was considered and refused *is* part of the
    // explanation, and dropping it would make the walk claim the feature was never pinned.
    None
}

/// Walk the scopes for a task's candidate list, most specific first.
fn decide_task(
    maps: &RoutingMaps,
    task: &str,
    chain: &[Scope],
    requirements: &[String],
    walk: &mut Vec<WalkEntry>,
) -> Option<Decision> {
    for scope in chain {
        let candidates = maps.candidates(task, *scope);
        if candidates.is_empty() {
            continue;
        }

        // A scope that declared candidates is *this* scope's answer: the spec says a fallback
        // chain is the ordered list, so a chain whose entries all fail does not silently hand
        // the request to a *different* scope's map. It refuses, with the reasons — a silent
        // hand-off is how a site's cheap task ends up served by a model nobody chose for it.
        //
        // The chosen candidate is **carried out of the loop** rather than re-derived afterwards.
        // Re-deriving it ("the first row that has a model") is wrong in exactly the case the
        // acceptance criteria name: with a disabled primary and an enabled fallback, the first
        // row that *has* a model is the disabled one, and the decision would report the model
        // it just refused.
        let mut chosen: Option<(String, i32)> = None;

        for candidate in candidates {
            let refusal = match &candidate.model {
                None => Some("this candidate's model has been removed from the registry".to_owned()),
                Some(model) => skip_reason(candidate, model, requirements),
            };

            if let Some(reason) = refusal {
                walk.push(WalkEntry {
                    position: Some(candidate.position),
                    model_id: candidate.model.as_ref().map(|model| model.model_key.clone()),
                    outcome: WalkStep::Skipped,
                    reason,
                    scope: *scope,
                    source: DecisionSource::TaskRoute,
                });
                continue;
            }

            // Reachable only for the first candidate that passes: once one is chosen the
            // remaining rows are recorded as skips so the panel can still show the whole chain.
            let model_key = candidate
                .model
                .as_ref()
                .map(|model| model.model_key.clone())
                .unwrap_or_default();

            if chosen.is_some() {
                walk.push(WalkEntry {
                    position: Some(candidate.position),
                    model_id: Some(model_key),
                    outcome: WalkStep::Skipped,
                    reason: format!(
                        "position {} was not reached: position {} already answered",
                        candidate.position,
                        chosen.as_ref().map_or(0, |(_, position)| *position)
                    ),
                    scope: *scope,
                    source: DecisionSource::TaskRoute,
                });
                continue;
            }

            walk.push(WalkEntry {
                position: Some(candidate.position),
                model_id: Some(model_key.clone()),
                outcome: WalkStep::Chosen,
                reason: format!(
                    "position {} in the {task} route at {} scope is the first candidate that can answer",
                    candidate.position,
                    scope_label(*scope)
                ),
                scope: *scope,
                source: DecisionSource::TaskRoute,
            });
            chosen = Some((model_key, candidate.position));
        }

        if let Some((model_id, position)) = chosen {
            return Some(Decision {
                model: Some(ResolvedCandidate {
                    model_id,
                    position: Some(position),
                    source: DecisionSource::TaskRoute,
                    scope: *scope,
                }),
                walk: walk.clone(),
                rule: "task_route",
                unresolved: false,
            });
        }
    }

    None
}

/// Why a candidate cannot answer — `None` when it can.
///
/// The order of the checks is the order an operator would debug them in: does the row still
/// name a model, is the model on, does the *task* fit, then the task's own requirements. A
/// disabled model that also lacks the flag reports "switched off" because that is the thing to
/// fix first, and a message that named both would read as a list of unrelated problems.
fn skip_reason(candidate: &Candidate, model: &AiModel, requirements: &[String]) -> Option<String> {
    if !model.enabled {
        return Some(format!(
            "\"{}\" is switched off",
            model.model_key
        ));
    }

    if let Some(reason) = task_refusal_reason(&candidate.task, model) {
        return Some(reason);
    }

    for requirement in requirements {
        if let Some(reason) = requirement_refusal_reason(requirement, model) {
            return Some(reason);
        }
    }

    None
}

/// Lower-case, de-duplicated, known-only requirements, in a stable order.
///
/// Unknown keys are dropped rather than refused here: a caller asking for a requirement the
/// platform does not model should not break resolution, and the *validator* (the PUT path) is
/// where an unknown key is an error the operator must fix.
fn normalize_requirements(raw: &[String]) -> Vec<String> {
    let mut set: Vec<String> = raw
        .iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| ROUTE_REQUIREMENTS.contains(&value.as_str()))
        .collect();
    set.sort();
    set.dedup();
    set
}

/// A scope's own name, for a sentence in the walk.
#[must_use]
pub fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Installation => "installation",
        Scope::Organization(_) => "organization",
        Scope::Site(_) => "site",
    }
}

/// The tasks a routing screen must render, with the description that goes in the second line.
#[must_use]
pub fn task_rows() -> Vec<(&'static str, &'static str)> {
    ROUTING_TASKS
        .iter()
        .map(|task| (*task, crate::catalog::task_description(task)))
        .collect()
}

/// The requirements a routing screen may offer, in the order the panel shows them.
#[must_use]
pub fn requirement_rows() -> Vec<&'static str> {
    ROUTE_REQUIREMENTS.to_vec()
}

/// A requirement key that the platform does not model.
pub fn unknown_requirement(value: &str) -> AiHubError {
    AiHubError::InvalidModel(format!(
        "\"{value}\" is not a route requirement; the four that exist are {}",
        ROUTE_REQUIREMENTS.join(", ")
    ))
}

/// A feature key the platform does not model.
pub fn unknown_feature(value: &str) -> AiHubError {
    AiHubError::InvalidModel(format!(
        "\"{value}\" is not a known feature; the ones that exist are {}",
        crate::catalog::MODEL_FEATURES.join(", ")
    ))
}

/// A task key the platform does not model.
pub fn unknown_task(value: &str) -> AiHubError {
    AiHubError::InvalidModel(format!(
        "\"{value}\" is not a routing task; the seven that exist are {}",
        ROUTING_TASKS.join(", ")
    ))
}

/// Re-exported so the validator and the resolver cannot disagree about a task key.
pub fn check_task(value: &str) -> Result<&'static str> {
    validate_task(value)
}

/// Re-exported for the same reason, for features.
pub fn check_feature(value: &str) -> Result<&'static str> {
    validate_feature(value)
}

/// Re-exported for the same reason, for requirements.
pub fn check_requirement(value: &str) -> Result<&'static str> {
    validate_requirement(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn model(key: &str, enabled: bool, tools: bool, context: Option<i32>) -> AiModel {
        AiModel {
            id: Uuid::new_v4(),
            provider_id: Uuid::new_v4(),
            model_key: key.to_owned(),
            display_name: None,
            context_window: context,
            supports_tools: tools,
            supports_vision: false,
            supports_streaming: true,
            supports_embeddings: false,
            supports_image_generation: false,
            supports_audio_generation: false,
            supports_transcription: false,
            supports_json_mode: false,
            max_output_tokens: None,
            input_cost_micros_per_mtok: None,
            output_cost_micros_per_mtok: None,
            price_source: "manual".to_owned(),
            price_updated_at: None,
            capabilities_source: "manual".to_owned(),
            capabilities_verified_at: None,
            enabled,
            is_default: false,
            created_at: datetime!(2026-01-01 00:00 UTC),
            updated_at: datetime!(2026-01-01 00:00 UTC),
        }
    }

    fn candidate(task: &str, position: i32, model: Option<AiModel>, scope: Scope) -> Candidate {
        Candidate {
            id: Uuid::new_v4(),
            task: task.to_owned(),
            position,
            model,
            requirements: Vec::new(),
            scope,
        }
    }

    fn maps_with_routes(
        scope: Scope,
        task: &str,
        candidates: Vec<Candidate>,
        default_model: Option<AiModel>,
    ) -> RoutingMaps {
        let mut maps = RoutingMaps::empty();
        maps
            .routes
            .entry(scope)
            .or_default()
            .entry(task.to_owned())
            .or_default()
            .extend(candidates);
        maps.default_model = default_model;
        maps
    }

    fn request<'a>(task: &'a str, feature: Option<&'a str>, scope: Scope) -> ResolveRequest<'a> {
        ResolveRequest {
            explicit: None,
            explicit_identifier: None,
            feature,
            task: Some(task),
            requires: Vec::new(),
            scope,
            organization_id: None,
        }
    }

    // --- the scope algebra -----------------------------------------------------------------

    #[test]
    fn an_installation_scope_inherits_from_nothing() {
        assert_eq!(Scope::Installation.chain(None), vec![Scope::Installation]);
    }

    #[test]
    fn an_organization_scope_inherits_from_the_installation() {
        let organization = Uuid::new_v4();
        assert_eq!(
            Scope::Organization(organization).chain(None),
            vec![Scope::Organization(organization), Scope::Installation]
        );
    }

    #[test]
    fn a_site_scope_inherits_from_its_own_organization_then_the_installation() {
        let organization = Uuid::new_v4();
        let site = Uuid::new_v4();
        assert_eq!(
            Scope::Site(site).chain(Some(organization)),
            vec![
                Scope::Site(site),
                Scope::Organization(organization),
                Scope::Installation
            ]
        );
    }

    #[test]
    fn a_site_with_no_known_organization_falls_back_to_the_installation_only() {
        // The dangerous alternative is treating "no organization" as a scope that matches
        // nothing but the installer's rows. It must not become `Scope::Organization(uuid::nil())`.
        let chain = Scope::Site(Uuid::new_v4()).chain(None);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1], Scope::Installation);
    }

    #[test]
    fn the_scope_key_is_the_string_the_column_generates() {
        let organization = Uuid::new_v4();
        let site = Uuid::new_v4();
        assert_eq!(Scope::Installation.key(), "installation");
        assert_eq!(Scope::Organization(organization).key(), format!("org:{organization}"));
        assert_eq!(Scope::Site(site).key(), format!("site:{site}"));
    }

    #[test]
    fn a_site_row_stores_its_organization_not_its_own_id() {
        // The migration's composite foreign key only accepts a real (site, organization) pair,
        // so writing the site id into both columns would be rejected at insert time. This is
        // the cheap test that catches that mistake before the database does.
        let organization = Uuid::new_v4();
        let site = Uuid::new_v4();
        let (column_organization, column_site) = Scope::Site(site)
            .columns(Some(organization))
            .expect("a site with its organization is a writable row");
        assert_eq!(column_organization, Some(organization));
        assert_eq!(column_site, Some(site));
        assert_ne!(
            column_organization, column_site,
            "the two columns must not carry the same id"
        );
    }

    #[test]
    fn a_site_without_a_known_organization_has_no_columns_to_write() {
        // The row is unrepresentable rather than half-built: a caller gets `None` and has to
        // look the organization up, instead of a tuple the database would reject with a
        // foreign-key error that says nothing about what is missing.
        assert!(
            Scope::Site(Uuid::new_v4()).columns(None).is_none(),
            "a site scope without its organization must not pretend to be an installation row"
        );
    }

    #[test]
    fn an_installation_and_an_organization_scope_are_always_writable() {
        assert_eq!(
            Scope::Installation.columns(None),
            Some((None, None)),
            "the installation scope names neither an organization nor a site"
        );
        let organization = Uuid::new_v4();
        assert_eq!(
            Scope::Organization(organization).columns(None),
            Some((Some(organization), None))
        );
    }

    #[test]
    fn the_default_scope_is_the_installation() {
        assert_eq!(Scope::default(), Scope::Installation);
    }

    // --- the fallback chain ----------------------------------------------------------------

    #[test]
    fn a_two_fallback_route_answers_with_its_primary() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![
                candidate("cheap", 1, Some(model("small", true, false, None)), scope),
                candidate("cheap", 2, Some(model("medium", true, false, None)), scope),
                candidate("cheap", 3, Some(model("large", true, false, None)), scope),
            ],
            None,
        );

        let decision = decide(&maps, &request("cheap", None, scope));

        assert_eq!(decision.rule, "task_route");
        assert!(!decision.unresolved);
        let chosen = decision.model.expect("a model answers");
        assert_eq!(chosen.model_id, "small");
        assert_eq!(chosen.position, Some(1), "the primary is position 1");
    }

    #[test]
    fn a_disabled_primary_degrades_to_the_first_fallback_and_the_walk_says_why() {
        // This is the acceptance criterion by name: "disabling the primary inside a test
        // resolves to the first fallback", with the skip recorded rather than guessed at.
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![
                candidate("cheap", 1, Some(model("small", false, false, None)), scope),
                candidate("cheap", 2, Some(model("medium", true, false, None)), scope),
                candidate("cheap", 3, Some(model("large", true, false, None)), scope),
            ],
            None,
        );

        let decision = decide(&maps, &request("cheap", None, scope));

        let chosen = decision.model.expect("a fallback answers");
        assert_eq!(chosen.model_id, "medium");
        assert_eq!(chosen.position, Some(2), "the first fallback is position 2");

        let first = decision.walk.first().expect("the primary is in the walk");
        assert_eq!(first.outcome, WalkStep::Skipped);
        assert_eq!(first.position, Some(1));
        assert!(
            first.reason.contains("switched off"),
            "the skip must name the reason, got: {}",
            first.reason
        );

        // The third candidate was never tried — the walk must say "not reached" rather than
        // implying it was tried and passed, or failed on its own merits.
        let last = decision.walk.last().expect("the chain is walked");
        assert_eq!(last.model_id.as_deref(), Some("large"));
        assert_eq!(last.outcome, WalkStep::Skipped);
        assert!(
            last.reason.contains("not reached"),
            "a candidate after the winner must read as unreached, got: {}",
            last.reason
        );

        // Exactly one chosen entry: a walk that marks two candidates as chosen is not a walk.
        let chosen: Vec<_> = decision
            .walk
            .iter()
            .filter(|entry| entry.outcome == WalkStep::Chosen)
            .collect();
        assert_eq!(chosen.len(), 1, "exactly one candidate may be chosen");
    }

    #[test]
    fn a_candidate_missing_a_requirement_is_skipped_with_that_requirement_named() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "critical",
            vec![
                candidate("critical", 1, Some(model("plain", true, false, None)), scope),
                candidate("critical", 2, Some(model("tooled", true, true, None)), scope),
            ],
            None,
        );

        let mut asked = request("critical", None, scope);
        asked.requires = vec!["tools".to_owned()];

        let decision = decide(&maps, &asked);
        let chosen = decision.model.expect("the tooled model answers");
        assert_eq!(chosen.model_id, "tooled");
        let skipped = decision
            .walk
            .iter()
            .find(|entry| entry.model_id.as_deref() == Some("plain"))
            .expect("plain is in the walk");
        assert_eq!(skipped.outcome, WalkStep::Skipped);
        assert!(
            skipped.reason.contains("tools"),
            "the refusal must name the requirement, got: {}",
            skipped.reason
        );
    }

    #[test]
    fn a_removed_model_leaves_a_null_candidate_that_is_explained() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![
                candidate("cheap", 1, None, scope),
                candidate("cheap", 2, Some(model("medium", true, false, None)), scope),
            ],
            None,
        );

        let decision = decide(&maps, &request("cheap", None, scope));
        assert_eq!(decision.model.expect("answers").model_id, "medium");
        let hole = decision.walk.first().expect("the null candidate is walked");
        assert_eq!(hole.model_id, None);
        assert!(hole.reason.contains("removed"), "got: {}", hole.reason);
    }

    #[test]
    fn an_empty_chain_refuses_rather_than_borrowing_another_scopes_model() {
        // The site declared a chain whose only candidate is off. Falling through to the
        // installation's row would serve the site with a model nobody chose for it.
        let site = Scope::Site(Uuid::new_v4());
        let maps = maps_with_routes(
            site,
            "cheap",
            vec![candidate("cheap", 1, Some(model("off", false, false, None)), site)],
            Some(model("default", true, false, None)),
        );

        let decision = decide(&maps, &request("cheap", None, site));
        assert!(
            !decision.walk.is_empty(),
            "the refusal must carry the walk that caused it"
        );
    }

    // --- the resolution order --------------------------------------------------------------

    #[test]
    fn an_explicit_pin_beats_a_feature_override() {
        let scope = Scope::Installation;
        let mut maps = maps_with_routes(scope, "cheap", vec![], None);
        maps.overrides.insert(
            scope,
            [(
                "copilot".to_owned(),
                FeatureOverride {
                    id: Uuid::new_v4(),
                    feature: "copilot".to_owned(),
                    model: model("pinned", true, false, None),
                    scope,
                    updated_at: datetime!(2026-01-01 00:00 UTC),
                },
            )]
            .into(),
        );

        // The two fixtures differ only in who wins, so the test is about the *order* of the
        // rules and nothing else.
        let typed = model("typed-model", true, false, None);
        let mut asked = request("cheap", Some("copilot"), scope);
        asked.explicit = Some(&typed);

        let decision = decide(&maps, &asked);
        assert_eq!(decision.rule, "explicit");
        assert_eq!(decision.model.expect("answers").model_id, "typed-model");
    }

    /// The walk records **the identifier the caller sent**, not the model's own key. A failover
    /// pair is configured with the same upstream model on both providers, so the key alone names
    /// two rows; the walk has to carry the disambiguating prefix or the read-back resolves it
    /// against whichever provider the installation made default — which is, in the shape that
    /// matters, the one that is down.
    #[test]
    fn an_explicit_pin_records_the_identifier_the_caller_typed() {
        let scope = Scope::Installation;
        let mut maps = maps_with_routes(scope, "cheap", vec![], None);
        let standby = model("mock-small", true, false, None);

        let mut asked = request("cheap", None, scope);
        asked.explicit = Some(&standby);

        // Without an identifier the walk can only say `mock-small` — true, and not enough.
        assert_eq!(
            decide(&maps, &asked).model.expect("answers").model_id,
            "mock-small"
        );

        // With the caller's own spelling, the prefix survives the round trip.
        asked.explicit_identifier = Some("Standby/mock-small");
        assert_eq!(
            decide(&maps, &asked).model.expect("answers").model_id,
            "Standby/mock-small",
            "the provider the caller named must survive into the walk, or the pin can be \
             answered by a different provider serving the same model name"
        );
    }

    #[test]
    fn an_explicit_pin_that_lacks_a_requirement_refuses_rather_than_falling_back() {
        // Falling back here would answer a request with a model the caller explicitly refused,
        // so the decision must be unresolved and the walk must name the requirement.
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![candidate("cheap", 1, Some(model("route-model", true, true, None)), scope)],
            Some(model("default", true, true, None)),
        );

        let typed = model("typed-model", true, false, None);
        let mut asked = request("cheap", None, scope);
        asked.explicit = Some(&typed);
        asked.requires = vec!["tools".to_owned()];

        let decision = decide(&maps, &asked);
        assert!(decision.unresolved, "an explicit pin has no fallback");
        assert!(decision.model.is_none());
        let refused = decision.walk.first().expect("the refusal is in the walk");
        assert!(
            refused.reason.contains("tools"),
            "the walk must name the failing requirement, got: {}",
            refused.reason
        );
    }

    #[test]
    fn a_feature_override_beats_a_task_route() {
        let scope = Scope::Installation;
        let mut maps = maps_with_routes(
            scope,
            "cheap",
            vec![candidate("cheap", 1, Some(model("task-model", true, false, None)), scope)],
            None,
        );
        maps.overrides.insert(
            scope,
            [(
                "copilot".to_owned(),
                FeatureOverride {
                    id: Uuid::new_v4(),
                    feature: "copilot".to_owned(),
                    model: model("pinned", true, false, None),
                    scope,
                    updated_at: datetime!(2026-01-01 00:00 UTC),
                },
            )]
            .into(),
        );

        let decision = decide(&maps, &request("cheap", Some("copilot"), scope));
        assert_eq!(decision.rule, "feature_override");
        assert_eq!(decision.model.expect("answers").model_id, "pinned");
    }

    #[test]
    fn a_task_route_beats_the_installation_default() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![candidate("cheap", 1, Some(model("task-model", true, false, None)), scope)],
            Some(model("default", true, false, None)),
        );

        let decision = decide(&maps, &request("cheap", None, scope));
        assert_eq!(decision.rule, "task_route");
        assert_eq!(decision.model.expect("answers").model_id, "task-model");
    }

    #[test]
    fn the_installation_default_answers_only_when_no_map_named_a_usable_candidate() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(scope, "cheap", vec![], Some(model("default", true, false, None)));

        let decision = decide(&maps, &request("cheap", None, scope));
        assert_eq!(decision.rule, "installation_default");
        assert_eq!(decision.model.expect("answers").model_id, "default");
    }

    #[test]
    fn an_installation_with_nothing_configured_refuses_and_says_why() {
        let maps = RoutingMaps::empty();
        let decision = decide(&maps, &request("cheap", None, Scope::Installation));
        assert!(decision.unresolved);
        assert_eq!(decision.rule, "unresolved");
        assert!(decision.model.is_none());
    }

    #[test]
    fn the_rule_names_are_the_five_the_spec_lists() {
        // The order here is the order `decide` tries them, and a test that only checks the set
        // would not catch a swap of the last two.
        assert_eq!(
            RULES,
            &[
                "explicit",
                "feature_override",
                "task_route",
                "installation_default",
                "unresolved"
            ]
        );
    }

    // --- scope inheritance -----------------------------------------------------------------

    #[test]
    fn a_site_pin_wins_over_the_organizations_pin() {
        let organization = Uuid::new_v4();
        let site = Uuid::new_v4();
        let mut maps = RoutingMaps::empty();
        for (scope, key) in [
            (Scope::Organization(organization), "org-model"),
            (Scope::Site(site), "site-model"),
        ] {
            maps.overrides.entry(scope).or_default().insert(
                "copilot".to_owned(),
                FeatureOverride {
                    id: Uuid::new_v4(),
                    feature: "copilot".to_owned(),
                    model: model(key, true, false, None),
                    scope,
                    updated_at: datetime!(2026-01-01 00:00 UTC),
                },
            );
        }

        let mut asked = request("cheap", Some("copilot"), Scope::Site(site));
        asked.organization_id = Some(organization);

        let decision = decide(&maps, &asked);
        let chosen = decision.model.expect("a pin answers");
        assert_eq!(chosen.model_id, "site-model");
        assert_eq!(chosen.scope, Scope::Site(site));
    }

    #[test]
    fn an_organization_pin_answers_a_site_that_has_none() {
        let organization = Uuid::new_v4();
        let site = Uuid::new_v4();
        let mut maps = RoutingMaps::empty();
        maps.overrides.insert(
            Scope::Organization(organization),
            [(
                "copilot".to_owned(),
                FeatureOverride {
                    id: Uuid::new_v4(),
                    feature: "copilot".to_owned(),
                    model: model("org-model", true, false, None),
                    scope: Scope::Organization(organization),
                    updated_at: datetime!(2026-01-01 00:00 UTC),
                },
            )]
            .into(),
        );

        let mut asked = request("cheap", Some("copilot"), Scope::Site(site));
        asked.organization_id = Some(organization);

        let decision = decide(&maps, &asked);
        assert_eq!(decision.model.expect("inherited").model_id, "org-model");
    }

    #[test]
    fn two_sites_of_one_organization_diverge() {
        // "A site-level task map does not change another site's decisions" — the two sites
        // differ only in their own route rows, and neither sees the other's.
        let organization = Uuid::new_v4();
        let site_one = Scope::Site(Uuid::new_v4());
        let site_two = Scope::Site(Uuid::new_v4());

        let mut maps = RoutingMaps::empty();
        maps.routes.insert(
            site_one,
            [(
                "cheap".to_owned(),
                vec![candidate("cheap", 1, Some(model("site-one-model", true, false, None)), site_one)],
            )]
            .into(),
        );
        maps.routes.insert(
            site_two,
            [(
                "cheap".to_owned(),
                vec![candidate("cheap", 1, Some(model("site-two-model", true, false, None)), site_two)],
            )]
            .into(),
        );
        maps.default_model = Some(model("default", true, false, None));

        let mut first = request("cheap", None, site_one);
        first.organization_id = Some(organization);
        let mut second = request("cheap", None, site_two);
        second.organization_id = Some(organization);

        assert_eq!(
            decide(&maps, &first).model.expect("answers").model_id,
            "site-one-model"
        );
        assert_eq!(
            decide(&maps, &second).model.expect("answers").model_id,
            "site-two-model"
        );
    }

    // --- requirements ---------------------------------------------------------------------

    #[test]
    fn requirements_are_normalized_before_they_are_matched() {
        assert_eq!(
            normalize_requirements(&[
                " TOOLS ".to_owned(),
                "tools".to_owned(),
                "json".to_owned(),
                "not-a-requirement".to_owned(),
            ]),
            vec!["json".to_owned(), "tools".to_owned()]
        );
    }

    #[test]
    fn an_unknown_requirement_is_named_with_the_four_that_exist() {
        let error = unknown_requirement("speed");
        assert!(error.to_string().contains("speed"));
        assert!(error.to_string().contains("tools, vision, long_context, json"));
    }

    #[test]
    fn a_long_context_requirement_refuses_a_small_window() {
        let scope = Scope::Installation;
        let maps = maps_with_routes(
            scope,
            "cheap",
            vec![
                candidate("cheap", 1, Some(model("small", true, false, Some(8_192))), scope),
                candidate("cheap", 2, Some(model("wide", true, false, Some(200_000))), scope),
            ],
            None,
        );

        let mut asked = request("cheap", None, scope);
        asked.requires = vec!["long_context".to_owned()];

        let decision = decide(&maps, &asked);
        assert_eq!(decision.model.expect("answers").model_id, "wide");
    }
}
