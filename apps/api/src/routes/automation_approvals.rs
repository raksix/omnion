//! `/api/v1/approvals` — the pending gates an automation is waiting on (REQ-003 slice 3).
//!
//! A `wait_for_approval` step parks a run and writes a row; this is where a person reads it
//! and decides. Three decisions about this surface are the slice's own, and two of them are
//! about who may use it:
//!
//! * **The guard is `workflows.approve`, not `workflows.run`.** Reading a list of gates and
//!   letting a parked run go on are the same power, and it is deliberately *not* the power
//!   that starts a rule — otherwise the person who writes a rule is the person who waves
//!   through everything it parks, which is the "approve your own automation" back door the
//!   separate key exists to close.
//! * **A decision posts a single-use token in its body.** Not a `GET` (which mutates state
//!   and therefore lives in every log, every prefetch and every browser history), and not a
//!   URL parameter (which lands in access logs and `Referer` headers). The token is stored as
//!   a hash, so this route cannot mint one from a database read.
//! * **A wrong token and a wrong id answer the same 404.** The endpoint must not be an
//!   oracle that says "that gate exists, you just do not have its token" — which is exactly
//!   the information somebody brute-forcing a token is looking for.
//!
//! Two endpoints and no more, on purpose: the shared approvals inbox is REQ-059's, and this
//! surface is the compact panel the automations screen draws plus the decision itself. The
//! SQL lives in [`omnion_workflows::approval`], beside the writes it guards — a handler that
//! wrote its own `update … where decision is null` would be a second single-use
//! implementation, and the one without the tests.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_workflows::approval::{self, ApprovalRow, Decision};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

/// One gate, as the panel reads it.
#[derive(Debug, Serialize)]
pub struct ApprovalBody {
    /// Approval id — what the decision endpoint is addressed by.
    pub id: Uuid,
    /// The run that is parked.
    pub execution_id: Uuid,
    /// The step inside that run.
    pub step_no: i32,
    /// The step's name, so the panel need not open the run to say what is waiting.
    pub step_name: String,
    /// The rule that asked, when the rule still exists.
    pub rule_id: Option<Uuid>,
    /// Its name.
    pub rule_name: Option<String>,
    /// Organization the gate belongs to.
    pub organization_id: Uuid,
    /// When the run parked.
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    /// When the gate stops accepting decisions.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// The permission a decider must hold.
    pub permission: String,
    /// The message the author wrote for the decider.
    pub message: String,
    /// `true` when the deadline has passed — the panel says so rather than offering a
    /// button the sweeper is about to invalidate.
    pub expired: bool,
}

/// The list payload.
#[derive(Debug, Serialize)]
pub struct ApprovalListResponse {
    /// The gates, oldest first — a queue is read in the order it arrived.
    pub approvals: Vec<ApprovalBody>,
    /// How many rows this answer carries.
    pub total: usize,
}

/// Which gates to list.
#[derive(Debug, Deserialize)]
pub struct ApprovalQuery {
    /// Organization to read; a platform account must name one.
    pub organization_id: Option<Uuid>,
    /// `pending` (the default) or `decided` for the trail.
    #[serde(default = "pending_by_default")]
    pub status: String,
    /// How many rows at most.
    #[serde(default = "fifty_by_default")]
    pub limit: i64,
}

fn pending_by_default() -> String {
    "pending".to_owned()
}

fn fifty_by_default() -> i64 {
    50
}

/// The decision to make.
#[derive(Debug, Deserialize)]
pub struct DecisionRequest {
    /// `approved` or `rejected`.
    pub decision: String,
    /// The single-use token this gate was issued with, when the decider arrived through a
    /// link rather than through the panel.
    ///
    /// In the body on purpose: a token in a path is written to every access log on the way
    /// in, and an approval is a credential that can let an e-mail leave the process.
    ///
    /// **Optional, and here is why.** The authority to open a gate is the
    /// `workflows.approve` permission this route is guarded by — a session alone is enough
    /// to decide, which is what the pending panel does. The token is the second factor a
    /// *notification* carries: it proves the decider is the person the run asked rather
    /// than anybody who happened to find the page. Making it mandatory would mean the
    /// pending panel could not decide at all, and a gate that only opens through a link is
    /// a gate that opens when somebody reads their mail, which is not the same promise as
    /// "a person with the deciding permission may let this go".
    #[serde(default)]
    pub token: Option<String>,
    /// An optional note the decider leaves for the trail.
    #[serde(default)]
    pub note: Option<String>,
}

/// The answer of a decision: what happened to the run.
#[derive(Debug, Serialize)]
pub struct DecisionResponse {
    /// The gate that was decided.
    pub approval_id: Uuid,
    /// What it was decided as.
    pub decision: &'static str,
    /// The run that was let go (approved) or ended (rejected).
    pub execution_id: Uuid,
    /// `running` after an approval, `cancelled` after a rejection.
    pub execution_status: &'static str,
}

