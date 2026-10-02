//! The human-in-the-loop gate: a step that parks a run until a person decides (REQ-003
//! slice 3).
//!
//! A `wait_for_approval` step is a **suspend**, not an action: it has no effect of its own,
//! it stops the run until somebody with `workflows.approve` says otherwise. So the engine
//! owns it, the way it owns a wait, and the machinery lives here beside the wait's rather
//! than in the automation layer's action registry — a host action that *does* something is
//! exactly what this is not.
//!
//! Three decisions are the gate's own, and each of them is where a naive implementation
//! has a security hole:
//!
//! 1. **The token is a credential.** Whoever holds it can let an e-mail leave the process,
//!    so only its SHA-256 is stored ([`issued_token`]), it is single-use (the unique index
//!    on `decision_token_hash` plus the one-write decision), and it expires
//!    ([`approval_ttl`]). A `GET` that mutates state is never offered: the decision posts
//!    the token in its body.
//! 2. **The decision is one write.** [`decide`] takes the row with
//!    `for update … and decision is null`, so a second decision — two browser tabs, a
//!    double click, a retried request — matches zero rows and is reported as *already
//!    decided* rather than applying a second time. The token being unique is not enough on
//!    its own: uniqueness says two rows cannot share it, not that one row is used once.
//! 3. **An expired gate is expired forever.** The sweep marks it `rejected` rather than
//!    leaving it pending, so a decision that arrives a week late is refused with a reason
//!    instead of reviving a run whose data has moved on.
//!
//! The run's own status while parked is `awaiting_approval` ([`crate::model`]), and both
//! the engine's claim query and the sweeper's reconciler read that status: a parked run is
//! never claimed and never settled behind a person's back.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::model::WorkflowExecution;

/// How many hex characters a decision token carries — 16 CSPRNG bytes.
pub const TOKEN_LENGTH: usize = 32;

/// The alphanumerics a random token is drawn from.
///
/// Lower case only: a token is pasted into a ticket, a terminal and a chat message, and
/// `O`/`0` and `l`/`1` are the two pairs that make an operator transcribe a *different*
/// token than the one they were given. The hooks' namespace prefix does the same job for
/// an inbound URL; a decision token never appears in a URL (it goes in a body), so it needs
/// no namespace.
const TOKEN_ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyz23456789";

/// `length` characters drawn from the CSPRNG.
fn random_chars(length: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| {
            TOKEN_ALPHABET[usize::try_from(rng.gen_range(0..TOKEN_ALPHABET.len())).unwrap_or(0)]
                as char
        })
        .collect()
}

/// Default lifetime of an approval, in hours — `automation_settings.approval_ttl_hours`.
pub const DEFAULT_TTL_HOURS: i32 = 72;

/// Ceiling of the lifetime an installation may grant an approval (30 days).
pub const MAX_TTL_HOURS: i32 = 720;

/// Longest a note on a decision may be.
pub const MAX_NOTE: usize = 2_000;

/// The permission that may let a parked run go on.
///
/// It is a constant rather than a `format!` because the gate's whole security story is
/// "the same key the API guards the decision endpoint with" — one string, written once, so
/// the panel's offer, the engine's check and the route's guard cannot drift apart.
pub const APPROVAL_PERMISSION: &str = "workflows.approve";

/// The event recorded when a run parks on a person.
pub const REQUESTED_EVENT: &str = "workflow.approval.requested";

/// The event recorded when somebody decides.
pub const DECIDED_EVENT: &str = "workflow.approval.decided";

/// A decision, as the engine reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The run goes on past the gate.
    Approved,
    /// The run ends at the gate.
    Rejected,
}

impl Decision {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// A freshly minted decision token and the hash stored beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    /// The token, shown to the deciding person once and never stored.
    pub token: String,
    /// SHA-256 of the token — the only half that reaches the database.
    pub hash: String,
}

/// Mint a decision token.
///
/// 32 hex characters of CSPRNG output: long enough that guessing one is not a project,
/// short enough that a token pasted into a ticket still reads as a token. The token is
/// *not* a JWT and carries nothing — it is an opaque handle whose only power is to say
/// "this specific gate was decided by this specific person at this specific moment".
pub fn issued_token() -> IssuedToken {
    let token = random_chars(TOKEN_LENGTH);
    IssuedToken {
        hash: hash_token(&token),
        token,
    }
}

