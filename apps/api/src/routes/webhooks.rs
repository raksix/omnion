//! `/api/v1/webhooks` and `/api/v1/events` — the events and webhooks surface (phase P12).
//!
//! Three things live here, and nothing else:
//!
//! * **Endpoints** — where an organization wants its events delivered. Reading them is
//!   `webhooks.read`, connecting and changing them is `webhooks.manage`, and both are scoped:
//!   an organization account only ever sees and changes its own endpoints (`crate::scope`).
//!   The signing secret is write-only: the platform shows it once when it generated it, and it
//!   never comes back out of the API afterwards — a rotation replaces it silently.
//! * **Deliveries** — the queue's history for one endpoint (`GET /webhooks/{id}/deliveries`):
//!   what the receiver accepted, what is still waiting, and what ran out of attempts.
//! * **Events** — the bus itself (`GET /events`), newest first, so the platform's own record is
//!   readable next to the endpoints that received it.
//!
//! Producing the first event lives one module over: publishing a page
//! (`crate::routes::content`) records `page.published` and the bus queues one signed delivery
//! per subscribed endpoint of the organization. The delivery worker is
//! `crate::event_runner`; the wire format is documented in `omnion_events::signature` and
//! proven by `infra/mocks/webhook-receiver.mjs`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{
    EndpointChanges, NewEndpoint, NewEvent, WebhookEndpoint, bus, store, validation,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Deliveries and events one read may return when the caller does not say.
const DEFAULT_PAGE: i64 = 50;

/// Upper bound of one read (`?limit=`).
const MAX_PAGE: i64 = 200;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One endpoint as the panel sees it. The secret is never part of this shape — the only place
/// it travels is the creation response, and only when the platform generated it.
#[derive(Debug, Serialize)]
pub struct EndpointBody {
    /// Endpoint id.
    pub id: Uuid,
    /// Organization it belongs to.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// URL deliveries are POSTed to.
    pub url: String,
    /// Subscribed event names.
    pub events: Vec<String>,
    /// Whether the platform keeps delivering to it.
    pub enabled: bool,
    /// The generated secret, shown exactly once at creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// When it was connected.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl EndpointBody {
    /// Describe one endpoint without its secret.
    fn build(endpoint: &WebhookEndpoint) -> Self {
        Self {
            id: endpoint.id,
            organization_id: endpoint.organization_id,
            name: endpoint.name.clone(),
            url: endpoint.url.clone(),
            events: endpoint.events.clone(),
            enabled: endpoint.enabled,
            secret: None,
            created_at: endpoint.created_at,
            updated_at: endpoint.updated_at,
        }
    }

    /// Describe one endpoint, showing a secret the platform just generated.
    fn with_secret(endpoint: &WebhookEndpoint, secret: Option<String>) -> Self {
        Self {
            secret,
            ..Self::build(endpoint)
        }
    }
}

/// The endpoints this account may see.
#[derive(Debug, Serialize)]
pub struct EndpointsResponse {
    /// The endpoints, name order.
    pub webhooks: Vec<EndpointBody>,
}

/// One delivery as the panel sees it.
#[derive(Debug, Serialize)]
pub struct DeliveryBody {
    /// Delivery id (the value a receiver sees in `X-Omnion-Delivery`).
    pub id: Uuid,
    /// Event it carries.
    pub event_id: i64,
    /// Name of that event.
    pub event_name: String,
    /// `pending`, `delivered` or `failed`.
    pub status: String,
    /// Attempts made so far.
    pub attempts: i32,
    /// Attempts allowed.
    pub max_attempts: i32,
    /// When the next attempt is due.
    #[serde(with = "time::serde::rfc3339")]
    pub next_attempt_at: OffsetDateTime,
    /// Status the receiver answered with, when it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<i32>,
    /// Why the delivery failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the receiver accepted it.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub delivered_at: Option<OffsetDateTime>,
    /// When it was queued.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl DeliveryBody {
    /// Describe one delivery.
    fn build(delivery: &omnion_events::Delivery) -> Self {
        Self {
            id: delivery.id,
            event_id: delivery.event_id,
            event_name: delivery.event_name.clone(),
            status: delivery.status.clone(),
            attempts: delivery.attempts,
            max_attempts: delivery.max_attempts,
            next_attempt_at: delivery.next_attempt_at,
            response_status: delivery.response_status,
            error: delivery.error.clone(),
            delivered_at: delivery.delivered_at,
            created_at: delivery.created_at,
        }
    }
}