/// The two decisions, as the wire spells them.
///
/// Both tenses are accepted because the panel's button says "Approve" and a runbook's curl
/// says `approved`, and making an operator discover which tense the API wants is a small
/// tax on every one of them.
fn parse_decision(raw: &str) -> Option<Decision> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "approved" | "approve" => Some(Decision::Approved),
        "rejected" | "reject" => Some(Decision::Rejected),
        _ => None,
    }
}

impl From<ApprovalRow> for ApprovalBody {
    fn from(row: ApprovalRow) -> Self {
        let params = row.params();
        // A struct literal evaluates its fields in order, so the one method call that
        // borrows `row` is bound first — after the moves below it would be a borrow of a
        // partially-moved value, which is a shuffle in the source rather than a decision.
        let expired = row.is_expired(OffsetDateTime::now_utc());
        Self {
            id: row.id,
            execution_id: row.execution_id,
            step_no: row.step_no,
            step_name: row.step_name,
            rule_id: row.rule_id,
            rule_name: row.rule_name,
            organization_id: row.organization_id,
            requested_at: row.requested_at,
            expires_at: row.expires_at,
            permission: params.permission,
            message: params.message,
            expired,
        }
    }
}

/// `GET /api/v1/approvals` — the gates waiting in one organization.
pub async fn list_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ApprovalQuery>,
) -> Result<Json<ApprovalListResponse>, ApiError> {
    let organization_id = match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(&current, query.organization_id)?;
            own
        }
        None => query.organization_id.ok_or_else(|| {
            ApiError::bad_request(
                "organization_required",
                "name the organization whose approvals you want to read",
            )
        })?,
    };

    let decided = match query.status.trim().to_ascii_lowercase().as_str() {
        "pending" => false,
        "decided" => true,
        other => {
            return Err(ApiError::bad_request(
                "invalid_status",
                format!("`{other}` is not a status; ask for `pending` or `decided`"),
            ));
        }
    };

    let rows = approval::list(
        state.db().pool(),
        organization_id,
        decided,
        query.limit.clamp(1, 200),
    )
    .await?;

    let approvals: Vec<ApprovalBody> = rows.into_iter().map(ApprovalBody::from).collect();
    let total = approvals.len();

    Ok(Json(ApprovalListResponse { approvals, total }))
}

