//! `/api/v1/sales/approvals` — the discount gate's HTTP surface (docs/requests/REQ-052, slice 3).
//!
//! Like [`crate::routes::sales_quotes`], this file is deliberately thin: the rules about who may
//! decide what live in [`omnion_module_sales::approvals`]. What this layer owns is the three
//! things a module deliberately does not know about:
//!
//! * **the permission** each call is behind — deciding a request is `sales.quotes.send`, because
//!   approving is the last step of putting the organization's name on a document, and a role
//!   that may not send may not clear the gate that lets it be sent;
//! * **the audit row and the event** for every decision, so "who cleared this discount" is
//!   answerable without reading the quote table;
//! * **the two notifications**, which is the half of the spec that is not a rule: the manager
//!   learns a request exists, and the requester learns what was decided. Both go through
//!   `omnion_notifications`, and a failure to write one is logged rather than failed — refusing a
//!   decision because a notification could not be delivered would be trading a real answer for a
//!   cosmetic one.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_sales::approvals::{
    self, ApprovalDecision, ApprovalQuery, ApprovalRequest, ApprovalView,
};
use omnion_notifications::NewNotification;
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

/// The query of the approval inbox.
#[derive(Debug, Default, Deserialize)]
pub struct ApprovalListParams {
    /// `pending` (default), `requested_by_me`, `decided` or `all`.
    #[serde(default)]
    pub scope: Option<String>,
    /// Only this status.
    #[serde(default)]
    pub status: Option<String>,
    /// Free text over the quote number and title.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/sales/approvals` — the inbox, four ways.
pub async fn list_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ApprovalListParams>,
) -> Result<Json<omnion_module_sales::Page<ApprovalView>>, ApiError> {
    let organization_id = organization_of(&state, &current, params.organization_id).await?;
    let query = ApprovalQuery {
        scope: params.scope,
        status: params.status,
        search: params.search,
        limit: params.limit,
        viewer: current.user.id,
    };
    let page = approvals::list_approvals(state.db().pool(), organization_id, &query).await?;
    Ok(Json(page))
}

/// `GET /api/v1/sales/approvals/{id}` — one request, with its decision.
pub async fn get_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
) -> Result<Json<ApprovalView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        approvals::get_approval(state.db().pool(), organization_id, approval_id).await?,
    ))
}

/// `GET /api/v1/sales/quotes/{id}/approvals` — a quote's whole approval history.
pub async fn list_quote_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Json<Vec<ApprovalView>>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        approvals::list_for_quote(state.db().pool(), organization_id, quote_id).await?,
    ))
}

/// `GET /api/v1/sales/quotes/{id}/approval-requirement` — what the send button needs to say.
///
/// It is a `GET` and not a field on the quote because the answer changes with the **settings**,
/// not only with the quote: lowering the threshold can make a quote that was inside the limit
/// need a manager, and the builder must see that without saving anything.
pub async fn approval_requirement(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Json<Option<approvals::ApprovalRequired>>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    Ok(Json(
        approvals::requirement(state.db().pool(), organization_id, quote_id).await?,
    ))
}

/// `POST /api/v1/sales/quotes/{id}/approval-requests` — ask a manager.
pub async fn request_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
    body: Option<Json<ApprovalRequest>>,
) -> Result<(StatusCode, Json<ApprovalView>), ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let raised =
        approvals::request_approval(state.db().pool(), organization_id, quote_id, current.user.id, &body)
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.approval_requested")
            .organization(organization_id)
            .target("sales_quote_approval", raised.id.to_string())
            .metadata(json!({
                "request_id": raised.id,
                "quote_id": raised.quote_id,
                "quote_number": raised.quote_number,
                "discount_percent": raised.discount_percent,
                "threshold_percent": raised.threshold_percent,
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.approval_requested")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "approval_id": raised.id,
                "quote_id": raised.quote_id,
                "quote_number": raised.quote_number,
                "discount_percent": raised.discount_percent,
                "threshold_percent": raised.threshold_percent,
                "requested_by": raised.requested_by,
            })),
    )
    .await;

    // The manager is whoever owns the quote, and failing that whoever raised it — the inbox has
    // to reach *somebody*. A quote with no owner falls back to the seller's own notification,
    // which is wrong in a way a person can see and fix by assigning an owner, rather than
    // silence that looks like the gate is working.
    let recipient = owner_of_quote(&state, organization_id, raised.quote_id)
        .await
        .unwrap_or(raised.requested_by);
    notify(
        &state,
        organization_id,
        current.user.id,
        NewNotification::to(
            recipient,
            "approval",
            format!("{} needs approval", raised.quote_number),
        )
        .with_body(format!(
            "{} discounts its largest line {}%, over the {}% limit.",
            raised.quote_title, raised.discount_percent, raised.threshold_percent
        ))
        .with_url(raised.subject_url.clone())
        .with_source("sales_quote_approval", raised.id.to_string())
        .with_dedupe_key(format!("approval:{}:requested", raised.id)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(raised)))
}