/// The deliveries of one endpoint.
#[derive(Debug, Serialize)]
pub struct DeliveriesResponse {
    /// The deliveries, newest first.
    pub deliveries: Vec<DeliveryBody>,
}

/// One recorded event as the panel sees it.
#[derive(Debug, Serialize)]
pub struct EventBody {
    /// Event id.
    pub id: i64,
    /// Event name.
    pub name: String,
    /// Organization the fact belongs to.
    pub organization_id: Option<Uuid>,
    /// Site the fact happened on.
    pub site_id: Option<Uuid>,
    /// Account that caused it.
    pub actor_user_id: Option<Uuid>,
    /// Structured detail.
    pub payload: serde_json::Value,
    /// When the platform recorded it.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The recent events this account may see.
#[derive(Debug, Serialize)]
pub struct EventsResponse {
    /// The events, newest first.
    pub events: Vec<EventBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/webhooks`.
#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    /// Organization the endpoint belongs to; an organization account may omit it.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Name the operator tells it apart by, unique inside the organization.
    pub name: String,
    /// URL deliveries are POSTed to.
    pub url: String,
    /// Event names to subscribe to.
    pub events: Vec<String>,
    /// Signing secret; the platform generates one when it is absent.
    #[serde(default)]
    pub secret: Option<String>,
}

/// `PATCH /api/v1/webhooks/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateWebhookRequest {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New URL.
    #[serde(default)]
    pub url: Option<String>,
    /// New subscription list.
    #[serde(default)]
    pub events: Option<Vec<String>>,
    /// New enabled flag.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New signing secret (a rotation; the old one stops working).
    #[serde(default)]
    pub secret: Option<String>,
}

/// `?limit=` on the delivery and event reads.
#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    /// How many rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `?name=` on the event read.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// How many rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Exact event name to filter by.
    #[serde(default)]
    pub name: Option<String>,
}

/// What a test delivery queued.
#[derive(Debug, Serialize)]
pub struct TestDeliveryBody {
    /// The recorded `webhook.test` event.
    pub event_id: i64,
    /// Deliveries queued (0 when the endpoint vanished between the check and the write).
    pub deliveries: u64,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/webhooks` — the endpoints this account may see.
pub async fn list_webhooks(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<EndpointsResponse>, ApiError> {
    let endpoints = store::list_endpoints(state.db().pool(), current.user.organization_id).await?;

    Ok(Json(EndpointsResponse {
        webhooks: endpoints.iter().map(EndpointBody::build).collect(),
    }))
}

/// `GET /api/v1/webhooks/{id}` — one endpoint.
pub async fn get_webhook(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(endpoint_id): Path<Uuid>,
) -> Result<Json<EndpointBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    Ok(Json(EndpointBody::build(&endpoint)))
}

/// `POST /api/v1/webhooks` — connect an endpoint.
///
/// The creation response is the one place a secret travels: when the operator supplied none,
/// the platform generated one and shows it here, once. Losing it means rotating it.
pub async fn create_webhook(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<EndpointBody>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let name = validation::validate_endpoint_name(&body.name)?;
    let url = validation::validate_url(&body.url)?;
    let events = validation::validate_subscriptions(&body.events)?;

    let provided = body.secret.is_some();
    let secret = match &body.secret {
        Some(raw) => validation::validate_secret(raw)?,
        None => validation::generate_secret(),
    };

    let endpoint = store::insert_endpoint(
        state.db().pool(),
        NewEndpoint {
            organization_id,
            name: name.clone(),
            url: url.clone(),
            secret,
            events: events.clone(),
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.endpoint.created")
            .target("webhook_endpoint", endpoint.id.to_string())
            .metadata(json!({
                "name": endpoint.name,
                "url": endpoint.url,
                "events": endpoint.events,
                "secret_generated": !provided,
            }))
            .ip_address(address.as_text())
            .organization(endpoint.organization_id),
    )
    .await?;

    let shown = (!provided).then(|| endpoint.secret.clone());
    Ok((
        StatusCode::CREATED,
        Json(EndpointBody::with_secret(&endpoint, shown)),
    ))
}

/// `PATCH /api/v1/webhooks/{id}` — change an endpoint.
pub async fn update_webhook(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(endpoint_id): Path<Uuid>,
    Json(body): Json<UpdateWebhookRequest>,
) -> Result<Json<EndpointBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;

    let name = body
        .name
        .map(|raw| validation::validate_endpoint_name(&raw))
        .transpose()?;
    let url = body
        .url
        .map(|raw| validation::validate_url(&raw))
        .transpose()?;
    let events = body
        .events
        .map(|raw| validation::validate_subscriptions(&raw))
        .transpose()?;
    let secret = body
        .secret
        .map(|raw| validation::validate_secret(&raw))
        .transpose()?;
    let rotated = secret.is_some();

    let changes = EndpointChanges {
        name,
        url,
        events,
        enabled: body.enabled,
        secret,
    };
    if changes.is_empty() {
        return Err(ApiError::bad_request(
            "empty_change_set",
            "the request carries no changes",
        ));
    }

    let updated = store::update_endpoint(state.db().pool(), endpoint.id, changes)
        .await?
        .ok_or_else(endpoint_not_found)?;

    let mut metadata = json!({ "name": updated.name, "enabled": updated.enabled });
    if rotated {
        metadata["secret_rotated"] = json!(true);
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.endpoint.updated")
            .target("webhook_endpoint", updated.id.to_string())
            .metadata(metadata)
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(EndpointBody::build(&updated)))
}

/// `DELETE /api/v1/webhooks/{id}` — disconnect an endpoint.
pub async fn delete_webhook(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(endpoint_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;

    if !store::delete_endpoint(state.db().pool(), endpoint.id).await? {
        return Err(endpoint_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.endpoint.removed")
            .target("webhook_endpoint", endpoint.id.to_string())
            .metadata(json!({ "name": endpoint.name }))
            .ip_address(address.as_text())
            .organization(endpoint.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/webhooks/{id}/deliveries` — the queue history of one endpoint.
pub async fn list_deliveries(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(endpoint_id): Path<Uuid>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<DeliveriesResponse>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);