/// The stored hash of a token.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// `true` when a string is shaped like a token this module minted.
///
/// A guard, not a validator: it keeps an obvious mistake (posting a UUID, posting a rule
/// name) from turning into a database round trip per click, and it never rejects a real
/// token. A `false` here is *not* an error the caller should report as "wrong token" — the
/// hash lookup answers that — so the API uses it to pick a message, not to refuse.
#[must_use]
pub fn looks_like_token(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() == TOKEN_LENGTH
        && trimmed
            .as_bytes()
            .iter()
            .all(|byte| TOKEN_ALPHABET.contains(byte))
}

/// The gate's parameters, as the author writes them.
///
/// Read here rather than ad hoc in the engine so a definition is checked against exactly
/// what will be stored — the same discipline the wait's `seconds` follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalParams {
    /// The permission that may decide (`workflows.approve` unless the author says
    /// otherwise).
    pub permission: String,
    /// The message the deciding person reads.
    pub message: String,
    /// How long the gate stays open, in hours.
    pub expires_in_hours: i32,
}

impl Default for ApprovalParams {
    fn default() -> Self {
        Self {
            permission: APPROVAL_PERMISSION.to_owned(),
            message: "a person must approve this step before the run goes on".to_owned(),
            expires_in_hours: DEFAULT_TTL_HOURS,
        }
    }
}

/// Read and check a gate's parameters.
pub fn params_from(raw: &serde_json::Value) -> Result<ApprovalParams> {
    let default = ApprovalParams::default();

    let permission = match raw.get("permission") {
        None | Some(serde_json::Value::Null) => default.permission,
        Some(serde_json::Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(WorkflowError::invalid(
                    "invalid_approval",
                    "an approval step names the permission that may decide it; an empty one \
                     decides nothing",
                ));
            }
            // A permission key is dotted lower case; refusing anything else here is what
            // keeps a definition from parking on a gate nobody can ever open.
            if !trimmed.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            }) {
                return Err(WorkflowError::invalid(
                    "invalid_approval",
                    format!(
                        "`{trimmed}` is not a permission key; a gate is opened by one, e.g. \
                         {APPROVAL_PERMISSION}"
                    ),
                ));
            }
            trimmed.to_owned()
        }
        Some(_) => {
            return Err(WorkflowError::invalid(
                "invalid_approval",
                "an approval step's `permission` is text",
            ));
        }
    };

    let message = match raw.get("message") {
        None | Some(serde_json::Value::Null) => default.message,
        Some(serde_json::Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(WorkflowError::invalid(
                    "invalid_approval",
                    "an approval step needs a message the deciding person reads",
                ));
            }
            if trimmed.chars().count() > 500 {
                return Err(WorkflowError::invalid(
                    "invalid_approval",
                    "an approval message is at most 500 characters",
                ));
            }
            trimmed.to_owned()
        }
        Some(_) => {
            return Err(WorkflowError::invalid(
                "invalid_approval",
                "an approval step's `message` is text",
            ));
        }
    };

    let expires_in_hours = match raw.get("expires_in_hours") {
        None | Some(serde_json::Value::Null) => default.expires_in_hours,
        Some(value) => {
            let hours = value.as_i64().ok_or_else(|| {
                WorkflowError::invalid(
                    "invalid_approval",
                    "an approval step's `expires_in_hours` is a whole number of hours",
                )
            })?;
            if !(1..=i64::from(MAX_TTL_HOURS)).contains(&hours) {
                return Err(WorkflowError::invalid(
                    "invalid_approval",
                    format!("an approval may wait 1 to {MAX_TTL_HOURS} hours, got {hours}"),
                ));
            }
            i32::try_from(hours).unwrap_or(default.expires_in_hours)
        }
    };

    Ok(ApprovalParams {
        permission,
        message,
        expires_in_hours,
    })
}

/// When a gate opened at `opened_at` stops accepting decisions.
#[must_use]
pub fn approval_deadline(params: &ApprovalParams, opened_at: OffsetDateTime) -> OffsetDateTime {
    opened_at + Duration::hours(i64::from(params.expires_in_hours))
}

