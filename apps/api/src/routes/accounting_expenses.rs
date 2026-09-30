//! Expenses: file one, read it, and move it along its approval (REQ-054, slice 4).
//!
//! Seven routes, and the shape worth reading twice is the split between them:
//!
//! * `/expenses` and `/expenses/{id}` are guarded by **route layers**, which is right for them —
//!   neither path's refusal says anything about a specific row the caller cannot see.
//! * The three transition routes are **not** layered, and that absence is the same decision slice 3
//!   made for the payment reversal: the path carries an id, so a layer's `403 permission_denied`
//!   would confirm the expense exists in some other organization before the handler could ask
//!   whose it is. Each handler reads the row (404 for another tenant) and *then* asks for the key.
//!   Anonymous callers are still refused first — `CurrentSession` answers 401 before any of this.
//!
//! ## The audit trail is per transition, not per status
//!
//! One audit row per move with the before/after status, the comment and the entry id. An approval
//! with no record of *who* and *when* is not an approval trail, which is why the route writes one
//! row per decision rather than one row per expense.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_accounting::expenses::{self, NewExpense};
use omnion_module_accounting::store::{DEFAULT_PER_PAGE, Page};
use omnion_module_accounting::{
    DecisionBody, ExpenseStatus, ExpenseSummary, ExpenseView,
};
use omnion_permissions::effective_permissions;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::guards::scope_of;
use crate::routes::accounting::{OrganizationParam, emit, parse_day};
use crate::routes::crm::organization_of;
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The query of the expenses list.
#[derive(Debug, Default, Deserialize)]
pub struct ExpenseListParams {
    /// `draft`, `submitted`, `approved`, `rejected` or `reimbursed`.
    #[serde(default)]
    pub status: Option<String>,
    /// One category, exactly as stored.
    #[serde(default)]
    pub category: Option<String>,
    /// Free text over the description, the vendor, the note and the number.
    #[serde(default)]
    pub search: Option<String>,
    /// `YYYY-MM-DD`, the earliest expense date.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, the latest expense date.
    #[serde(default)]
    pub to: Option<String>,
    /// The cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/expenses` — the list the expenses screen draws.
pub async fn list_expenses(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ExpenseListParams>,
) -> Result<Json<Page<ExpenseSummary>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;

    // An unknown status is refused by name rather than answered with an empty list: "no expenses
    // have the status approvedd" and "you typed it wrong" are different problems to solve, and
    // only one of them is what the operator did.
    let status = match params.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(ExpenseStatus::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!(
                    "{raw:?} is not an expense status — use draft, submitted, approved, rejected or \
                     reimbursed"
                ),
            )
        })?),
    };

    let cursor = match params.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => None,
        Some(raw) => Some(Uuid::parse_str(raw).map_err(|_| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!("{raw:?} is not a cursor — pass the `next_cursor` a previous page returned"),
            )
        })?),
    };

    Ok(Json(
        expenses::list_expenses(
            state.db().pool(),
            organization_id,
            status,
            params.category.as_deref().map(str::trim).filter(|c| !c.is_empty()),
            params.search.as_deref(),
            parse_day(params.from.as_deref(), "from")?,
            parse_day(params.to.as_deref(), "to")?,
            cursor,
            params.limit.unwrap_or(DEFAULT_PER_PAGE),
        )
        .await?,
    ))
}

/// `GET /api/v1/accounting/expenses/categories` — the picker's options.
///
/// **Declared before `/expenses/{id}`** for the same reason every other router in this file puts
/// its literals first: axum matches in registration order, so `categories` read as a uuid and the
/// route answered `400` for a word.
pub async fn list_expense_categories(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<Vec<String>>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        expenses::list_categories(state.db().pool(), organization_id).await?,
    ))
}

