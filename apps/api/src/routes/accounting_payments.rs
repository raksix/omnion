//! Payments: record one with its allocations, list them, reverse one (REQ-054, slice 3).
//!
//! Three routes, and the audit + event pairs are written here rather than in the module because
//! the module has no idea who the caller is — `record_payment` takes a `Uuid` it was handed, and
//! the audit row has to name the session that made the call.
//!
//! ## The override, and why it is not a query parameter
//!
//! `allow_overpayment` is a field on the body, but it is **not** taken at face value. The route
//! only lets it through when the session holds `accounting.payments.overpay`, and it says so in
//! the refusal when it does not. The alternative — a client that sends the flag and gets a 422 —
//! teaches an operator that the button is broken rather than that the role is not allowed, and a
//! button that is greyed for a reason the panel cannot explain is a dead button.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_accounting::payments::{self, NewPayment, PaymentMethod, PaymentView};
use omnion_module_accounting::store::{DEFAULT_PER_PAGE, Page};
use omnion_module_accounting::PaymentSummary;
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

/// The query of the payments list.
#[derive(Debug, Default, Deserialize)]
pub struct PaymentListParams {
    /// `bank_transfer`, `card`, `cash` or `other`.
    #[serde(default)]
    pub method: Option<String>,
    /// `YYYY-MM-DD`, the earliest payment date.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, the latest payment date.
    #[serde(default)]
    pub to: Option<String>,
    /// Free text over the number and the customer.
    #[serde(default)]
    pub search: Option<String>,
    /// Only what has not been reversed. The list's default view in practice — a reversed
    /// payment is history, and the operator asking for money that arrived wants the live rows.
    #[serde(default)]
    pub unreversed_only: Option<bool>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of `POST /accounting/payments/{id}/reverse`.
#[derive(Debug, Default, Deserialize)]
pub struct ReverseBody {
    /// Why the payment is being undone. Required — see the route.
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/payments` — the list the payments screen draws.
pub async fn list_payments(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<PaymentListParams>,
) -> Result<Json<Page<PaymentSummary>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;

    let method = match params.method.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        None => None,
        // The four names in the message: a misspelling that returned an empty list reads like
        // "no payments like that exist", which is a different problem to solve.
        Some(raw) => Some(PaymentMethod::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_accounting_query",
                format!("{raw:?} is not a payment method — use bank_transfer, card, cash or other"),
            )
        })?),
    };

    // `Page` is the module's own cursor shape, returned as-is rather than re-wrapped: the
    // invoice list answers with a bare array because it is an older route, and a list that
    // returns an array has no way to say "there is more" — which is why every module since
    // writes one page type and shares it.
    Ok(Json(
        payments::list_payments(
            state.db().pool(),
            organization_id,
            method,
            parse_day(params.from.as_deref(), "from")?,
            parse_day(params.to.as_deref(), "to")?,
            params.search.as_deref(),
            params.unreversed_only.unwrap_or(false),
            params.limit.unwrap_or(DEFAULT_PER_PAGE),
        )
        .await?,
    ))
}

/// `GET /api/v1/accounting/payments/{id}` — one payment with its allocations.
pub async fn get_payment(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(payment_id): Path<Uuid>,
) -> Result<Json<PaymentView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        payments::get_payment(state.db().pool(), organization_id, payment_id).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/accounting/payments` — record money received, applied to invoices.
pub async fn record_payment(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewPayment>,
) -> Result<(StatusCode, Json<PaymentView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;

    // The override is the caller's *request* and the session's *permission*, and the two are
    // reconciled here rather than passed down. A client that sends the flag without the key is
    // told which key is missing, because "422" on its own sends an operator to look at the
    // arithmetic instead of at their role.
    let mut payload = body.0.clone();
    if payload.allow_overpayment.unwrap_or(false) && !may_overpay(&state, &current).await? {
        return Err(ApiError::forbidden(
            "accounting.payments.overpay",
            "this payment allocates more than an invoice has outstanding; recording it needs the \
             accounting.payments.overpay permission",
        ));
    }

    let created =
        payments::record_payment(state.db().pool(), organization_id, &payload, Some(current.user.id))
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.payment.recorded")
            .organization(organization_id)
            .target("accounting_payment", created.summary.id.to_string())
            .metadata(json!({
                "payment_id": created.summary.id,
                "number": created.summary.number,
                "amount": created.summary.amount,
                "currency": created.summary.currency,
                "allocated": created.summary.allocated,
                "unallocated": created.summary.unallocated,
                "allocations": created.allocations.len(),
                "settled": payment_reference(&created),
                "journal_entry_id": created.summary.journal_entry_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The name a finance system subscribes to. It fires once per payment, carrying both the
    // money and what it settled, so an automation can "chase the rest" from the payload's
    // `unallocated` without a second request.
    emit(
        &state,
        NewEvent::new("accounting.payment.recorded")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(payment_reference(&created)),
    )
    .await;

    // One event per invoice this payment moved, and only for the ones it **closed**. A partial
    // payment fires `accounting.invoice.partially_paid`; the final one fires `paid`. The REQ
    // names both, and a subscriber that has to compare the payload's outstanding against the
    // invoice total to work out which happened will get it wrong on a full payment.
    for settled in &created.settled_invoices {
        let name = if settled.outstanding == "0.00" {
            "accounting.invoice.paid"
        } else {
            "accounting.invoice.partially_paid"
        };
        emit(
            &state,
            NewEvent::new(name)
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "invoice_id": settled.invoice_id,
                    "invoice_number": settled.invoice_number,
                    "status": settled.status_after.as_str(),
                    "outstanding": settled.outstanding,
                    "payment_id": created.summary.id,
                    "payment_number": created.summary.number,
                    "amount_applied": created
                        .allocations
                        .iter()
                        .find(|line| line.invoice_id == settled.invoice_id)
                        .map(|line| line.amount.clone()),
                })),
        )
        .await;
    }

    Ok((StatusCode::CREATED, Json(created)))
}

/// `POST /api/v1/accounting/payments/{id}/reverse` — undo a payment, keeping the record.
pub async fn reverse_payment(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(payment_id): Path<Uuid>,
    body: Json<ReverseBody>,
) -> Result<Json<PaymentView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();