/// Reopen a run after a decision and hand the gate's step back to the queue.
///
/// Both statuses are accepted — `awaiting_approval` is the normal one, `running` covers a
/// sweep that already expired the gate and resumed the run itself — and the step goes back
/// to `pending` with its attempt count kept, so the second claim of a gate is *its* resume,
/// not a fresh budget. `available_at` is reset to now: a gate that was approved an hour
/// after it opened must not wait out the remainder of its own expiry.
pub async fn resume_after_decision(pool: &PgPool, execution_id: Uuid, step_id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflow_executions set status = 'running', approval_id = null, \
         finished_at = null where id = $1 and status in ('awaiting_approval', 'running')",
    )
    .bind(execution_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "update workflow_steps set status = 'pending', started_at = null, available_at = now() \
         where id = $1 and status = 'waiting'",
    )
    .bind(step_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// End a run at a rejected gate: the step is cancelled and the run is settled as cancelled.
///
/// A *rejection* is a decision, not a failure, so the run settles as `cancelled` — the same
/// state a person's "cancel" produces, and not `failed`: nothing in the run went wrong, the
/// rule was told not to do this, and an operator reading a list of runs should not see a
/// red row for a gate that did its job. The gate's own step keeps the reason in its error.
pub async fn end_at_rejection(
    pool: &PgPool,
    execution_id: Uuid,
    step_id: Uuid,
    reason: &str,
) -> Result<()> {
    // The steps *after* the gate are closed first, with the branch's own sentence — "the
    // run ended before this step" — which is right for them. Order matters: this helper
    // exempts one `step_id` and stamps the rest, so it has to run *before* the gate's own
    // row is written, or the gate's real reason gets overwritten by the generic one and an
    // operator reading the trace sees a step that "never ran" rather than one a person
    // refused.
    let _ = crate::store::end_run_after_branch(pool, execution_id, step_id).await?;

    sqlx::query(
        "update workflow_steps set status = 'cancelled', finished_at = now(), error = $2 \
         where id = $1 and status in ('waiting', 'pending', 'running')",
    )
    .bind(step_id)
    .bind(reason)
    .execute(pool)
    .await?;

    // Settle the run **from `awaiting_approval`, not from `running`**. This is the bug the
    // first version of this function had: it reached for the store's `settle_execution_as`,
    // which guards on `status = 'running'`, and a rejection happens while the run is
    // *parked* — so the write matched zero rows and the run sat in `awaiting_approval`
    // forever, with a gate that said "rejected" and a trace that never ended. The two
    // voluntary endings (a branch, a stop) both run from `running` and the store's guard is
    // right for them; a rejection is the one ending that starts from a parked run, so it
    // needs its own predicate.
    let settled = sqlx::query(
        "update workflow_executions set status = 'cancelled', finished_at = now(), \
         approval_id = null where id = $1 and status = 'awaiting_approval'",
    )
    .bind(execution_id)
    .execute(pool)
    .await?
    .rows_affected();

    if settled == 0 {
        // Not parked: the run was cancelled, failed or completed between the decision and
        // this write. A decision on a run that is no longer waiting is not an error — the
        // caller already answered from the row it read — so the ending falls back to the
        // store's ordinary guard and lets whoever won the race stand.
        crate::store::settle_execution_as(pool, execution_id, "cancelled").await?;
    }

    Ok(())
}

/// A parked gate nobody decided.
///
/// The read the engine makes on its *second* claim: has this run been let go? A gate that
/// was approved or rejected must not be resumed by the claim that noticed it, so the
/// answer is a row read and not a `status` check.
pub async fn decision_of(pool: &PgPool, approval_id: Uuid) -> Result<Option<Decision>> {
    let stored: Option<String> =
        sqlx::query_scalar("select decision from workflow_approvals where id = $1")
            .bind(approval_id)
            .fetch_optional(pool)
            .await?;

    stored
        .as_deref()
        .map(|raw| {
            Decision::parse(raw).ok_or_else(|| {
                WorkflowError::invalid(
                    "workflow_store_error",
                    format!("the approval carries the unknown decision {raw:?}"),
                )
            })
        })
        .transpose()
}

/// Close a gate whose deadline passed without a decision.
///
/// One write, guarded on `decision is null`: the sweeper and a person pressing *Reject* at
/// the same moment must not both decide, and whichever got there first is what the row
/// says. Returns `true` for the call that wrote the row — the same "only the writer audits"
/// shape [`crate::store::settle_execution`] uses for a run.
pub async fn expire_approval(pool: &PgPool, approval_id: Uuid) -> Result<bool> {
    let written = sqlx::query(
        "update workflow_approvals set decision = 'rejected', decided_at = now(), \
         note = 'the approval expired before anybody decided it' \
         where id = $1 and decision is null and expires_at <= now()",
    )
    .bind(approval_id)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(written > 0)
}

/// The gates that are parked, undecided and past their deadline.
///
/// Bounded by `limit` and ordered by deadline, so a long queue of expired gates is worked
/// through oldest-first rather than in whatever order the planner returns.
pub async fn expired_approvals(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select id from workflow_approvals \
         where decision is null and expires_at <= now() \
         order by expires_at asc, id limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(ids)
}

/// Park a run on a gate: the row, the step, the run's status, and the audit entry.
///
/// One place, because the three writes must agree: a gate row whose step is not `waiting`
/// is a gate nobody can decide, and a step that is `waiting` with no row is a run that is
/// stuck forever with no panel row explaining it. The audit row is written here rather than
/// by the caller so the trail cannot lose one of the paths.
pub async fn open(
    pool: &PgPool,
    execution: &WorkflowExecution,
    step_id: Uuid,
    step_no: i32,
    params: &ApprovalParams,
    now: OffsetDateTime,
) -> Result<IssuedToken> {
    let issued = issued_token();
    let deadline = approval_deadline(params, now);

    let mut transaction = pool.begin().await?;

    let row: (Uuid,) = sqlx::query_as(
        "insert into workflow_approvals (execution_id, step_id, step_no, organization_id, \
         rule_id, requested_at, expires_at, decision_token_hash) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(execution.id)
    .bind(step_id)
    .bind(step_no)
    .bind(execution.organization_id)
    .bind(execution.workflow_id)
    .bind(now)
    .bind(deadline)
    .bind(&issued.hash)
    .fetch_one(&mut *transaction)
    .await?;
    let approval_id = row.0;

    sqlx::query(
        "update workflow_steps set status = 'waiting', started_at = null, approval_id = $2, \
         available_at = $3 where id = $1 and status = 'running'",
    )
    .bind(step_id)
    .bind(approval_id)
    .bind(deadline)
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "update workflow_executions set status = 'awaiting_approval', approval_id = $2 \
         where id = $1 and status = 'running'",
    )
    .bind(execution.id)
    .bind(approval_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    tracing::info!(
        execution_id = %execution.id,
        approval_id = %approval_id,
        step_no,
        permission = %params.permission,
        expires_at = %deadline,
        "a run parked on an approval"
    );

    Ok(issued)
}

// ---------------------------------------------------------------------------------------------
// The reads the panel makes
// ---------------------------------------------------------------------------------------------

/// Columns of one gate, joined with the step it parks and the rule it came from.
pub const APPROVAL_COLUMNS: &str = "a.id, a.execution_id, a.step_id, a.step_no, \
     a.organization_id, a.rule_id, a.requested_at, a.expires_at, a.decision, a.decided_by, \
     a.decided_at, a.note, s.name as step_name, s.params, w.name as rule_name";

/// One gate, with the step's name and parameters and the rule's name.
///
/// The join is not decoration: the panel's pending panel draws a row per gate, and asking
/// for the step name and the rule name with three more queries *per row* is what a list of
/// twenty waiting gates turns into sixty round trips. One statement, one round trip.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ApprovalRow {
    /// Approval id.
    pub id: Uuid,
    /// The run that is parked.
    pub execution_id: Uuid,
    /// The step inside that run.
    pub step_id: Uuid,
    /// Its 1-based position.
    pub step_no: i32,
    /// Organization the gate belongs to.
    pub organization_id: Uuid,
    /// The rule that asked, when the rule still exists.
    pub rule_id: Option<Uuid>,
    /// When the run parked.
    pub requested_at: OffsetDateTime,
    /// When the gate stops accepting decisions.
    pub expires_at: OffsetDateTime,
    /// `approved`, `rejected` or `null`.
    pub decision: Option<String>,
    /// Who decided, once somebody has.
    pub decided_by: Option<Uuid>,
    /// When they decided.
    pub decided_at: Option<OffsetDateTime>,
    /// Their note.
    pub note: Option<String>,
    /// The gated step's name.
    pub step_name: String,
    /// The gated step's parameters, read back through [`params_from`].
    pub params: serde_json::Value,
    /// The rule's name, when the rule still exists.
    pub rule_name: Option<String>,
}

impl ApprovalRow {
    /// The gate's own parameters, read the way the engine read them when it wrote the row.
    ///
    /// A row whose parameters no longer parse falls back to the defaults rather than
    /// failing: a gate is still a gate, and a panel that refuses to list it because one
    /// column drifted is worse than one that lists it with the platform's own values.
    #[must_use]
    pub fn params(&self) -> ApprovalParams {
        params_from(&self.params).unwrap_or_default()
    }

    /// `true` when nobody has decided and the deadline has passed.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.decision.is_none() && self.expires_at <= now
    }
}