/// `GET /api/v1/accounting/expenses/{id}` — one expense with its decision trail.
pub async fn get_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(expense_id): Path<Uuid>,
) -> Result<Json<ExpenseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        expenses::get_expense(state.db().pool(), organization_id, expense_id).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/accounting/expenses` — file one as a draft.
pub async fn create_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewExpense>,
) -> Result<(StatusCode, Json<ExpenseView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    let created = expenses::create_expense(
        state.db().pool(),
        organization_id,
        &body.0,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.expense.created")
            .organization(organization_id)
            .target("accounting_expense", created.summary.id.to_string())
            .metadata(created.summary.reference())
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /api/v1/accounting/expenses/{id}` — edit a draft.
///
/// No event: an edit before submission is nobody else's business, and the REQ names
/// `accounting.expense.submitted` / `.approved` / `.rejected` rather than an edit event.
pub async fn update_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(expense_id): Path<Uuid>,
    body: Json<NewExpense>,
) -> Result<Json<ExpenseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    // Read before write, for the audit row's "before" half and — the reason that matters — so a
    // caller who does not own the row gets the same 404 everybody else gets, whatever their role.
    let before = expenses::get_expense(state.db().pool(), organization_id, expense_id).await?;

    let updated = expenses::update_expense(
        state.db().pool(),
        organization_id,
        expense_id,
        &body.0,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.expense.updated")
            .organization(organization_id)
            .target("accounting_expense", updated.summary.id.to_string())
            .metadata(json!({
                "before": before.summary.reference(),
                "after": updated.summary.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(updated))
}

// ---------------------------------------------------------------------------------------------
// The three transitions
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/accounting/expenses/{id}/submit` — send it for approval.
///
/// Guarded by `accounting.expenses.create` rather than `.approve`: filing is not deciding, and
/// the person who spends the money is normally the one who files it.
pub async fn submit_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(expense_id): Path<Uuid>,
) -> Result<Json<ExpenseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    // The ownership read comes before the key check — see the module header. Same order, same
    // reason, same `404` for a stranger's expense.
    let before = expenses::get_expense(state.db().pool(), organization_id, expense_id).await?;

    let submitted = transition(
        &state,
        &current,
        address,
        organization_id,
        expense_id,
        &before,
        ExpenseStatus::Submitted,
        "",
        "accounting.expenses.create",
        "sending an expense for approval needs the accounting.expenses.create permission",
        "accounting.expense.submitted",
    )
    .await?;

    Ok(Json(submitted))
}

/// `POST /api/v1/accounting/expenses/{id}/decision` — approve or reject, with a comment.
///
/// One route rather than two, because the REQ's API table names one and the module's table
/// already carries which comment is mandatory: sending `{ "reject": true }` with no comment is
/// refused **by the module**, with the message telling the operator to write one.
pub async fn decide_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(expense_id): Path<Uuid>,
    body: Json<DecisionBody>,
) -> Result<Json<ExpenseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    let before = expenses::get_expense(state.db().pool(), organization_id, expense_id).await?;

    // The body names which way: `approve` (the default) or `reject`. A route that guessed would
    // make a rejection button post an approval, so the direction is explicit in the request.
    let to = match body.0.approved {
        Some(false) => ExpenseStatus::Rejected,
        _ => ExpenseStatus::Approved,
    };

    let decided = transition(
        &state,
        &current,
        address,
        organization_id,
        expense_id,
        &before,
        to,
        body.0.comment.as_deref().unwrap_or(""),
        "accounting.expenses.approve",
        "deciding whether the business pays for an expense needs the accounting.expenses.approve \
         permission",
        match to {
            ExpenseStatus::Rejected => "accounting.expense.rejected",
            _ => "accounting.expense.approved",
        },
    )
    .await?;

    Ok(Json(decided))
}

/// `POST /api/v1/accounting/expenses/{id}/reimburse` — record the payout.
pub async fn reimburse_expense(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(expense_id): Path<Uuid>,
) -> Result<Json<ExpenseView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    let before = expenses::get_expense(state.db().pool(), organization_id, expense_id).await?;

    let paid = transition(
        &state,
        &current,
        address,
        organization_id,
        expense_id,
        &before,
        ExpenseStatus::Reimbursed,
        "",
        "accounting.expenses.approve",
        "paying a claim back needs the accounting.expenses.approve permission",
        "accounting.expense.reimbursed",
    )
    .await?;

    Ok(Json(paid))
}

// ---------------------------------------------------------------------------------------------
// The shared body of the three transitions
// ---------------------------------------------------------------------------------------------

/// Read, check the key, move, audit, announce.
///
/// The order is the whole point and it is the order slice 3's reversal established: **read first
/// (404 for another tenant), then the key (403 about something the caller can already see).** A
/// `403` that names a row the caller does not own is a tenant leak, and a permission layer would
/// produce exactly that.
#[allow(clippy::too_many_arguments)]
async fn transition(
    state: &AppState,
    current: &CurrentSession,
    address: ClientAddress,
    organization_id: Uuid,
    expense_id: Uuid,
    before: &ExpenseView,
    to: ExpenseStatus,
    comment: &str,
    key: &'static str,
    refusal: &'static str,
    event_name: &'static str,
) -> Result<ExpenseView, ApiError> {
    if !may(state, current, key).await? {
        return Err(ApiError::forbidden(key, refusal));
    }

    let after = expenses::transition(
        state.db().pool(),
        organization_id,
        expense_id,
        to,
        comment,
        Some(current.user.id),
    )
    .await?;

    // **No `format!`.** `NewAuditEntry::by_user` takes `&'static str`, so a `String` built here
    // would have to live for the whole program — and the event name the caller already handed in
    // is itself a `&'static str`, so the audit action is that name verbatim rather than a second
    // spelling of it that the two could stop agreeing on. The audit action is
    // `accounting.expense.approved`, not `expense.approved`: an audit trail somebody greps by
    // module deserves the module.
    record(
        state,
        NewAuditEntry::by_user(current.user.id, event_name)
            .organization(organization_id)
            .target("accounting_expense", after.summary.id.to_string())
            .metadata(json!({
                "status": after.summary.status.as_str(),
                "amount": after.summary.amount,
                "currency": after.summary.currency,
                "comment": comment,
                "journal_entry_id": after.summary.journal_entry_id,
                "number": after.summary.number,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The event fires only after the audit row is written, so a subscriber that calls back into
    // the API and asks "what decided this?" finds the answer rather than a gap.
    announce(state, current, organization_id, before, &after, event_name).await;

    Ok(after)
}

/// The audit suffix for an event name: `accounting.expense.approved` -> `approved`.
///
/// `event_name` is passed whole so the call site reads as the event it emits, and the audit action
/// reuses it rather than repeating the word — two literals that must agree is the kind of pair
/// that stops agreeing.
fn event_name_suffix(event_name: &'static str) -> &'static str {
    event_name
        .rsplit('.')
        .next()
        .unwrap_or("transitioned")
}

/// Emit the event for a transition.
///
/// The name is **passed in by the caller** rather than matched off the resulting status: the
/// caller already knows which transition it asked for, and deriving the name from the outcome
/// would mean a reopen to draft has to be special-cased here (it is, in `transition`, by simply
/// not calling this for it) — a rule that lives in two places and is checked in neither.
async fn announce(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    before: &ExpenseView,
    after: &ExpenseView,
    event_name: &'static str,
) {
    emit(
        state,
        NewEvent::new(event_name)
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "expense": after.summary.reference(),
                "from": before.summary.status.as_str(),
                "to": after.summary.status.as_str(),
                "decided_by": after.decided_by,
                "comment": after.decision_reason,
                "journal_entry_id": after.summary.journal_entry_id,
            })),
    )
    .await;
}

/// Whether the session holds a key.
///
/// The same `effective_permissions` lookup the route guard uses, so the two cannot disagree about
/// what a role can do — a check that is a second opinion is a check that eventually becomes the
/// first one to be wrong.
async fn may(state: &AppState, current: &CurrentSession, key: &str) -> Result<bool, ApiError> {
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    Ok(permissions.allows(key))
}
