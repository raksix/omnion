//! `/api/v1/iam/approvals` and `/api/v1/iam/requests` — permission requests and the window an
//! approval grants (docs/07-IAM.md §16; REQ-006, slice 4b).
//!
//! The split is deliberate:
//!
//! * **asking** (`POST /iam/requests`) needs nothing but a signed-in session — a person who
//!   cannot do something is exactly the person who has to be able to ask for it, and a
//!   permission for asking would lock the door it should open;
//! * **reading the inbox** needs `iam.approvals.read` and **deciding** needs
//!   `iam.approvals.decide`.
//!
//! An approval is a real time-boxed binding (see `omnion_permissions::approvals`): the resolver
//! stops counting it when the window ends, and the request then reads `expired` — no sweeper
//! process, no flag to forget.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_permissions::approvals::{
    self, ApprovalDecision, NewPermissionRequest, PermissionRequest, RequestFilter,
};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Query of the inbox list.
#[derive(Debug, Deserialize)]
pub struct ApprovalsQuery {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// `pending` (default), `approved`, `rejected`, `expired` or `all`.
    #[serde(default)]
    pub status: Option<String>,
}

/// The body of a decision.
#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    /// `approve` or `reject`.
    pub decision: String,
    /// Window to grant, in minutes (required for an approval).
    #[serde(default)]
    pub grant_minutes: Option<i32>,
    /// Note the approver leaves on the request.
    #[serde(default)]
    pub note: String,
}

/// The body of a new request.
#[derive(Debug, Deserialize)]
pub struct NewRequest {
    /// Exact permission key from the catalogue.
    pub permission_key: String,
    /// Why it is needed.
    #[serde(default)]
    pub justification: String,
    /// Optional resource kind (`path`).
    #[serde(default)]
    pub resource_type: Option<String>,
    /// Optional resource pattern (`/blog/*`).
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Organization to ask in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The stored request as the panel reads it.
fn request_json(record: &PermissionRequest) -> Value {
    let grant_active = record.grant_expires_at.is_some_and(|expires| {
        expires > time::OffsetDateTime::now_utc() && record.grant_revoked != Some(true)
    });

    json!({
        "id": record.id,
        "organization_id": record.organization_id,
        "permission_key": record.permission_key,
        "resource_type": record.resource_type,
        "resource_id": record.resource_id,
        "justification": record.justification,
        "status": record.status,
        "requester": {
            "id": record.requester_id,
            "email": record.requester_email,
            "name": record.requester_name,
        },
        "decided_by": record.decided_by.map(|id| json!({
            "id": id,
            "email": record.decider_email,
        })),
        "decided_at": record.decided_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        "decision_note": record.decision_note.clone().unwrap_or_default(),
        "grant_minutes": record.grant_minutes,
        "binding_id": record.binding_id,
        "grant_expires_at": record.grant_expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        "grant_active": grant_active,
        "created_at": record.created_at.format(&Rfc3339).unwrap_or_default(),
    })
}

/// Load one request and refuse it to an account from another organization.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    request_id: Uuid,
) -> Result<PermissionRequest, ApiError> {
    let record = approvals::find(state.db().pool(), request_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "request_not_found",
                "no such permission request",
            )
        })?;

    ensure_same_organization(current, Some(record.organization_id))?;
    Ok(record)
}

/// The organization's request inbox.
pub async fn list_approvals(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ApprovalsQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let filter = RequestFilter {
        status: Some(query.status.unwrap_or_else(|| "pending".to_owned())),
        requester_id: None,
    };

    let requests = approvals::list(state.db().pool(), organization_id, &filter).await?;

    // The counts behind the tabs always describe the whole inbox, whatever the filter shows.
    let all = approvals::list(
        state.db().pool(),
        organization_id,
        &RequestFilter {
            status: Some("all".to_owned()),
            requester_id: None,
        },
    )
    .await?;
    let mut counts = [
        ("pending", 0_i64),
        ("approved", 0),
        ("rejected", 0),
        ("expired", 0),
    ]
    .into_iter()
    .collect::<std::collections::BTreeMap<_, _>>();
    for request in &all {
        if let Some(counter) = counts.get_mut(request.status.as_str()) {
            *counter += 1;
        }
    }

    Ok(Json(json!({
        "organization_id": organization_id,
        "requests": requests.iter().map(request_json).collect::<Vec<_>>(),
        "counts": {
            "pending": counts.get("pending").copied().unwrap_or(0),
            "approved": counts.get("approved").copied().unwrap_or(0),
            "rejected": counts.get("rejected").copied().unwrap_or(0),
            "expired": counts.get("expired").copied().unwrap_or(0),
        },
    })))
}