fn approval_select(extra: &str) -> String {
    format!(
        "select {APPROVAL_COLUMNS} from workflow_approvals a \
         join workflow_steps s on s.id = a.step_id \
         left join workflows w on w.id = a.rule_id {extra}"
    )
}

/// The gates of one organization, oldest first.
///
/// `decided` picks the side: `false` is the pending queue, `true` the trail. The predicate
/// is `(decision is not null) = $2` rather than a second query — one statement, and the
/// "no rows" answer is the same for both sides.
pub async fn list(
    pool: &PgPool,
    organization_id: Uuid,
    decided: bool,
    limit: i64,
) -> Result<Vec<ApprovalRow>> {
    let sql = approval_select(
        "where a.organization_id = $1 and (a.decision is not null) = $2 \
         order by a.requested_at asc, a.id limit $3",
    );

    let rows: Vec<ApprovalRow> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(decided)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

/// One gate, with its step and rule.
pub async fn find(pool: &PgPool, approval_id: Uuid) -> Result<Option<ApprovalRow>> {
    let row: Option<ApprovalRow> = sqlx::query_as(&approval_select("where a.id = $1"))
        .bind(approval_id)
        .fetch_optional(pool)
        .await?;

    Ok(row)
}

/// Decide a gate, once, optionally checking the token that was issued for it.
///
/// The one statement that is the whole single-use story: `decision is null` means a second
/// decision matches zero rows, and the caller can report "somebody already decided this"
/// rather than applying twice.
///
/// The token is **optional** and that is a deliberate split of the two things a gate
/// involves:
///
/// * the *authority* to decide is the caller's `workflows.approve` — the route guard, not
///   this function. A session that holds it may open any gate of its organization, which
///   is what the pending panel needs and what "a person with the deciding permission may
///   let this go" has to mean;
/// * the *token* is the second factor a notification carries. It is checked here when it
///   is sent, so a forwarded link whose token belongs to a different gate decides nothing,
///   and a replay of one that was already used matches nothing either.
///
/// `None` therefore means "no second factor presented", never "no authority": the guard
/// has already answered the second question. A token that does not match and a gate that
/// does not exist both come back as `None`, so the caller can answer one way and cannot
/// become an oracle for which gates exist.
pub async fn decide(
    pool: &PgPool,
    approval_id: Uuid,
    token: Option<&str>,
    decision: Decision,
    decided_by: Uuid,
    note: Option<&str>,
) -> Result<Option<Gate>> {
    let sql = match token {
        Some(_) => {
            "update workflow_approvals set decision = $3, decided_by = $4, decided_at = now(), \
             note = $5 where id = $1 and decision_token_hash = $2 and decision is null \
             returning execution_id, step_id"
        }
        // The same statement with the token predicate dropped — written out rather than
        // built with a placeholder, so the two shapes are both visible in the source and
        // neither can be reached with a null comparison.
        None => {
            "update workflow_approvals set decision = $2, decided_by = $3, decided_at = now(), \
             note = $4 where id = $1 and decision is null returning execution_id, step_id"
        }
    };

    let row: Option<Gate> = match token {
        Some(token) => {
            sqlx::query_as(sql)
                .bind(approval_id)
                .bind(hash_token(token))
                .bind(decision.as_str())
                .bind(decided_by)
                .bind(note)
                .fetch_optional(pool)
                .await?
        }
        None => {
            sqlx::query_as(sql)
                .bind(approval_id)
                .bind(decision.as_str())
                .bind(decided_by)
                .bind(note)
                .fetch_optional(pool)
                .await?
        }
    };

    Ok(row)
}

/// What a decision wrote: the run and the step it touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct Gate {
    /// The parked run.
    pub execution_id: Uuid,
    /// The gated step.
    pub step_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_minted_token_is_shown_once_and_stored_as_a_hash() {
        let issued = issued_token();
        assert_eq!(issued.token.len(), TOKEN_LENGTH);
        assert_eq!(hash_token(&issued.token), issued.hash);
        // The hash is not the token, and the token is not recoverable from it.
        assert_ne!(issued.hash, issued.token);
        assert!(looks_like_token(&issued.token));
        assert!(!looks_like_token(&issued.hash), "a hash is not the token");
        // Two mints never collide, so two gates never share a credential.
        assert_ne!(issued_token().token, issued_token().token);
    }

    #[test]
    fn a_token_that_is_not_shaped_like_one_is_refused_without_a_lookup() {
        assert!(!looks_like_token(""));
        assert!(!looks_like_token("not-a-token"));
        assert!(!looks_like_token("00000000-0000-0000-0000-000000000000"));
        assert!(!looks_like_token(&"a".repeat(31)));
        assert!(!looks_like_token(&"a".repeat(33)));
        // The alphabet is lower case, and `0`/`1`/`l`/`o` are not in it: the whole reason
        // a token cannot be transcribed into a different token.
        assert!(
            !TOKEN_ALPHABET.contains(&b'0'),
            "0 is not in the alphabet, so a read-aloud token is unambiguous"
        );
        assert!(!looks_like_token(&"0".repeat(TOKEN_LENGTH)));
        // Whitespace is trimmed rather than refused: a token pasted with a newline is
        // still the token, and the hash trims the same way.
        let minted = issued_token().token;
        assert!(looks_like_token(&format!("  {minted}  ")));
        assert_eq!(hash_token(&format!("  {minted} ")), hash_token(&minted));
    }

    #[test]
    fn gate_parameters_default_rather_than_being_required() {
        // An author who just drops the step into a rule gets a working gate: the platform's
        // own permission, a readable message and the default lifetime.
        let params = params_from(&json!({})).expect("an empty gate is valid");
        assert_eq!(params.permission, APPROVAL_PERMISSION);
        assert_eq!(params.expires_in_hours, DEFAULT_TTL_HOURS);
        assert!(!params.message.is_empty());
    }

    #[test]
    fn a_gate_must_name_a_permission_somebody_can_hold() {
        let params = params_from(&json!({ "permission": "content.pages.publish" }))
            .expect("another permission is a valid gate");
        assert_eq!(params.permission, "content.pages.publish");

        for broken in [
            "",
            "  ",
            "workflows approve",
            "workflows..approve",
            ".approve",
            "approve.",
        ] {
            let error = params_from(&json!({ "permission": broken }))
                .expect_err("a gate nobody can open is refused");
            assert_eq!(error.code(), "invalid_approval", "{broken:?}");
        }
        // A non-string permission is refused on the same grounds: a gate named by a number
        // is not a gate.
        assert_eq!(
            params_from(&json!({ "permission": 7 }))
                .expect_err("not a permission key")
                .code(),
            "invalid_approval"
        );
    }

    #[test]
    fn a_gate_needs_a_message_and_a_bounded_lifetime() {
        assert!(params_from(&json!({ "message": "   " })).is_err());
        assert!(params_from(&json!({ "message": 7 })).is_err());
        assert!(
            params_from(&json!({ "message": "m".repeat(501) })).is_err(),
            "the message is bounded"
        );
        assert!(params_from(&json!({ "message": "m".repeat(500) })).is_ok());

        assert!(params_from(&json!({ "expires_in_hours": 0 })).is_err());
        assert!(params_from(&json!({ "expires_in_hours": -1 })).is_err());
        assert!(params_from(&json!({ "expires_in_hours": MAX_TTL_HOURS + 1 })).is_err());
        assert!(params_from(&json!({ "expires_in_hours": "soon" })).is_err());
        assert!(params_from(&json!({ "expires_in_hours": 1.5 })).is_err());
        assert_eq!(
            params_from(&json!({ "expires_in_hours": 4 }))
                .expect("bounded")
                .expires_in_hours,
            4
        );
    }

    #[test]
    fn the_deadline_is_the_opening_instant_plus_the_lifetime() {
        let params = ApprovalParams {
            expires_in_hours: 6,
            ..ApprovalParams::default()
        };
        let opened = OffsetDateTime::UNIX_EPOCH;
        let deadline = approval_deadline(&params, opened);
        assert_eq!(deadline, opened + Duration::hours(6));
        assert!(
            deadline > opened,
            "a gate that is already dead is refused at write time"
        );
    }

    #[test]
    fn an_expired_gate_is_one_nobody_decided_and_the_clock_passed() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let row = ApprovalRow {
            id: Uuid::nil(),
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            organization_id: Uuid::nil(),
            rule_id: None,
            requested_at: now,
            expires_at: now + Duration::hours(1),
            decision: None,
            decided_by: None,
            decided_at: None,
            note: None,
            step_name: "gate".to_owned(),
            params: serde_json::json!({}),
            rule_name: None,
        };

        assert!(!row.is_expired(now), "an hour is left");
        // The boundary is `<=`, not `<`: a gate whose deadline is exactly now has stopped
        // accepting decisions, because the write that decides it is guarded the same way.
        // A gate one second *before* its deadline is still open — that is the second the
        // decider's click has to beat.
        assert!(
            !row.is_expired(now + Duration::hours(1) - Duration::seconds(1)),
            "one second before the deadline is still open"
        );
        assert!(
            row.is_expired(now + Duration::hours(1)),
            "on the deadline is not"
        );
        assert!(row.is_expired(now + Duration::days(9)));
        // A gate somebody already decided is not "expired" whatever the clock says: it is
        // decided, and the panel shows that instead of offering a button.
        let decided = ApprovalRow {
            decision: Some("rejected".to_owned()),
            ..row.clone()
        };
        assert!(!decided.is_expired(now + Duration::days(9)));
    }

    #[test]
    fn a_gate_row_reads_its_parameters_the_way_the_engine_wrote_them() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let base = ApprovalRow {
            id: Uuid::nil(),
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            organization_id: Uuid::nil(),
            rule_id: None,
            requested_at: now,
            expires_at: now,
            decision: None,
            decided_by: None,
            decided_at: None,
            note: None,
            step_name: "gate".to_owned(),
            params: serde_json::json!({
                "permission": "content.pages.publish",
                "message": "publish this?",
                "expires_in_hours": 4,
            }),
            rule_name: None,
        };

        let params = base.params();
        assert_eq!(params.permission, "content.pages.publish");
        assert_eq!(params.message, "publish this?");
        assert_eq!(params.expires_in_hours, 4);

        // A row whose parameters drifted (a definition edited under an open gate) falls back
        // to the platform's own values rather than failing the list it appears in.
        let drifted = ApprovalRow {
            params: serde_json::json!({ "permission": "!!!" }),
            ..base
        };
        assert_eq!(drifted.params().permission, APPROVAL_PERMISSION);
    }

    #[test]
    fn the_two_decisions_round_trip_and_nothing_else_does() {
        for decision in [Decision::Approved, Decision::Rejected] {
            assert_eq!(Decision::parse(decision.as_str()), Some(decision));
        }
        assert_eq!(Decision::parse("approved"), Some(Decision::Approved));
        assert_eq!(Decision::parse("rejected"), Some(Decision::Rejected));
        // Notably not "pending" — an undecided gate has no decision value at all, which is
        // what makes `decision is null` the single predicate the whole module leans on.
        assert_eq!(Decision::parse("pending"), None);
        assert_eq!(Decision::parse(""), None);
    }
}
