//! The plan's stored shape (docs/requests/REQ-045).
//!
//! A **plan** is one generation attempt: the request that was made, the artifacts the model
//! produced from it, and where those artifacts stand. The lifecycle is
//! `generating → draft → approved → applying → applied` with `rejected` and `failed` as the
//! two ways a plan stops.
//!
//! Nothing here is executable. Artifacts are drafts a person reviews; the apply runner (a
//! later slice) is the only writer of live tables. That separation is the request's own —
//! *"…and the app is actually created"* is a second act, not a continuation.
//!
//! `status` and `kind` are `String`s rather than enums for the same reason
//! `workflows.trigger` is one: a row is read back with whatever a future migration may have
//! added, and an unknown value must not make a stored plan unreadable. The checks that admit
//! a value are database constraints, and the constructors below refuse what they would
//! refuse.

use serde_json::Value;
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

/// Longest a plan's title may be (the migration's check is the authority; this is the same
/// number so a refusal happens in validation rather than in the insert).
pub const MAX_TITLE_LEN: usize = 120;

/// Longest a prompt may be.
pub const MAX_PROMPT_LEN: usize = 4000;

/// Shortest a prompt may be once trimmed — a plan whose request is whitespace is a plan
/// whose generator was handed nothing.
pub const MIN_PROMPT_LEN: usize = 3;

/// Every status a plan may hold, in lifecycle order.
///
/// Ordered the way the console shows them rather than alphabetically: the middle three are
/// what an operator is waiting on, and `failed` is what they are looking for.
pub const STATUSES: &[&str] = &[
    "generating",
    "draft",
    "approved",
    "applying",
    "applied",
    "rejected",
    "failed",
];

/// Statuses that still need a person to do something.
pub const OPEN_STATUSES: &[&str] = &["generating", "draft", "approved", "applying"];

/// Status a plan is written in before the provider is called.
///
/// The row exists before the answer does, so a generation that dies mid-flight leaves a
/// `failed` plan naming its reason instead of leaving nothing at all.
pub const NEW_PLAN_STATUS: &str = "generating";

/// Every artifact kind, in the order the review tree groups them.
///
/// The order is the request's own pipeline (entity → fields → UI → permissions → roles →
/// workflow → notifications → reports), so the tree reads top to bottom in the order apply
/// would execute them.
pub const KINDS: &[&str] = &[
    "entity",
    "field",
    "ui",
    "permission",
    "role",
    "workflow",
    "notification",
    "report",
];

/// Every status an artifact may hold.
pub const ARTIFACT_STATUSES: &[&str] = &["pending", "accepted", "rejected", "edited", "invalid"];

/// Kinds apply cannot leave unresolved. A plan missing any of these is not a plan the apply
/// button may offer: "the app is created" includes the permission set that keeps it from
/// being a hole, and a screen nobody can reach is not a feature.
pub const REQUIRED_KINDS: &[&str] = &[
    "entity",
    "field",
    "ui",
    "permission",
    "workflow",
    "notification",
    "report",
];

/// `true` when a status is one the catalogue admits.
#[must_use]
pub fn plan_status_is_known(status: &str) -> bool {
    STATUSES.contains(&status)
}

/// `true` when an artifact status is one the catalogue admits.
#[must_use]
pub fn artifact_status_is_known(status: &str) -> bool {
    ARTIFACT_STATUSES.contains(&status)
}

