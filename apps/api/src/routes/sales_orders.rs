//! `/api/v1/sales/orders` — the order chain's HTTP surface (docs/requests/REQ-052, slice 4).
//!
//! Like [`crate::routes::sales_quotes`] and [`crate::routes::sales_approvals`], this file is
//! deliberately thin: what an order may do to another order lives in
//! [`omnion_module_sales::orders`]. What this layer owns is the three things a module
//! deliberately does not know about:
//!
//! * **the permission** each call is behind. Confirming, cancelling and raising an invoice
//!   draft are all `sales.orders.confirm`, because each of them **commits the organization**:
//!   stock is held, stock is given back, or a document goes to accounting. Reading the order
//!   list is `sales.orders.read` and writing a hand-made draft is `sales.orders.create`, so a
//!   person who may look at the deliveries may not promise one.
//! * **the audit row and the event** for every transition, so "who promised this order" and
//!   "who gave the stock back" are answerable without reading the order's own history table.
//! * **the two events the spec names** — `sales.order.created` and `sales.order.confirmed` —
//!   which are what a webhook subscriber and an automation rule listen on. The quote's events
//!   shipped with slice 2; these are the second half of the chain.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_inventory::reservations::{self, ReservationAction};
use omnion_module_sales::orders::{
    self, CancelOrder, NewOrder, OrderQuery, OrderView,
};
use omnion_module_sales::SalesError;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::organization_of;
use crate::routes::iam::record;
use crate::state::AppState;

/// The `organization_id` a platform account may read another tenant's data through.
#[derive(Debug, Deserialize)]
pub struct OrganizationParam {
    /// Organization to read.
    pub organization_id: Option<Uuid>,
}

/// The list query, as the route receives it.
#[derive(Debug, Default, Deserialize)]
pub struct OrderListParams {
    /// Free text over the number, the customer and the quote number.
    #[serde(default)]
    pub search: Option<String>,
    /// Comma-separated statuses; `all` for every one.
    #[serde(default)]
    pub status: Option<String>,
    /// One owner.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// `YYYY-MM-DD`, inclusive.
    #[serde(default)]
    pub from: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    #[serde(default)]
    pub to: Option<String>,
    /// `true` for live orders, `false` for archived.
    #[serde(default)]
    pub active: Option<bool>,
    /// Sort key: `created` (default), `number`, `total`, `status`.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl From<OrderListParams> for OrderQuery {
    fn from(params: OrderListParams) -> Self {
        Self {
            search: params.search,
            status: params.status,
            owner_user_id: params.owner_user_id,
            from: params.from,
            to: params.to,
            active: params.active,
            sort: params.sort,
            direction: params.direction,
            limit: params.limit,
            cursor: params.cursor,
        }
    }
}

/// `GET /api/v1/sales/orders` — one page of the order list.
pub async fn list_orders(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<OrderListParams>,
) -> Result<Json<omnion_module_sales::Page<OrderView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let page = orders::list_orders(state.db().pool(), organization_id, &params.into()).await?;
    Ok(Json(page))
}

/// `GET /api/v1/sales/orders/{id}` — one order with its lines, holds, history and invoice draft.
pub async fn get_order(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<orders::OrderDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        orders::get_order(state.db().pool(), organization_id, order_id).await?,
    ))
}