    // **The ownership read comes before the permission check, and that order is the feature.**
    // `accounting.payments.reverse` is also enforced as a route layer, so this looks redundant —
    // it is not. A caller who lacks the key and names a payment in another organization would be
    // refused by the layer with `403 permission_denied`, and the difference between `403` and
    // `404` is the whole question: a `403` on a specific id confirms the row exists somewhere,
    // which is the one fact a tenant boundary must never leak. Reading the row first means a
    // caller without the key gets the same `404` everybody else gets for a payment that is not
    // theirs, and the permission is then checked against a row that is provably theirs — so the
    // `403` that survives is always about a payment in the caller's own organization.
    let before = payments::get_payment(pool, organization_id, payment_id).await?;

    if !may_reverse(&state, &current).await? {
        return Err(ApiError::forbidden(
            "accounting.payments.reverse",
            "undoing a payment releases its allocations and posts a counter entry; it needs the \
             accounting.payments.reverse permission",
        ));
    }

    let reversed = payments::reverse_payment(
        pool,
        organization_id,
        payment_id,
        body.0.reason.as_deref().unwrap_or(""),
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.payment.summary.reversed")
            .organization(organization_id)
            .target("accounting_payment", reversed.summary.id.to_string())
            .metadata(json!({
                "payment_id": reversed.summary.id,
                "number": reversed.summary.number,
                "reason": reversed.reversal_reason,
                "before": payment_reference(&before),
                "after": payment_reference(&reversed),
                "reversal_entry_id": reversed.reversal_entry_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.payment.summary.reversed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "payment_id": reversed.summary.id,
                "number": reversed.summary.number,
                "amount": reversed.summary.amount,
                "currency": reversed.summary.currency,
                "reason": reversed.reversal_reason,
                "reversal_entry_id": reversed.reversal_entry_id,
            })),
    )
    .await;

    Ok(Json(reversed))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Whether the session may record an allocation above an invoice's outstanding.
///
/// The same session lookup the permission guard does, asked as a question rather than as a
/// filter, because the answer changes what the caller is told. It is deliberately **not** the
/// `accounting.payments.record` key: overpaying is a different act from recording, and folding
/// it in would mean every bookkeeper can write money that does not correspond to an invoice.
async fn may_overpay(state: &AppState, current: &CurrentSession) -> Result<bool, ApiError> {
    // The same `effective_permissions` the guard itself resolves, so this question and the route
    // layer's cannot disagree about the same session — two implementations of "does this person
    // hold the key" is how a button is enabled for someone the route will refuse.
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    Ok(permissions.allows("accounting.payments.overpay"))
}

/// Whether the session may reverse a payment.
///
/// Asked as a question, and **after** the ownership read, for the reason spelled out at the call
/// site: the route layer already refuses a caller without the key, and the only thing this adds is
/// that the refusal happens *after* the tenant check. Without it, naming another organization's
/// payment without the key answers `403`, and `403` on a specific id is a statement that the id
/// exists. The same `effective_permissions` lookup as the guard keeps the two from disagreeing.
async fn may_reverse(state: &AppState, current: &CurrentSession) -> Result<bool, ApiError> {
    let permissions =
        effective_permissions(state.db().pool(), current.user.id, scope_of(&current.user)).await?;
    Ok(permissions.allows("accounting.payments.reverse"))
}

/// The compact payload an event and an audit row carry.
///
/// Small on purpose: it travels to every webhook subscriber, so it carries the money and the
/// invoices it touched — not the allocations' per-row arithmetic, which the id can be fetched for.
fn payment_reference(payment: &PaymentView) -> serde_json::Value {
    json!({
        "payment_id": payment.summary.id,
        "number": payment.summary.number,
        "customer_name": payment.summary.customer_name,
        "method": payment.summary.method.as_str(),
        "amount": payment.summary.amount,
        "currency": payment.summary.currency,
        "allocated": payment.summary.allocated,
        "unallocated": payment.summary.unallocated,
        "reversed": payment.summary.reversed,
        "invoices": payment
            .allocations
            .iter()
            .map(|line| json!({
                "invoice_id": line.invoice_id,
                "invoice_number": line.invoice_number,
                "amount": line.amount,
            }))
            .collect::<Vec<_>>(),
    })
}