/// `POST /api/v1/sales/approvals/{id}/decision` — approve or reject.
pub async fn decide_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
    Json(body): Json<ApprovalDecision>,
) -> Result<Json<ApprovalView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let decided = approvals::decide_approval(
        state.db().pool(),
        organization_id,
        approval_id,
        current.user.id,
        &body,
    )
    .await?;

    let approved = decided.status == omnion_module_sales::ApprovalStatus::Approved;
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.approval_decided")
            .organization(organization_id)
            .target("sales_quote_approval", decided.id.to_string())
            .metadata(json!({
                "request_id": decided.id,
                "quote_id": decided.quote_id,
                "quote_number": decided.quote_number,
                "decision": decided.status,
                "comment": decided.decision.as_ref().map(|d| d.comment.clone()),
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("sales.quote.approval_decided")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "approval_id": decided.id,
                "quote_id": decided.quote_id,
                "quote_number": decided.quote_number,
                "decision": decided.status,
                "requested_by": decided.requested_by,
            })),
    )
    .await;

    // The other side of the gate, told what happened. A rejection carries the comment, because
    // the comment is the only part of the decision the seller can act on.
    let (title, body) = if approved {
        (
            format!("{} was approved", decided.quote_number),
            "You can send it now.".to_string(),
        )
    } else {
        let why = decided
            .decision
            .as_ref()
            .map(|d| d.comment.clone())
            .unwrap_or_default();
        (
            format!("{} was rejected", decided.quote_number),
            format!("{why} — change the line and ask again."),
        )
    };
    notify(
        &state,
        organization_id,
        current.user.id,
        NewNotification::to(decided.requested_by, "approval", title)
            .with_body(body)
            .with_url(decided.subject_url.clone())
            .with_source("sales_quote_approval", decided.id.to_string())
            .with_dedupe_key(format!("approval:{}:decided", decided.id)),
    )
    .await;

    Ok(Json(decided))
}

/// `POST /api/v1/sales/approvals/{id}/cancel` — the requester withdraws their own request.
pub async fn cancel_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(approval_id): Path<Uuid>,
) -> Result<Json<ApprovalView>, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let cancelled = approvals::cancel_approval(
        state.db().pool(),
        organization_id,
        approval_id,
        current.user.id,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "sales.quote.approval_cancelled")
            .organization(organization_id)
            .target("sales_quote_approval", cancelled.id.to_string())
            .metadata(json!({
                "request_id": cancelled.id,
                "quote_id": cancelled.quote_id,
                "quote_number": cancelled.quote_number,
            })),
    )
    .await?;

    Ok(Json(cancelled))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the sales approval event could not be recorded");
    }
}

/// Write a notification, logging a failure instead of refusing the call.
///
/// The decision is already committed by the time this runs, so failing the request here would
/// tell the manager their approval did not happen when it did — the worst possible message to
/// give somebody about a document they just cleared.
async fn notify(
    state: &AppState,
    organization_id: Uuid,
    emitter: Uuid,
    notification: NewNotification,
) {
    let result = omnion_notifications::store::record(
        state.db().pool(),
        Some(organization_id),
        Some(emitter),
        &notification,
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "the sales approval notification could not be recorded");
    }
}

/// The quote's owner, or `None` when it has none.
async fn owner_of_quote(state: &AppState, organization_id: Uuid, quote_id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "select owner_user_id from sales_quotes where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(quote_id)
    .fetch_optional(state.db().pool())
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_module_sales::ApprovalStatus;

    #[test]
    fn every_approval_status_the_module_knows_has_a_label() {
        // A status the API can filter by and no tab can show is a status nobody can find.
        for status in [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Cancelled,
        ] {
            assert!(!status.label().is_empty(), "{status:?} has no label");
        }
    }

    #[test]
    fn the_inbox_reads_a_scope_the_panel_writes_and_a_book_would_write() {
        let module = omnion_module_sales::ApprovalScope::parse;
        for raw in ["pending", "requested_by_me", "decided", "all"] {
            assert!(module(raw).is_ok(), "{raw} should be a scope");
        }
    }
}