/// `true` when a kind is one the catalogue admits.
#[must_use]
pub fn kind_is_known(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// What an artifact has to be resolved to before apply may run.
#[must_use]
pub fn artifact_is_resolved(status: &str) -> bool {
    matches!(status, "accepted" | "edited")
}

/// A stored plan.
#[derive(Debug, Clone, FromRow)]
pub struct AppBuilderPlan {
    /// Plan identity.
    pub id: Uuid,
    /// Owning organization; `null` for a platform account.
    pub organization_id: Option<Uuid>,
    /// Site scope, when the app is about one site.
    pub site_id: Option<Uuid>,
    /// The request as it was typed.
    pub prompt: String,
    /// Human-readable name; the model's wins when it sends one, a human's otherwise.
    pub title: String,
    /// Where the plan stands.
    pub status: String,
    /// Which attempt this is within a chain.
    pub plan_version: i32,
    /// The model that wrote it, frozen at generation.
    pub model_label: String,
    /// Tokens the provider reported, `null` when it reported none.
    pub tokens_in: Option<i32>,
    /// Tokens the provider reported, `null` when it reported none.
    pub tokens_out: Option<i32>,
    /// Attributed cost in cents.
    pub cost_cents: i32,
    /// Why a generation failed, when it did.
    pub error: Option<String>,
    /// Who asked.
    pub created_by: Option<Uuid>,
    /// When apply finished, `null` until then.
    pub applied_at: Option<OffsetDateTime>,
    /// Row creation.
    pub created_at: OffsetDateTime,
    /// Last write.
    pub updated_at: OffsetDateTime,
    /// The attempt this one replaces, `null` for a first attempt.
    pub supersedes_id: Option<Uuid>,
    /// Why the whole plan was rejected, `null` unless `status` is `rejected`.
    ///
    /// Set by a reviewer's rejection and by the two machine retirements that also land on
    /// `rejected` — a plan a fresh attempt superseded, and a generation that failed. It is
    /// never cleared, because "rejected" is terminal and a reason that disappears is a
    /// decision that cannot be learned from.
    pub decision_reason: Option<String>,
}

/// A stored artifact.
#[derive(Debug, Clone, FromRow)]
pub struct AppBuilderArtifact {
    /// Artifact identity.
    pub id: Uuid,
    /// The plan it belongs to.
    pub plan_id: Uuid,
    /// Which kind of artifact.
    pub kind: String,
    /// Stable within (plan, kind).
    pub key: String,
    /// The artifact this one hangs off, `null` at the top of the tree.
    pub parent_key: Option<String>,
    /// Order inside the tree.
    pub ordinal: i32,
    /// Where the reviewer has got to with it.
    pub status: String,
    /// The artifact itself, in the shape its validator reads.
    pub spec: Value,
    /// The model's own explanation.
    pub rationale: String,
    /// Findings the validator produced, `{ path, message }` each.
    pub validation: Value,
    /// The artifact this one replaced.
    pub supersedes_id: Option<Uuid>,
    /// Row creation.
    pub created_at: OffsetDateTime,
    /// Last write.
    pub updated_at: OffsetDateTime,
    /// Why the reviewer refused this artifact, `null` unless `status` is `rejected`.
    ///
    /// **Not every `rejected` row has one**, and that is deliberate: `supersede_artifact`
    /// retires the row a regeneration replaced with `rejected` and no reason, because nothing
    /// was decided — the version was simply overtaken. A reviewer must give a reason; a
    /// machine does not have to invent one.
    pub rejected_reason: Option<String>,
}

/// One apply run.
#[derive(Debug, Clone, FromRow)]
pub struct AppBuilderApplication {
    /// Run identity.
    pub id: Uuid,
    /// The plan being applied.
    pub plan_id: Uuid,
    /// Where the run stands.
    pub status: String,
    /// What it created, by kind.
    pub summary: Value,
    /// Entity keys this run created — what a rollback may remove and nothing else.
    pub created_entity_keys: Value,
    /// Who ran it.
    pub applied_by: Option<Uuid>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it stopped, `null` while running.
    pub finished_at: Option<OffsetDateTime>,
}

/// One step of an apply run.
#[derive(Debug, Clone, FromRow)]
pub struct AppBuilderApplicationStep {
    /// Row identity.
    pub id: i64,
    /// The run it belongs to.
    pub application_id: Uuid,
    /// Order in the run; unique per application, so the order is the table's order.
    pub ordinal: i32,
    /// Which stage (`entities`, `fields`, `permissions`, …).
    pub kind: String,
    /// What the progress panel shows.
    pub label: String,
    /// Where the step stands.
    pub status: String,
    /// Anything the step learned — created keys, the reason it failed.
    pub detail: Value,
    /// When it began.
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    pub finished_at: Option<OffsetDateTime>,
}

/// Token counts and what they cost.
///
/// One type rather than two loose fields, because the console shows them together and a row
/// that had one without the other would render as a half-answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanUsage {
    /// Tokens in, `None` when the provider reported none.
    pub input: Option<i32>,
    /// Tokens out, `None` when the provider reported none.
    pub output: Option<i32>,
    /// Attributed cost in cents.
    pub cost_cents: i32,
}

