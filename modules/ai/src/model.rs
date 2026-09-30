//! The draft's stored shape (docs/requests/REQ-046).
//!
//! A **draft** is one answer from the model that has not yet become a workflow: the prompt it
//! was asked for, the definition it produced, the rationale it gave and the decision a person
//! made about it. The lifecycle is
//! `generating → draft → approved → activated` with `rejected` and `failed` as the two ways a
//! draft stops.
//!
//! `status` is a `String` here rather than an enum for the same reason `workflows.trigger` is
//! one: the row is read back with whatever a future migration may have added, and an unknown
//! status must not make a stored draft unreadable. The check that admits a status is a
//! database constraint, and the constructors below refuse a value it would refuse.

use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

/// Longest a draft's title may be (the migration's check is the authority; this is the same
/// number so a refusal happens in the answer's own validation rather than in the insert).
pub const MAX_TITLE_LEN: usize = 120;

/// Longest a prompt may be.
pub const MAX_PROMPT_LEN: usize = 4000;

/// Shortest a prompt may be once trimmed — a row whose prompt is whitespace is a row whose
/// generator was handed nothing.
pub const MIN_PROMPT_LEN: usize = 1;

/// Every status a draft may hold, in lifecycle order.
///
/// Ordered the way the console shows them rather than alphabetically, because the first two
/// are the ones an operator is waiting on and `failed` is the one they are looking for.
pub const STATUSES: &[&str] = &[
    "generating",
    "draft",
    "approved",
    "activated",
    "rejected",
    "failed",
];

/// Statuses that still need a person to do something.
pub const OPEN_STATUSES: &[&str] = &["generating", "draft", "approved"];

/// Status a row is written in before the provider is called.
///
/// Named rather than written as a literal at the call site: the whole reason this state exists
/// is that a generation which dies mid-flight must leave a row behind, and a literal
/// `"generating"` in three places is three places that can drift from the vocabulary above.
pub const NEW_DRAFT_STATUS: &str = "generating";

/// `true` when the value names a status the row's constraint admits.
#[must_use]
pub fn draft_status_is_known(status: &str) -> bool {
    STATUSES.contains(&status)
}

/// One stored draft.
#[derive(Debug, Clone, FromRow)]
pub struct AiWorkflowDraft {
    /// Draft id.
    pub id: Uuid,
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// Site it is scoped to, when it is.
    pub site_id: Option<Uuid>,
    /// What the draft is called.
    pub title: String,
    /// The prompt it was generated from.
    pub prompt: String,
    /// The model's own explanation, markdown; `null` until an answer validated.
    pub rationale: Option<String>,
    /// The validated definition: `{ trigger, steps }`.
    pub definition: Option<serde_json::Value>,
    /// Lifecycle state.
    pub status: String,
    /// The workflow approval materialised, once it has.
    pub workflow_id: Option<Uuid>,
    /// The model that wrote it, frozen at generation.
    pub model_key: Option<String>,
    /// Input tokens the answer reported.
    pub tokens_input: Option<i32>,
    /// Output tokens the answer reported.
    pub tokens_output: Option<i32>,
    /// Why generation failed, when it did.
    pub error: Option<String>,
    /// The last revision prompt an operator sent.
    pub revision_note: Option<String>,
    /// How many revision round-trips this draft has had.
    pub revision_count: i32,
    /// Who asked for it.
    pub created_by: Option<Uuid>,
    /// Who decided.
    pub decided_by: Option<Uuid>,
    /// Why it was rejected; required for `rejected` (the store refuses otherwise).
    pub decision_reason: Option<String>,
    /// When the draft was created.
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    pub updated_at: OffsetDateTime,
    /// When a person decided.
    pub decided_at: Option<OffsetDateTime>,
}

impl AiWorkflowDraft {
    /// `true` when the draft still needs a person.
    #[must_use]
    pub fn is_open(&self) -> bool {
        OPEN_STATUSES.contains(&self.status.as_str())
    }

    /// `true` when generation ran out of answers.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.status == "failed"
    }

    /// `true` when the draft has a validated definition stored.
    #[must_use]
    pub fn has_definition(&self) -> bool {
        self.definition.as_ref().is_some_and(|value| value.is_object())
    }

    /// The definition, when there is one.
    ///
    /// `Option` rather than a default: a draft still `generating` has no definition, and a
    /// caller that reads `Value::Null` out of this would hand `null` to the engine's
    /// deserialiser and get a message about JSON rather than about the row it is looking at.
    #[must_use]
    pub fn definition(&self) -> Option<&serde_json::Value> {
        self.definition.as_ref().filter(|value| value.is_object())
    }
}

/// A draft to create, already validated.
#[derive(Debug, Clone)]
pub struct NewDraft {
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// Optional site scope.
    pub site_id: Option<Uuid>,
    /// What the draft is called.
    pub title: String,
    /// The prompt it answers.
    pub prompt: String,
    /// The model's explanation, when the answer carried one.
    pub rationale: Option<String>,
    /// The validated definition.
    pub definition: Option<serde_json::Value>,
    /// Lifecycle state to start in (`generating` for the row written before the answer).
    pub status: String,
    /// The model that answered.
    pub model_key: Option<String>,
    /// Input tokens reported.
    pub tokens_input: Option<i32>,
    /// Output tokens reported.
    pub tokens_output: Option<i32>,
    /// Why generation failed.
    pub error: Option<String>,
    /// Who asked for it.
    pub created_by: Option<Uuid>,
}

/// What a list screen asks for.
#[derive(Debug, Clone, Default)]
pub struct DraftFilter {
    /// Statuses to keep; empty means every status.
    pub statuses: Vec<String>,
    /// Free-text over title and prompt; both fields, because an operator remembers the words
    /// they typed far more often than the name the model gave them.
    pub query: Option<String>,
    /// Restrict to one author.
    pub created_by: Option<Uuid>,
    /// Rows to skip (the list is offset-paged because the filters live in the URL and an
    /// offset survives a filter change better than a cursor does).
    pub offset: i64,
    /// Rows to return.
    pub limit: i64,
}

/// One page of drafts and how many rows matched.
#[derive(Debug, Clone)]
pub struct DraftPage {
    /// The page's rows, newest first.
    pub drafts: Vec<AiWorkflowDraft>,
    /// How many rows match the filter, ignoring the page window.
    pub total: i64,
}

/// The whole spend of one draft, for the console's "what did this cost" line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DraftTokens {
    /// Input tokens across every answer.
    pub input: i64,
    /// Output tokens across every answer.
    pub output: i64,
}

impl DraftTokens {
    /// Add one answer's counts. A count a provider did not report is `0`, never a subtraction:
    /// an absent number is missing, not negative.
    pub fn add(&mut self, usage_input: Option<i64>, usage_output: Option<i64>) {
        self.input += usage_input.unwrap_or_default().max(0);
        self.output += usage_output.unwrap_or_default().max(0);
    }

    /// Input + output.
    #[must_use]
    pub fn total(&self) -> i64 {
        self.input + self.output
    }
}
