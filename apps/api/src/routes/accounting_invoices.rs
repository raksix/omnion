//! The invoice routes of slice 2 (docs/requests/REQ-054).
//!
//! Sliced out of `routes/accounting.rs` rather than appended to it for a reason worth stating:
//! that file was already 822 lines and its three subjects — the chart, the rates, the journal —
//! are three different arguments about money. Invoices are a **fourth** argument (a document with
//! a lifecycle), and appending them would have made one file where the reader has to work out
//! which of four rule sets applies before reading a line. The module boundary is the same
//! boundary the crate uses: `modules/accounting/src/invoices.rs` holds the arithmetic and the
//! state machine, this file holds the HTTP.
//!
//! What this layer owns, and it is the same three the sibling route file states:
//!
//! * the caller's organization comes from the CRM's [`organization_of`], so a screen opened on
//!   `/accounting/invoices` with no query string shows **its** records;
//! * a record of another organization is a `404`, never a `403` — a `403` confirms it exists,
//!   and one organization's receivables are the thing this module exists to keep apart;
//! * every mutation writes an audit row with the actor, what changed and the before/after.
//!
//! # The two events this file emits, and the one it deliberately does not
//!
//! `accounting.invoice.issued` and `accounting.invoice.voided` travel out of here, plus
//! `accounting.invoice.sent` for the send. `accounting.invoice.overdue` is **not** emitted by a
//! route at all: the sweep in the module flips the status and returns the ids it changed, and a
//! route is what announces them. That split is the whole idempotence design — a sweep that
//! announced from inside the UPDATE would fire once per racing scheduler.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_module_accounting::invoices::{self, InvoiceStatus, NewInvoice};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::accounting::{OrganizationParam, emit, parse_day};
use crate::routes::crm::organization_of;
use crate::routes::iam::record;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The query of the invoice list.
#[derive(Debug, Default, Deserialize)]
pub struct InvoiceListParams {
    /// `draft`, `sent`, `partial`, `paid`, `overdue` or `void`.
    #[serde(default)]
    pub status: Option<String>,
    /// `YYYY-MM-DD`, the earliest issue date.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, the latest issue date.
    #[serde(default)]
    pub to: Option<String>,
    /// Only what is past its due date. The list's own definition — `due_date < today` on a
    /// receivable — so the tab and the red date on the row can never disagree.
    #[serde(default)]
    pub overdue_only: Option<bool>,
    /// Free text over the number and the customer.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of `POST /accounting/invoices/{id}/void`.
#[derive(Debug, Default, Deserialize)]
pub struct VoidBody {
    /// Why the document is withdrawn. Required — see the route.
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/accounting/invoices` — the list the invoice screen draws.
pub async fn list_invoices(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<InvoiceListParams>,
) -> Result<Json<Vec<invoices::InvoiceSummary>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;

    let status = match params.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(InvoiceStatus::parse(raw).ok_or_else(|| {
            // The names in the refusal are the six the schema's CHECK allows, so a caller who
            // misspells one is told what the right one looks like instead of getting an empty
            // list that reads like "no invoices match".
            ApiError::bad_request(
                "invalid_accounting_query",
                format!(
                    "{raw:?} is not an invoice status — use draft, sent, partial, paid, \
                     overdue or void"
                ),
            )
        })?),
    };

    let rows = invoices::list_invoices(
        state.db().pool(),
        organization_id,
        status,
        parse_day(params.from.as_deref(), "from")?,
        parse_day(params.to.as_deref(), "to")?,
        params.overdue_only.unwrap_or(false),
        params.search.as_deref(),
        params.limit,
    )
    .await?;

    Ok(Json(rows))
}

/// `GET /api/v1/accounting/invoices/{id}` — one invoice with its lines.
pub async fn get_invoice(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<invoices::InvoiceView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        invoices::get_invoice(state.db().pool(), organization_id, invoice_id).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/accounting/invoices` — a manual invoice, or one converted from a sales order.
///
/// The two are one route on purpose. An invoice converted from an order is not a different kind
/// of document with different rules; it is the same document whose lines were copied, and a
/// second endpoint would be a second implementation of the same invariants.
pub async fn create_invoice(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    body: Json<NewInvoice>,
) -> Result<(StatusCode, Json<invoices::InvoiceView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let created =
        invoices::create_invoice(state.db().pool(), organization_id, &body.0, Some(current.user.id))
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.invoice.created")
            .organization(organization_id)
            .target("accounting_invoice", created.id.to_string())
            .metadata(json!({
                "invoice_id": created.id,
                "order_id": created.order_id,
                "after": created.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // A draft is not an issued invoice, so `accounting.invoice.issued` is **not** emitted here —
    // that name is the trigger a finance automation subscribes to, and firing it for a document
    // nobody has seen would fire it for every keystroke of a draft. The event fires on send.
    emit(
        &state,
        NewEvent::new("accounting.invoice.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(created.reference()),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// `POST /api/v1/accounting/invoices/{id}/send` — issue the document to the customer.
pub async fn send_invoice(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<invoices::InvoiceView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = invoices::get_invoice(pool, organization_id, invoice_id).await?;
    let sent = invoices::send_invoice(pool, organization_id, invoice_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.invoice.sent")
            .organization(organization_id)
            .target("accounting_invoice", sent.id.to_string())
            .metadata(json!({
                "invoice_id": sent.id,
                "number": sent.number,
                "before": before.reference(),
                "after": sent.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The name a finance system subscribes to, and the one the REQ's webhook section names as
    // the trigger for "send the invoice, then chase it". The payload carries the amounts and the
    // due date, which is all an automation needs to decide whether to follow up.
    emit(
        &state,
        NewEvent::new("accounting.invoice.issued")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "invoice_id": sent.id,
                "number": sent.number,
                "currency": sent.currency,
                "grand_total": sent.grand_total,
                "due_date": sent.due_date.map(|d| omnion_module_accounting::dates::to_wire(&d)),
                "customer_name": sent.customer_name,
            })),
    )
    .await;
    emit(
        &state,
        NewEvent::new("accounting.invoice.sent")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(sent.reference()),
    )
    .await;

    Ok(Json(sent))
}

/// `POST /api/v1/accounting/invoices/{id}/void` — withdraw the document, keeping its number.
pub async fn void_invoice(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(organization): Query<OrganizationParam>,
    Path(invoice_id): Path<Uuid>,
    body: Json<VoidBody>,
) -> Result<Json<invoices::InvoiceView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let before = invoices::get_invoice(pool, organization_id, invoice_id).await?;
    let voided = invoices::void_invoice(pool, organization_id, invoice_id, body.0.reason.as_deref().unwrap_or(""))
        .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "accounting.invoice.voided")
            .organization(organization_id)
            .target("accounting_invoice", voided.id.to_string())
            // The reason is in the audit metadata as well as on the row: an audit reader asks
            // "why was this withdrawn" in the audit trail, not by opening the document.
            .metadata(json!({
                "invoice_id": voided.id,
                "number": voided.number,
                "reason": voided.void_reason,
                "before": before.reference(),
                "after": voided.reference(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("accounting.invoice.voided")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "invoice_id": voided.id,
                "number": voided.number,
                "reason": voided.void_reason,
                "grand_total": voided.grand_total,
            })),
    )
    .await;

    Ok(Json(voided))
}

/// `POST /api/v1/accounting/invoices/sweep-overdue` — flip what is past its due date, once.
///
/// # Why this is a route and not a background job
///
/// The REQ asks for an "overdue sweep" and the platform has an automation family for exactly
/// this (wave 3 owns it), so the **trigger** belongs to an automation. What belongs here is the
/// statement: the sweep is a single idempotent UPDATE, and putting it behind a route means it can
/// be called by the scheduler, by a button on the invoice list ("Check now"), and by a test
/// without three different implementations of "which invoices are late".
///
/// The response is the count **and the ids**, because the caller announcing events needs to know
/// which rows changed — announcing inside the UPDATE would fire once per racing scheduler.
pub async fn sweep_overdue_invoices(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
) -> Result<Json<SweepReport>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let flipped = invoices::sweep_overdue(state.db().pool(), organization_id).await?;

    for invoice_id in &flipped {
        let invoice = invoices::get_invoice(state.db().pool(), organization_id, *invoice_id).await?;
        // The event the REQ names as the trigger for the documented automation. `days_past_due`
        // travels because the automation's own rule is "wait 3 days then chase", and it cannot
        // compute that from the payload's due date without a second request.
        emit(
            &state,
            NewEvent::new("accounting.invoice.overdue")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "invoice_id": invoice.id,
                    "number": invoice.number,
                    "customer_name": invoice.customer_name,
                    "currency": invoice.currency,
                    "outstanding": invoice.outstanding,
                    "days_past_due": invoice.days_past_due,
                })),
        )
        .await;
    }

    Ok(Json(SweepReport {
        flipped: flipped.len(),
        invoice_ids: flipped,
    }))
}

/// What one sweep changed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepReport {
    /// How many invoices this call flipped.
    pub flipped: usize,
    /// Which ones — the same set that was announced, so a caller can verify.
    pub invoice_ids: Vec<Uuid>,
}