/// A new plan, before the provider is called.
#[derive(Debug, Clone)]
pub struct NewPlan {
    /// Owning organization.
    pub organization_id: Option<Uuid>,
    /// Site scope.
    pub site_id: Option<Uuid>,
    /// The request.
    pub prompt: String,
    /// A human's name for it, if one was typed.
    pub title: Option<String>,
    /// The model about to be asked.
    pub model_label: String,
    /// Who asked.
    pub created_by: Option<Uuid>,
    /// The attempt this replaces.
    pub supersedes_id: Option<Uuid>,
}

/// A new artifact, as the generator proposes it.
#[derive(Debug, Clone)]
pub struct NewArtifact {
    /// Which kind.
    pub kind: String,
    /// Stable key inside (plan, kind).
    pub key: String,
    /// What it hangs off.
    pub parent_key: Option<String>,
    /// Order in the tree.
    pub ordinal: i32,
    /// The artifact itself.
    pub spec: Value,
    /// The model's explanation.
    pub rationale: String,
    /// What the validator said; empty when nothing was checked yet.
    pub validation: Value,
}

/// A new artifact as a reviewer edited it.
///
/// Separate from [`NewArtifact`] because the two carry different obligations: the model's
/// version may be invalid and is stored as such, while an edit that claims `edited` without
/// naming what it replaced would make the apply log unreadable.
#[derive(Debug, Clone)]
pub struct EditedArtifact {
    /// The artifact's new body.
    pub spec: Value,
    /// Re-validated findings for the new body.
    pub validation: Value,
    /// The artifact this edit replaced.
    pub supersedes_id: Uuid,
}

/// The list screen's filter.
#[derive(Debug, Clone, Default)]
pub struct PlanFilter {
    /// Restrict to one status.
    pub status: Option<String>,
    /// Case-insensitive substring of title or prompt.
    pub text: Option<String>,
    /// Restrict to one author.
    pub created_by: Option<Uuid>,
    /// Offset for paging.
    pub offset: i64,
    /// Page size.
    pub limit: i64,
}

/// One page of plans.
#[derive(Debug, Clone)]
pub struct PlanPage {
    /// The rows.
    pub plans: Vec<AppBuilderPlan>,
    /// How many match the filter in total.
    pub total: i64,
    /// The artifact and entity counts the list's columns show, by plan id.
    ///
    /// Read in the same query as the page rather than per row: the list is re-fetched on
    /// every filter change and a count per row turns one query into eleven.
    pub counts: Vec<PlanCounts>,
}

/// What the list screen shows beside a plan.
///
/// `Serialize` **and** `Deserialize` because the counts ride the same body as the plan they
/// decorate, in the list and in every decision answer: a client that has to fetch a second
/// endpoint to draw the footer bar is a screen that shows a number the reviewer cannot act
/// on. The `Deserialize` half exists for the export file — `omnion.app-builder.plan/1`
/// carries the counters so a reader outside the panel sees the same "4 accepted · 3 pending"
/// the operator did, and a type that could only be written could not be read back to prove
/// the file round-trips.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanCounts {
    /// Artifacts of every kind.
    pub artifacts: i64,
    /// Artifacts the reviewer has resolved.
    pub accepted: i64,
    /// Artifacts the reviewer refused.
    pub rejected: i64,
    /// Artifacts still waiting.
    pub pending: i64,
    /// Artifacts the validator refused.
    pub invalid: i64,
}