/// `POST /api/v1/sales/orders` — convert a quote, or write an order by hand.
pub async fn create_order(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    axum::Json(input): axum::Json<NewOrder>,
) -> Result<(StatusCode, Json<orders::OrderDetail>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let settings = omnion_module_sales::store::get_settings(state.db().pool(), organization_id).await?;
    let detail = orders::create_order(
        state.db().pool(),
        organization_id,
        &settings,
        current.user.id,
        &input,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.order.create")
            .organization(organization_id)
            .target("sales_order", detail.order.id.to_string())
            .metadata(json!({
                "number": detail.order.number,
                "status": detail.order.status.as_str(),
                "customer": detail.order.customer.name,
                "currency": detail.order.currency,
                "grand_total": detail.order.grand_total,
                "quote_number": detail.order.quote_number,
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.order.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "order_id": detail.order.id,
                "number": detail.order.number,
                "quote_id": detail.order.quote_id,
                "customer": detail.order.customer.name,
                "currency": detail.order.currency,
                "grand_total": detail.order.grand_total,
                "owner_user_id": detail.order.owner.as_ref().map(|owner| owner.id),
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(detail)))
}

/// `POST /api/v1/sales/orders/{id}/confirm` — promise the order and hold its stock.
pub async fn confirm_order(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<orders::OrderDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let detail =
        orders::confirm_order(state.db().pool(), organization_id, order_id, current.user.id).await?;

    // The hold, on the shelf. This is deliberately **not** a subscriber to the event below: the
    // event is what a webhook or an automation rule reads, and both of those may be off, may be
    // slow, and may fail. A promise made in an order that no shelf knows about is the exact gap
    // this line closes, and it is the same argument the events section of REQ-053 makes about
    // `sales.order.confirmed` — consumed, but consumed *here*, where the document is committed
    // and a failure can still be reported to the person who pressed the button.
    let holds = reservations::reserve_for_order(
        state.db().pool(),
        organization_id,
        order_id,
        ReservationAction::Reserve,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.order.confirm")
            .organization(organization_id)
            .target("sales_order", order_id.to_string())
            .metadata(json!({
                "number": detail.order.number,
                "status": detail.order.status.as_str(),
                "reservation_state": detail.order.reservation_state.as_str(),
                "currency": detail.order.currency,
                "grand_total": detail.order.grand_total,
                "held": holds.movements.len(),
                "unheld_lines": holds.unheld_lines,
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.order.confirmed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "order_id": order_id,
                "number": detail.order.number,
                "currency": detail.order.currency,
                "grand_total": detail.order.grand_total,
                "reservation_state": detail.order.reservation_state.as_str(),
                // The reserved quantities, per the spec's payload list: a warehouse subscriber
                // needs the numbers, not the fact that something happened to them. These are the
                // numbers **actually written to the shelf**, which is what makes this payload worth
                // subscribing to — before the bridge, a subscriber that trusted them would be
                // reading a promise rather than a movement.
                "held_movements": holds.movements.len(),
                "unheld_lines": holds.unheld_lines,
                "lines": detail
                    .lines
                    .iter()
                    .map(|line| json!({
                        "product_id": line.product_id,
                        "quantity": line.quantity,
                        "unit": line.unit,
                        "reserved": line.reservation.is_some(),
                    }))
                    .collect::<Vec<_>>(),
            })),
    )
    .await;

    Ok(Json(detail))
}

/// `POST /api/v1/sales/orders/{id}/cancel` — withdraw the order and release its stock.
pub async fn cancel_order(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(order_id): Path<Uuid>,
    axum::Json(input): axum::Json<CancelOrder>,
) -> Result<Json<orders::OrderDetail>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let detail = orders::cancel_order(
        state.db().pool(),
        organization_id,
        order_id,
        current.user.id,
        &input,
    )
    .await?;

    // The stock goes back, through the same bridge that took it. Symmetry is the whole argument
    // for doing it here rather than leaving it to a subscriber: a release that is written by a
    // different code path from the reserve is a release that can drift, and "the cancel button
    // gave the stock back" is a promise made to a warehouse.
    let released = reservations::reserve_for_order(
        state.db().pool(),
        organization_id,
        order_id,
        ReservationAction::Release,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.order.cancel")
            .organization(organization_id)
            .target("sales_order", order_id.to_string())
            .metadata(json!({
                "number": detail.order.number,
                "status": detail.order.status.as_str(),
                "reason": input.reason,
                "reservation_state": detail.order.reservation_state.as_str(),
                "released_movements": released.movements.len(),
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.order.cancelled")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "order_id": order_id,
                "number": detail.order.number,
                "reason": input.reason,
                "reservation_state": detail.order.reservation_state.as_str(),
            })),
    )
    .await;

    Ok(Json(detail))
}

/// `POST /api/v1/sales/orders/{id}/invoice-draft` — hand the delivery to accounting.
pub async fn raise_invoice_draft(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<orders::InvoiceHandoffView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let handoff =
        orders::raise_invoice_draft(state.db().pool(), organization_id, order_id, current.user.id)
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.order.invoice_draft")
            .organization(organization_id)
            .target("sales_order", order_id.to_string())
            .metadata(json!({
                "handoff_id": handoff.id,
                "state": handoff.state,
                "currency": handoff.currency,
                "grand_total": handoff.grand_total,
            })),
    )
    .await?;

    // `200` rather than `201`: asking twice returns the draft that already exists, and a repeated
    // ask is not a second document.
    Ok(Json(handoff))
}

/// Record an event, logging a failure instead of failing the request.
///
/// The transition is **already committed** by the time this runs, so refusing the request here
/// would tell a seller their confirmation failed when it did not — the worst possible message to
/// give somebody about stock they believe is held. The event is a downstream fact; the order is
/// the promise.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the sales order event could not be recorded");
    }
}

/// Keeps the compiler honest about the error type this file maps.
const _: fn(SalesError) -> ApiError = ApiError::from;