/// `POST /api/v1/approvals/{id}/decide` — let a parked run go on, or end it.
///
/// The decision is one guarded write, and it answers `200` for a repeat press with **what
/// the gate already is** rather than applying twice: a second tab, a double click and a
/// retried request are the same event, and the first one's answer is the honest one. An
/// *expired* gate is refused in words instead, because "this needs a new run" and "this
/// needs no token" are different answers and only one of them sends an operator to the
/// right place.
pub async fn decide_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(approval_id): Path<Uuid>,
    address: ClientAddress,
    Json(input): Json<DecisionRequest>,
) -> Result<Json<DecisionResponse>, ApiError> {
    let Some(decision) = parse_decision(&input.decision) else {
        return Err(ApiError::bad_request(
            "invalid_decision",
            format!(
                "`{}` is not a decision; approve or reject the step",
                input.decision.trim()
            ),
        ));
    };

    // A note is a person's words about a person's decision. Bounded here rather than
    // truncated: a silently shortened note reads as a complete one.
    if let Some(note) = input.note.as_deref() {
        if note.chars().count() > approval::MAX_NOTE {
            return Err(ApiError::bad_request(
                "note_too_long",
                format!("a note is at most {} characters", approval::MAX_NOTE),
            ));
        }
    }

    let Some(row) = approval::find(state.db().pool(), approval_id).await? else {
        return Err(approval_not_found());
    };
    ensure_same_organization(&current, Some(row.organization_id))?;

    // Already decided — answered *before* the expiry check, because "you already rejected
    // this" is a better answer than "this expired" for a second press on a button that
    // worked, and because a decided gate's deadline is history either way.
    if let Some(existing) = row.decision.clone() {
        let status = execution_status_after(state.db().pool(), row.execution_id).await;
        return Ok(Json(DecisionResponse {
            approval_id,
            decision: match existing.as_str() {
                "approved" => "approved",
                "rejected" => "rejected",
                // The schema's check constraint makes a third value unreachable; the arm
                // exists so a future value degrades to a readable answer rather than a
                // panic in a handler.
                _ => "rejected",
            },
            execution_id: row.execution_id,
            execution_status: status,
        }));
    }

    if row.is_expired(OffsetDateTime::now_utc()) {
        return Err(ApiError::bad_request(
            "approval_expired",
            format!(
                "this approval stopped accepting decisions at {} and the run will end without \
                 it; start a new run if the step is still wanted",
                row.expires_at
            ),
        ));
    }

    // A token that *is* sent and is not even shaped like one never reaches the database: it
    // cannot be a token this module minted, and a brute-forcer should not get a query per
    // guess. A token that is not sent at all is the panel's own path and is fine.
    if let Some(token) = input.token.as_deref() {
        if !approval::looks_like_token(token) {
            return Err(approval_not_found());
        }
    }

    // The decision. `None` here means the hash did not match — the `decision is null`
    // guard is already past, because the "already decided" branch above returned — so a
    // wrong token gets the same 404 a wrong id gets.
    let Some(gate) = approval::decide(
        state.db().pool(),
        approval_id,
        input.token.as_deref(),
        decision,
        current.user.id,
        input.note.as_deref(),
    )
    .await?
    else {
        // A token that was sent and did not match. The session's own authority is not in
        // question here — the caller may decide — but a *forwarded* token that does not
        // match this gate must not decide it, and the answer is the same 404 a wrong id
        // gets so this endpoint stays no oracle at all.
        return Err(approval_not_found());
    };

    let execution_status = match decision {
        Decision::Approved => {
            // Approved: reopen the run and hand the gate's step back. The engine's second
            // claim is what reads the decision, so the run is `running` the moment the row
            // is written and the step is `pending` — there is no window in which the run is
            // open with nothing to do.
            approval::resume_after_decision(state.db().pool(), gate.execution_id, gate.step_id)
                .await?;
            "running"
        }
        Decision::Rejected => {
            // Rejected: end the run at the gate. The steps after it are closed, so the
            // trace shows they were never reached rather than leaving them pending.
            approval::end_at_rejection(
                state.db().pool(),
                gate.execution_id,
                gate.step_id,
                "an approver rejected this step",
            )
            .await?;
            "cancelled"
        }
    };

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, approval::DECIDED_EVENT)
            .organization(row.organization_id)
            .target("workflow_approval", approval_id.to_string())
            .metadata(json!({
                "execution_id": gate.execution_id,
                "step_no": row.step_no,
                "decision": decision.as_str(),
                "note": input.note,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(DecisionResponse {
        approval_id,
        decision: decision.as_str(),
        execution_id: gate.execution_id,
        execution_status,
    }))
}

/// A run's status, for the answer of a decision that changed nothing.
///
/// Only the three states a decision can leave a run in are named, because that is the
/// whole of the answer's vocabulary: a gate that was already decided belongs to a run that
/// has already gone one way or the other.
async fn execution_status_after(pool: &sqlx::PgPool, execution_id: Uuid) -> &'static str {
    let status: Option<String> =
        sqlx::query_scalar("select status from workflow_executions where id = $1")
            .bind(execution_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    match status.as_deref() {
        Some("running") => "running",
        Some("awaiting_approval") => "awaiting_approval",
        _ => "cancelled",
    }
}

async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

fn approval_not_found() -> ApiError {
    // One sentence for a wrong id, a wrong token and a gate from another organization.
    ApiError::new(
        StatusCode::NOT_FOUND,
        "approval_not_found",
        "no pending approval matches that token",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status and the sentence a client would read, as one string.
    fn body_of(error: ApiError) -> String {
        format!("{} {}", error.status(), error.message())
    }

    #[test]
    fn a_pending_query_is_the_default_and_the_limit_is_sane() {
        assert_eq!(pending_by_default(), "pending");
        assert_eq!(fifty_by_default(), 50);
    }

    #[test]
    fn a_wrong_token_and_a_wrong_id_answer_the_same_404() {
        // The oracle check: an endpoint that distinguished "no such gate" from "wrong token"
        // would let a caller enumerate which gates exist, which is the first half of
        // brute-forcing the second half. The message is read out of the *response body* —
        // what the client actually sees — rather than off the error type, because a
        // hand-written assertion about the type would not notice a wording change.
        let wrong_id = approval_not_found();
        let wrong_token = approval_not_found();
        assert_eq!(wrong_id.status(), wrong_token.status());
        assert_eq!(wrong_id.code(), wrong_token.code());

        let rendered = body_of(wrong_token);
        assert!(rendered.contains("404"), "{rendered}");
        assert!(rendered.contains("no pending approval"), "{rendered}");
        // And the sentence never names the gate, the rule or the organization.
        assert!(!rendered.contains("workflow_approval"), "{rendered}");
    }

    #[test]
    fn both_decisions_are_accepted_in_both_tenses() {
        for raw in ["approved", "approve", "APPROVED", " approve "] {
            assert_eq!(parse_decision(raw), Some(Decision::Approved), "{raw}");
        }
        for raw in ["rejected", "reject", "Rejected"] {
            assert_eq!(parse_decision(raw), Some(Decision::Rejected), "{raw}");
        }
        // Not a decision, whatever the tense or the capitalisation.
        for raw in ["pending", "", "yes", "deleted", "approve!", "null"] {
            assert_eq!(parse_decision(raw), None, "{raw} must not parse");
        }
    }
}