    let deliveries = store::list_deliveries(state.db().pool(), endpoint.id, limit).await?;

    Ok(Json(DeliveriesResponse {
        deliveries: deliveries.iter().map(DeliveryBody::build).collect(),
    }))
}

/// `POST /api/v1/webhooks/{id}/test` — queue one signed `webhook.test` delivery.
///
/// The test reaches the endpoint even when it is switched off, because the question it answers
/// is "does my receiver accept a signed delivery", and asking it before going live is the
/// point.
pub async fn test_webhook(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(endpoint_id): Path<Uuid>,
) -> Result<(StatusCode, Json<TestDeliveryBody>), ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;

    let report = bus::emit_to(
        state.db().pool(),
        NewEvent::new("webhook.test")
            .organization(endpoint.organization_id)
            .actor(current.user.id)
            .payload(json!({
                "endpoint_id": endpoint.id,
                "endpoint_name": endpoint.name,
                "message": "This is a test delivery from Omnion.",
            })),
        &[endpoint.id],
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.endpoint.tested")
            .target("webhook_endpoint", endpoint.id.to_string())
            .metadata(json!({
                "event_id": report.event.id,
                "deliveries": report.deliveries,
            }))
            .ip_address(address.as_text())
            .organization(endpoint.organization_id),
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(TestDeliveryBody {
            event_id: report.event.id,
            deliveries: report.deliveries,
        }),
    ))
}

/// `GET /api/v1/events` — the platform's recent events.
pub async fn list_events(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    let name = match query.name.as_deref() {
        Some(raw) => Some(validation::validate_event_name(raw)?),
        None => None,
    };
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);

    let events = store::list_events(
        state.db().pool(),
        current.user.organization_id,
        name.as_deref(),
        limit,
    )
    .await?;

    Ok(Json(EventsResponse {
        events: events
            .iter()
            .map(|event| EventBody {
                id: event.id,
                name: event.name.clone(),
                organization_id: event.organization_id,
                site_id: event.site_id,
                actor_user_id: event.actor_user_id,
                payload: event.payload.clone(),
                created_at: event.created_at,
            })
            .collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Load an endpoint and refuse the request when it lives outside the caller's organization.
async fn endpoint_in_scope(
    state: &AppState,
    current: &CurrentSession,
    endpoint_id: Uuid,
) -> Result<WebhookEndpoint, ApiError> {
    let endpoint = store::find_endpoint(state.db().pool(), endpoint_id)
        .await?
        .ok_or_else(endpoint_not_found)?;
    ensure_same_organization(current, Some(endpoint.organization_id))?;
    Ok(endpoint)
}

/// The `404` handed out for an endpoint that is not there (or not this account's).
fn endpoint_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "webhook_endpoint_not_found",
        "no such webhook endpoint",
    )
}

/// Write one audit row.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}