/// Decide a request: approve with a window, or reject with a note.
pub async fn decide_approval(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(request_id): Path<Uuid>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<Value>, ApiError> {
    let before = load(&state, &current, request_id).await?;

    let approve = match body.decision.trim() {
        "approve" => true,
        "reject" => false,
        other => {
            return Err(ApiError::bad_request(
                "invalid_decision",
                format!("decision must be \"approve\" or \"reject\", not {other:?}"),
            ));
        }
    };

    let decision = ApprovalDecision {
        approve,
        grant_minutes: body.grant_minutes,
        note: body.note.clone(),
    };

    let after =
        approvals::decide(state.db().pool(), request_id, current.user.id, &decision).await?;

    let audit_action = if approve {
        "iam.approval.approved"
    } else {
        "iam.approval.rejected"
    };
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, audit_action)
            .target("permission_request", request_id.to_string())
            .metadata(json!({
                "permission_key": before.permission_key,
                "requester_id": before.requester_id,
                "grant_minutes": after.grant_minutes,
                "binding_id": after.binding_id,
                "status": after.status,
            }))
            .ip_address(address.as_text())
            .organization(Some(after.organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.approval_decided")
            .organization(Some(after.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "request_id": after.id,
                "permission_key": after.permission_key,
                "decision": if approve { "approved" } else { "rejected" },
                "grant_minutes": after.grant_minutes,
                "binding_id": after.binding_id,
            })),
    )
    .await;

    if after.binding_id.is_some() {
        emit(
            &state,
            NewEvent::new("iam.binding_created")
                .organization(Some(after.organization_id))
                .actor(Some(current.user.id))
                .payload(json!({
                    "binding_id": after.binding_id,
                    "subject_type": "user",
                    "subject_id": after.requester_id,
                    "via": "permission_request",
                    "request_id": after.id,
                    "expires_at": after.grant_expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
                })),
        )
        .await;
    }

    Ok(Json(request_json(&after)))
}

/// Ask for a permission. Any signed-in account may ask for itself.
pub async fn create_request(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewRequest>,
) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;

    let saved = approvals::request(
        state.db().pool(),
        organization_id,
        current.user.id,
        &NewPermissionRequest {
            permission_key: body.permission_key,
            resource_type: body.resource_type,
            resource_id: body.resource_id,
            justification: body.justification,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.approval.requested")
            .target("permission_request", saved.id.to_string())
            .metadata(json!({
                "permission_key": saved.permission_key,
                "resource_type": saved.resource_type,
                "resource_id": saved.resource_id,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.approval_requested")
            .organization(Some(organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "request_id": saved.id,
                "permission_key": saved.permission_key,
                "requester_id": current.user.id,
                "justification": saved.justification,
            })),
    )
    .await;

    Ok((axum::http::StatusCode::CREATED, Json(request_json(&saved))))
}

/// The caller's own requests — what a person without inbox access can still see.
pub async fn list_my_requests(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ApprovalsQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let filter = RequestFilter {
        status: query.status.or_else(|| Some("all".to_owned())),
        requester_id: Some(current.user.id),
    };

    let requests = approvals::list(state.db().pool(), organization_id, &filter).await?;

    Ok(Json(json!({
        "organization_id": organization_id,
        "requests": requests.iter().map(request_json).collect::<Vec<_>>(),
    })))
}
