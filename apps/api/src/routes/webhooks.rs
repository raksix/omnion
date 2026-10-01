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
    EndpointChanges, EventsError, NewEndpoint, NewEvent, WebhookEndpoint, bus, store, validation,
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
    /// `event`, `test` or `replay` — what asked for this delivery.
    pub trigger: String,
    /// How long the receiver took in milliseconds, when it has run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i32>,
    /// How many times an operator has forced this row again.
    pub redeliver_count: i32,
    /// When it was last forced again.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub replayed_at: Option<OffsetDateTime>,
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
            trigger: delivery.trigger.clone(),
            duration_ms: delivery.duration_ms,
            redeliver_count: delivery.redeliver_count,
            replayed_at: delivery.replayed_at,
        }
    }
}

/// The deliveries of one endpoint (the original unfiltered shape).
///
/// Kept because `GET /webhooks/{id}/deliveries` answered this for a release; the filtered read
/// is the same path with query parameters, and a client that sends none gets this shape's
/// `deliveries` key plus the two paging fields. A client built against the old body keeps
/// working, which is what a deprecation window is for.
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
    /// Cursor for the next page: the id of the last row above, or `None` at the end.
    pub next_cursor: Option<i64>,
    /// Whether a further page exists. The panel needs it to decide whether "Load more" is a
    /// real button or decoration, and it is read from the row *past* the page rather than from
    /// a second count that could disagree with what is on screen.
    pub has_more: bool,
}

/// One payload field of one event, as the catalogue describes it.
#[derive(Debug, Serialize)]
pub struct CatalogueFieldBody {
    /// Field name, as it appears in the payload.
    pub name: &'static str,
    /// What it carries: `uuid`, `string`, `integer`, `boolean`, `timestamp`, `json` or `any`.
    pub kind: &'static str,
    /// Whether a receiver may rely on it being there.
    pub required: bool,
}

/// One entry of the catalogue.
#[derive(Debug, Serialize)]
pub struct CatalogueEntryBody {
    /// Dotted, lower-case name.
    pub name: &'static str,
    /// Which part of the platform it belongs to.
    pub area: &'static str,
    /// The group a receiver subscribes to as a whole (`page.*`).
    pub group: &'static str,
    /// One sentence a consumer can read before subscribing.
    pub description: &'static str,
    /// `live` or `reserved`.
    pub status: &'static str,
    /// The fields the payload carries.
    pub payload_fields: Vec<CatalogueFieldBody>,
    /// Deliveries this name produced in the last 24 hours, for this organization.
    ///
    /// The number answers the question the status column cannot: a *live* name with a zero here
    /// is a name the platform records but nobody is subscribed to, which is a fact an operator
    /// wants before connecting an endpoint rather than after. It is `0` rather than absent for
    /// a name with no deliveries so the column is always a number the screen can render.
    pub deliveries_24h: i64,
    /// The payload as a JSON Schema (REQ-033, slice 3d).
    ///
    /// **Served on the existing read rather than on a second endpoint.** A developer asking
    /// "what does this event carry" and a developer asking "what is the schema" are the same
    /// question, and splitting them would mean two round trips, two permission checks and two
    /// answers that can disagree. The document is generated by
    /// `omnion_events::schema::schema_for` from the same registry row as `payload_fields` above,
    /// so a schema that drifted from the field list would be a bug in one function rather than
    /// a data problem two sources could develop.
    ///
    /// The schema closes the payload's field set (`additionalProperties: false`). That is a
    /// promise, not a warning: an emitter that carries a field the row does not declare is
    /// refused by the gate in `apps/api/tests/events.rs`.
    pub payload_schema: serde_json::Value,
    /// A payload that satisfies the schema above.
    ///
    /// Every value is a placeholder that says what it is, so a developer who copies one into a
    /// receiver has pasted something obviously not real — and it is generated rather than
    /// hand-written, because a hand-written sample per event is a file that drifts from the
    /// registry and is discovered by a subscriber rather than by a test.
    pub sample: serde_json::Value,
}

/// The whole catalogue, grouped for the picker.
#[derive(Debug, Serialize)]
pub struct CatalogueResponse {
    /// Areas in table order; the picker's groups.
    pub areas: Vec<&'static str>,
    /// Every event the platform knows.
    pub events: Vec<CatalogueEntryBody>,
    /// How many entries are `live` right now.
    pub live_count: usize,
    /// How many are `reserved` — named, subscribable, and not emitted yet.
    pub reserved_count: usize,
    /// The most selections one endpoint may subscribe to.
    pub max_subscriptions: usize,
}

// ---------------------------------------------------------------------------------------------
// Retention (REQ-016, slice 3)
// ---------------------------------------------------------------------------------------------

/// One sweep's outcome, in the shape the run log stores and the screen reads back.
#[derive(Debug, Serialize)]
pub struct RetentionRunBody {
    /// Run id.
    pub id: Uuid,
    /// The organization swept; `null` is the platform's own events.
    pub organization_id: Option<Uuid>,
    /// When the sweep began.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When it finished.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// The window that was applied, days.
    pub window_days: i32,
    /// The instant older rows were swept.
    #[serde(with = "time::serde::rfc3339")]
    pub cutoff: OffsetDateTime,
    /// Events removed.
    pub events_deleted: i32,
    /// Delivery rows removed with them.
    pub deliveries_deleted: i32,
    /// Why the sweep could not finish, if it could not.
    pub error: Option<String>,
}

impl RetentionRunBody {
    fn build(run: &omnion_events::RetentionRun) -> Self {
        Self {
            id: run.id,
            organization_id: run.organization_id,
            started_at: run.started_at,
            finished_at: run.finished_at,
            window_days: run.window_days,
            cutoff: run.cutoff,
            events_deleted: run.events_deleted,
            deliveries_deleted: run.deliveries_deleted,
            error: run.error.clone(),
        }
    }
}

/// `GET /api/v1/events/retention` — the window, the counts, and the last sweep.
#[derive(Debug, Serialize)]
pub struct RetentionStatusBody {
    /// The organization described; `null` is the platform's own events.
    pub organization_id: Option<Uuid>,
    /// The window in force, days.
    pub window_days: i32,
    /// Shortest window the API accepts, so the screen can bound its own input.
    pub min_days: i32,
    /// Longest window the API accepts.
    pub max_days: i32,
    /// Events currently on the bus.
    pub events: i64,
    /// Events old enough to be swept on the next tick.
    pub due: i64,
    /// The last finished sweep, if this organization has ever been swept.
    pub last_run: Option<RetentionRunBody>,
    /// The last finished sweeps, newest first.
    pub recent_runs: Vec<RetentionRunBody>,
}

/// What a manual sweep removed.
#[derive(Debug, Serialize)]
pub struct SweepBody {
    /// The organization swept.
    pub organization_id: Option<Uuid>,
    /// The window that was applied.
    pub window_days: i32,
    /// The instant older rows were swept.
    #[serde(with = "time::serde::rfc3339")]
    pub cutoff: OffsetDateTime,
    /// Events removed.
    pub events_deleted: i64,
    /// Delivery rows removed with them.
    pub deliveries_deleted: i64,
    /// The run-log row this sweep wrote.
    pub run_id: Uuid,
}

/// `PATCH /api/v1/events/retention` — set the window.
#[derive(Debug, Deserialize)]
pub struct SetRetentionRequest {
    /// New window, days. Refused outside 1…3650 rather than clamped, because a silent clamp
    /// answers `200` with a number the caller did not ask for.
    pub window_days: i32,
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
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// How many rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
    /// One exact event name; repeatable, and several names mean "any of these".
    #[serde(default)]
    pub name: Vec<String>,
    /// Site the events happened on.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Account that caused them.
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    /// Lower bound of the window, RFC 3339.
    #[serde(default)]
    pub from: Option<String>,
    /// Upper bound of the window, RFC 3339.
    #[serde(default)]
    pub to: Option<String>,
    /// Keyset cursor: the id of the previous page's last row.
    #[serde(default)]
    pub cursor: Option<i64>,
}

/// The event read's query string, parsed by hand.
///
/// The reason is one line long and it was found the hard way: `serde_urlencoded` — which is
/// what `Query<T>` is built on — **rejects `?name=a` for a `Vec<String>` field outright**,
/// with a `400` whose body is plain text rather than the API's own error envelope. One
/// `?name=page.published` therefore did not filter, it *failed*, and the failure was invisible
/// to any client that only checked the status of a happy-path request. The same parser shape
/// already exists for the notification list (see `notifications::parse_list_params`), and its
/// doc comment carries the three rules this one repeats: a repeated key accumulates, a single
/// value is a one-element list, and a valueless key is a flag rather than a malformed pair.
///
/// Anything this build does not know is **ignored** rather than refused, so a panel that
/// starts sending one filter earlier than the API does still gets its feed. The two values
/// that cannot be guessed at are refused by name, because both are requests a reader would
/// otherwise see silently ignored: a `limit` of "lots" and a `cursor` that is not a number.
fn parse_events_query(raw: Option<&str>) -> Result<EventsQuery, ApiError> {
    let mut query = EventsQuery::default();

    let Some(raw) = raw else {
        return Ok(query);
    };

    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        // `split_once` yielding nothing is a valueless key (`?archived`), which is legal URI
        // syntax and means "present" — not a malformed pair.
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_query_token(key);
        let value = decode_query_token(value);
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "name" => query.name.push(value),
            "from" => query.from = Some(value),
            "to" => query.to = Some(value),
            "site_id" => {
                query.site_id = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_query("site_id", "it is not a uuid"))?,
                )
            }
            "actor_user_id" => {
                query.actor_user_id = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_query("actor_user_id", "it is not a uuid"))?,
                )
            }
            "cursor" => {
                query.cursor = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_query("cursor", "it is not a row id"))?,
                )
            }
            "limit" => {
                query.limit = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_query("limit", "it is not a whole number"))?,
                )
            }
            _ => {}
        }
    }

    Ok(query)
}

/// Percent-decode one query-string token, `+` meaning a space.
fn decode_query_token(value: &str) -> String {
    let bytes = value.replace('+', " ");
    percent_encoding::percent_decode_str(&bytes)
        .decode_utf8_lossy()
        .to_string()
}

/// The one refusal this parser hands out, naming the parameter so the panel can put the
/// message next to the right box instead of next to the whole filter bar.
fn invalid_query(field: &str, because: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_event_query",
        format!("`{field}` is not usable: {because}"),
    )
}

/// `GET /api/v1/webhooks/{id}/deliveries` — one page of an endpoint's history.
#[derive(Debug, Serialize)]
pub struct DeliveryPageBody {
    /// The rows, newest first.
    pub deliveries: Vec<DeliveryBody>,
    /// How many rows the filter matches in total, so the table's header can say
    /// "showing 25 of 340" instead of implying that 25 is all there is.
    pub total: i64,
    /// Whether a further page exists.
    pub has_more: bool,
    /// Cursor for the next page: the last row's `(created_at, id)`, or `None` at the end.
    ///
    /// Both halves, because the read sorts by `created_at desc, id desc` and a cursor on one
    /// column of a two-column order can repeat a row when two deliveries share a timestamp —
    /// which is the normal case, not an edge case, when the bus fans out to several endpoints.
    pub next_cursor: Option<DeliveryCursorBody>,
}

/// The pair one page's cursor is made of.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryCursorBody {
    /// `created_at` of the previous page's last row, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// That row's id.
    pub id: Uuid,
}

/// `POST /api/v1/webhooks/{id}/deliveries/{delivery_id}/redeliver` — one row forced again.
#[derive(Debug, Serialize)]
pub struct RedeliverBody {
    /// The delivery that was queued.
    pub delivery_id: Uuid,
    /// Its new status (`pending`).
    pub status: &'static str,
    /// How many times it has now been forced (1 after the first).
    pub redeliver_count: i32,
}

/// `POST /api/v1/webhooks/{id}/deliveries/redeliver` — many rows, each answered on its own.
#[derive(Debug, Serialize)]
pub struct RedeliverManyBody {
    /// How many of the requested ids moved.
    pub queued: usize,
    /// The ones that did not, with the code and sentence explaining each.
    pub skipped: Vec<RedeliverSkipBody>,
}

/// Why one id in a bulk redelivery did not move.
#[derive(Debug, Serialize)]
pub struct RedeliverSkipBody {
    /// The id as the caller wrote it.
    pub delivery_id: Uuid,
    /// Machine-readable reason.
    pub code: &'static str,
    /// The sentence the operator reads.
    pub message: &'static str,
}

/// `GET /api/v1/webhooks/{id}/stats` — what the receiver has been doing.
#[derive(Debug, Serialize)]
pub struct EndpointStatsBody {
    /// Window the numbers cover, in hours.
    pub window_hours: i64,
    /// Rows the receiver accepted.
    pub delivered: i64,
    /// Rows that ran out of attempts.
    pub failed: i64,
    /// Rows still waiting, whatever their age.
    pub pending: i64,
    /// Rows queued in the window, tests included — the history really does contain them.
    pub total: i64,
    /// Rows in the window an operator asked for by hand.
    pub tests: i64,
    /// Share of settled **traffic** rows that were accepted, or `null` when none settled.
    pub success_rate: Option<f64>,
    /// 95th percentile receiver duration in the window, or `null` when nothing ran.
    pub p95_duration_ms: Option<i32>,
}

/// `POST /api/v1/webhooks/{id}/secret/rotate`.
#[derive(Debug, Serialize)]
pub struct RotateSecretBody {
    /// The endpoint, without its old secret.
    #[serde(flatten)]
    pub endpoint: EndpointBody,
    /// The new secret, shown exactly once — the same rule as creation.
    pub secret: String,
}

/// `?status=&name=&from=&to=&q=&cursor=&limit=` on a delivery history read.
#[derive(Debug, Default, Deserialize)]
pub struct DeliveryQuery {
    /// How many rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
    /// One status, repeatable; several mean "any of these".
    #[serde(default)]
    pub status: Vec<String>,
    /// One event name, repeatable; several mean "any of these".
    #[serde(default)]
    pub name: Vec<String>,
    /// Lower bound of the window, RFC 3339.
    #[serde(default)]
    pub from: Option<String>,
    /// Upper bound of the window, RFC 3339.
    #[serde(default)]
    pub to: Option<String>,
    /// Substring of the delivery id or the event name.
    #[serde(default)]
    pub q: Option<String>,
    /// Keyset cursor: `created_at` of the previous page's last row, RFC 3339.
    #[serde(default)]
    pub cursor_at: Option<String>,
    /// The id that goes with `cursor_at`.
    #[serde(default)]
    pub cursor_id: Option<Uuid>,
}

/// `POST /api/v1/webhooks/{id}/deliveries/redeliver`.
#[derive(Debug, Deserialize)]
pub struct RedeliverManyRequest {
    /// The rows to force again, at most 100.
    pub delivery_ids: Vec<Uuid>,
}

/// How many rows one bulk redelivery may name.
///
/// A cap rather than a limit: the operation is one `update` per id, so a caller naming ten
/// thousand would hold a database connection for as long as it took to run ten thousand
/// statements, and the 100 that fit in a request body is already far more than a person
/// selecting by hand.
const MAX_REDELIVERY_BATCH: usize = 100;

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

/// `GET /api/v1/events/catalogue` — every event name the platform knows.
///
/// The registry is compiled in, so this read needs no database and no organization: the
/// catalogue is a fact about the *platform*, not about a tenant, and two organizations asking
/// must get the same answer. That is also why it carries no filter — the panel's filtering is
/// done on the list the picker already has, and a server-side filter over a constant would
/// only move the cost to a round trip.
///
/// The route is declared **before** `/events/{id}` would shadow it (a sibling of `/events`,
/// not a child), and it is `events.read` like the feed: describing what an event means is
/// reading the bus, not administering an endpoint.
pub async fn list_catalogue(
    state: State<AppState>,
    current: CurrentSession,
) -> Result<Json<CatalogueResponse>, ApiError> {
    let entries = omnion_events::catalogue::all();
    let live_count = entries
        .iter()
        .filter(|entry| entry.status == omnion_events::catalogue::Status::Live)
        .count();
    let reserved_count = entries.len() - live_count;

    // The registry itself needs no database — it is a fact about the *platform*, not about a
    // tenant, and two organizations asking must get the same answer. The one row-derived
    // number on this read is the 24-hour delivery count, and it is scoped to the caller's
    // organization exactly like every other read: a count is an observation about this
    // tenant's endpoints, and a platform-wide count would tell one organization how much
    // traffic another one receives.
    let since = OffsetDateTime::now_utc() - time::Duration::hours(24);
    let counts =
        store::delivery_counts_since(state.db().pool(), current.user.organization_id, since)
            .await?
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();

    Ok(Json(CatalogueResponse {
        areas: omnion_events::catalogue::areas(),
        live_count,
        reserved_count,
        max_subscriptions: validation::MAX_SUBSCRIPTIONS,
        events: entries
            .iter()
            .map(|entry| CatalogueEntryBody {
                name: entry.name,
                area: entry.area,
                group: entry.group(),
                description: entry.description,
                status: entry.status.as_str(),
                payload_fields: entry
                    .payload_fields
                    .iter()
                    .map(|field| CatalogueFieldBody {
                        name: field.name,
                        kind: field.kind.as_str(),
                        required: field.required,
                    })
                    .collect(),
                deliveries_24h: counts.get(entry.name).copied().unwrap_or_default(),
                // Both halves come out of the same registry row as the field list above, which
                // is the point: a schema that described a different payload than the one the
                // emitter writes would be two sources of truth, and the drift would be found
                // by a subscriber. `every_catalogue_sample_validates_against_its_own_schema` in
                // `omnion-events` holds the pair to each other on every row.
                payload_schema: omnion_events::schema::schema_for(entry),
                sample: omnion_events::schema::sample_for(entry),
            })
            .collect(),
    }))
}

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
    // Reconciliation, not just validation: a `page.*` group is stored expanded *and* as the
    // wildcard, so the endpoint is ready for an event added next release without anybody
    // editing it then.
    let events = omnion_events::catalogue::reconcile(&body.events)?;

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

    // The endpoint's own lifecycle belongs on the bus, not only in the audit trail. An
    // organization that watches itself through a second receiver needs to know a consumer
    // appeared \u2014 that is how a downstream system knows to start looking for a class of
    // event it was not previously told about. The name is the endpoint's own, never its
    // secret.
    bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.endpoint.created")
            .organization(endpoint.organization_id)
            .actor(current.user.id)
            .payload(json!({
                "endpoint_id": endpoint.id,
                "name": endpoint.name,
                "url": endpoint.url,
                "events": endpoint.events,
                "enabled": endpoint.enabled,
            })),
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
        .map(|raw| omnion_events::catalogue::reconcile(&raw))
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

    bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.endpoint.updated")
            .organization(updated.organization_id)
            .actor(current.user.id)
            .payload(json!({
                "endpoint_id": updated.id,
                "name": updated.name,
                "url": updated.url,
                "events": updated.events,
                "enabled": updated.enabled,
                "secret_rotated": rotated,
            })),
    )
    .await?;

    // A rotation is its own fact, and it is the one a receiver most needs: the old signature
    // stops verifying, and a receiver that does not hear this starts rejecting every delivery
    // it was being sent. The secret itself is never in the payload \u2014 only that it changed.
    if rotated {
        bus::emit(
            state.db().pool(),
            NewEvent::new("webhook.secret.rotated")
                .organization(updated.organization_id)
                .actor(current.user.id)
                .payload(json!({ "endpoint_id": updated.id, "name": updated.name })),
        )
        .await?;
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

    bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.endpoint.removed")
            .organization(endpoint.organization_id)
            .actor(current.user.id)
            .payload(json!({ "endpoint_id": endpoint.id, "name": endpoint.name })),
    )
    .await?;

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
///
/// The hand parser again, for the same reason as the event feed: `Query<DeliveryQuery>` would
/// answer a plain-text `400` for one `?status=`, which is a failure the panel would render as
/// an empty table rather than as an error. Every status is validated against the stored
/// vocabulary so a typo says `unknown_delivery_status` instead of quietly matching nothing.
pub async fn list_deliveries(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(endpoint_id): Path<Uuid>,
    query: axum::extract::RawQuery,
) -> Result<Json<DeliveryPageBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    let query = parse_delivery_query(query.0.as_deref())?;

    let mut names = Vec::with_capacity(query.name.len());
    for raw in &query.name {
        names.push(validation::validate_event_name(raw)?);
    }

    // Both halves of the cursor, or neither: a `cursor_at` without its `cursor_id` would be a
    // row-comparison against a null id, which Postgres answers by refusing — a 500 on a
    // request the panel builds itself, so it is refused here by name instead.
    let before = match (query.cursor_at.as_deref(), query.cursor_id) {
        (None, None) => None,
        (Some(raw), Some(id)) => Some((
            parse_instant(Some(raw), "cursor_at")?.ok_or_else(|| {
                invalid_delivery_query("cursor_at", "it is not an RFC 3339 timestamp")
            })?,
            id,
        )),
        _ => {
            return Err(invalid_delivery_query(
                "cursor",
                "a cursor needs both `cursor_at` and `cursor_id`",
            ));
        }
    };

    let filter = store::DeliveryFilter {
        statuses: query.status,
        names,
        from: parse_instant(query.from.as_deref(), "from")?,
        to: parse_instant(query.to.as_deref(), "to")?,
        search: query.q,
        before,
        limit: query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE),
    };

    let page = store::list_deliveries_filtered(state.db().pool(), endpoint.id, &filter).await?;

    // The cursor is the last row's own `(created_at, id)`, exclusive, so the next page cannot
    // re-serve the row the cursor names.
    let next_cursor = page
        .has_more
        .then(|| page.deliveries.last())
        .flatten()
        .map(|row| DeliveryCursorBody {
            at: row.created_at,
            id: row.id,
        });

    Ok(Json(DeliveryPageBody {
        deliveries: page.deliveries.iter().map(DeliveryBody::build).collect(),
        total: page.total,
        has_more: page.has_more,
        next_cursor,
    }))
}

/// Parse the delivery read's query string by hand — see [`parse_events_query`] for why.
///
/// The one addition to the event parser's rules: a `status` is checked against the stored
/// vocabulary, because a filter that matches nothing because of a typo is indistinguishable
/// from an endpoint that has had no failures, and those two send an operator opposite ways.
fn parse_delivery_query(raw: Option<&str>) -> Result<DeliveryQuery, ApiError> {
    let mut query = DeliveryQuery::default();
    let Some(raw) = raw else {
        return Ok(query);
    };

    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_query_token(key);
        let value = decode_query_token(value);
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "status" => query.status.push(value),
            "name" => query.name.push(value),
            "from" => query.from = Some(value),
            "to" => query.to = Some(value),
            "q" => query.q = Some(value),
            "cursor_at" => query.cursor_at = Some(value),
            "cursor_id" => {
                query.cursor_id = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_delivery_query("cursor_id", "it is not a uuid"))?,
                )
            }
            "limit" => {
                query.limit = Some(
                    value
                        .parse()
                        .map_err(|_| invalid_delivery_query("limit", "it is not a whole number"))?,
                )
            }
            _ => {}
        }
    }

    for status in &query.status {
        if omnion_events::model::DeliveryStatus::parse(status).is_none() {
            return Err(invalid_delivery_query(
                "status",
                "it is not pending, delivered or failed",
            ));
        }
    }

    Ok(query)
}

/// The delivery read's one refusal, naming the parameter.
fn invalid_delivery_query(field: &str, because: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_delivery_query",
        format!("`{field}` is not usable: {because}"),
    )
}

/// `POST /api/v1/webhooks/{id}/secret/rotate` — replace the signing secret.
///
/// A separate route rather than a `PATCH` with `secret`, because the two differ in what the
/// caller gets back: a rotation *shows* the new secret exactly once (a receiver cannot be
/// reconfigured with a secret it never saw), and a `PATCH` never does. Folding rotation into
/// the update route would have made the secret a field that appears and disappears according to
/// which verb the panel happened to use.
pub async fn rotate_secret(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(endpoint_id): Path<Uuid>,
) -> Result<Json<RotateSecretBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    let secret = validation::generate_secret();

    let updated = store::update_endpoint(
        state.db().pool(),
        endpoint.id,
        omnion_events::EndpointChanges {
            secret: Some(secret.clone()),
            ..Default::default()
        },
    )
    .await?
    .ok_or_else(endpoint_not_found)?;

    // The payload names the endpoint and nothing else: a rotation's own event is the one an
    // operator subscribes to in order to reconfigure a fleet, and it must not be the vector
    // that leaks the value it is announcing.
    bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.secret.rotated")
            .organization(updated.organization_id)
            .actor(current.user.id)
            .payload(json!({ "endpoint_id": updated.id, "name": updated.name })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.secret.rotated")
            .target("webhook_endpoint", updated.id.to_string())
            .metadata(json!({ "name": updated.name }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(RotateSecretBody {
        endpoint: EndpointBody::build(&updated),
        secret,
    }))
}

/// `POST /api/v1/webhooks/{id}/deliveries/{delivery_id}/redeliver` — force one row again.
pub async fn redeliver_one(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((endpoint_id, delivery_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<RedeliverBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    // The count comes back from the update itself, so the number reported is the one this call
    // wrote rather than one a second read might have caught after somebody else pressed the
    // same button concurrently.
    let redeliver_count = store::redeliver(state.db().pool(), endpoint.id, delivery_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.delivery.redelivered")
            .target("webhook_delivery", delivery_id.to_string())
            .metadata(json!({
                "endpoint": endpoint.name,
                "redeliver_count": redeliver_count,
            }))
            .ip_address(address.as_text())
            .organization(endpoint.organization_id),
    )
    .await?;

    Ok(Json(RedeliverBody {
        delivery_id,
        status: "pending",
        redeliver_count,
    }))
}

/// `POST /api/v1/webhooks/{id}/deliveries/redeliver` — force many rows, each answered on its own.
pub async fn redeliver_many(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(endpoint_id): Path<Uuid>,
    Json(body): Json<RedeliverManyRequest>,
) -> Result<Json<RedeliverManyBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;

    if body.delivery_ids.is_empty() {
        return Err(ApiError::bad_request(
            "empty_redelivery_batch",
            "name at least one delivery to send again",
        ));
    }
    if body.delivery_ids.len() > MAX_REDELIVERY_BATCH {
        return Err(ApiError::bad_request(
            "redelivery_batch_too_large",
            format!("name at most {MAX_REDELIVERY_BATCH} deliveries at once"),
        ));
    }

    let outcomes =
        store::redeliver_many(state.db().pool(), endpoint.id, &body.delivery_ids).await?;

    let mut queued = 0;
    let mut skipped = Vec::new();
    for (id, outcome) in outcomes {
        match outcome {
            Ok(_) => queued += 1,
            Err(EventsError::RedeliveryRefused { code, message }) => {
                // The code and sentence come from the crate's own enum rather than being
                // rebuilt here, so a fourth refusal there cannot reach the panel under one of
                // these three labels — and a store error, which is not a per-row answer, has
                // already stopped the batch above rather than being reported as a skip.
                skipped.push(RedeliverSkipBody {
                    delivery_id: id,
                    code,
                    message,
                });
            }
            Err(other) => return Err(other.into()),
        }
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.delivery.redelivered")
            .target("webhook_endpoint", endpoint.id.to_string())
            .metadata(json!({ "queued": queued, "skipped": skipped.len() }))
            .ip_address(address.as_text())
            .organization(endpoint.organization_id),
    )
    .await?;

    // `200` with per-id outcomes rather than `207` or a refusal: the request itself was
    // honoured, and a batch where half the rows are already pending is a normal outcome of a
    // multi-select, not a failure of the call.
    Ok(Json(RedeliverManyBody { queued, skipped }))
}

/// `GET /api/v1/webhooks/{id}/stats` — what this receiver has been doing.
pub async fn endpoint_stats(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(endpoint_id): Path<Uuid>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<EndpointStatsBody>, ApiError> {
    let endpoint = endpoint_in_scope(&state, &current, endpoint_id).await?;
    let window_hours = query
        .window_hours
        .unwrap_or(DEFAULT_STATS_WINDOW_HOURS)
        .clamp(1, 24 * 365);

    let stats = store::endpoint_stats(
        state.db().pool(),
        endpoint.id,
        OffsetDateTime::now_utc() - time::Duration::hours(window_hours),
    )
    .await?;

    Ok(Json(EndpointStatsBody {
        window_hours,
        delivered: stats.delivered,
        failed: stats.failed,
        pending: stats.pending,
        total: stats.total,
        tests: stats.tests,
        success_rate: stats.success_rate,
        p95_duration_ms: stats.p95_duration_ms,
    }))
}

/// `?window_hours=` on the stats read.
#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    /// How far back the numbers reach, in hours.
    #[serde(default)]
    pub window_hours: Option<i64>,
}

/// The default stats window: a day, which is long enough to cover a nightly batch and short
/// enough that a receiver which broke this morning does not still look green tomorrow.
const DEFAULT_STATS_WINDOW_HOURS: i64 = 24;

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

    // The test delivery is addressed to one endpoint with `emit_to`, so recording it as an
    // event as well would be pointless \u2014 it is aimed at the endpoint that was just tested
    // and reaches nobody else. The catalogue carries `webhook.endpoint.tested` for the other
    // direction: a second receiver watching the organization needs to know this endpoint was
    // proven, which is a fact about the endpoint rather than about a delivery.
    bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.endpoint.tested")
            .organization(endpoint.organization_id)
            .actor(current.user.id)
            .payload(json!({
                "endpoint_id": endpoint.id,
                "name": endpoint.name,
                "url": endpoint.url,
                "deliveries": report.deliveries,
            })),
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
    query: axum::extract::RawQuery,
) -> Result<Json<EventsResponse>, ApiError> {
    // The hand parser rather than `Query<EventsQuery>`: see `parse_events_query` — one
    // `?name=` is enough to make the generic deserializer answer a plain-text 400.
    let query = parse_events_query(query.0.as_deref())?;

    // Every name is validated, not just the first: a filter that silently ignored a typo
    // would return "nothing happened" for a name that has happened a thousand times, and the
    // operator would go looking for a broken module instead of a typo in their own filter.
    let mut names = Vec::with_capacity(query.name.len());
    for raw in &query.name {
        names.push(validation::validate_event_name(raw)?);
    }

    let filter = store::EventFilter {
        organization_id: current.user.organization_id,
        names,
        site_id: query.site_id,
        actor_user_id: query.actor_user_id,
        from: parse_instant(query.from.as_deref(), "from")?,
        to: parse_instant(query.to.as_deref(), "to")?,
        before: query.cursor.filter(|id| *id > 0),
        limit: query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE),
    };

    let page = store::list_events(state.db().pool(), &filter).await?;

    // The cursor is the last row's own id, exclusive: the next page asks for `id < cursor` and
    // therefore cannot re-serve the row the cursor names.
    let next_cursor = page
        .has_more
        .then(|| page.events.last().map(|event| event.id))
        .flatten();

    Ok(Json(EventsResponse {
        events: page
            .events
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
        next_cursor,
        has_more: page.has_more,
    }))
}

/// Parse an RFC 3339 bound of the feed's window.
///
/// The error names the parameter rather than the value: `from` and `to` arrive as strings
/// because `Query` will not do the parse for us, and a `400` that says "unparsable" without
/// saying *which* field leaves the caller guessing between two boxes on the screen.
fn parse_instant(
    raw: Option<&str>,
    field: &'static str,
) -> Result<Option<OffsetDateTime>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_event_window",
                format!("`{field}` is not an RFC 3339 timestamp"),
            )
        })
}

// ---------------------------------------------------------------------------------------------
// Retention handlers (REQ-016, slice 3)
// ---------------------------------------------------------------------------------------------

/// How many runs the status read returns in its `recent_runs` list.
const RETENTION_RUN_HISTORY: i64 = 5;

/// `GET /api/v1/events/retention` — the window, the counts and the last sweeps.
///
/// It is `events.read` like the feed, because reading how much history is kept is reading the
/// bus. **Changing** it is `webhooks.manage`, which is the stronger of the two keys, because
/// shortening a window is a destructive action on somebody's audit trail and a caller with
/// only `events.read` must not be able to trigger it by accident or by a link.
pub async fn retention_status(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RetentionStatusBody>, ApiError> {
    let organization_id = current.user.organization_id;
    let status = store::retention_status(state.db().pool(), organization_id).await?;
    let recent_runs =
        store::list_retention_runs(state.db().pool(), organization_id, RETENTION_RUN_HISTORY)
            .await?;

    Ok(Json(RetentionStatusBody {
        organization_id,
        window_days: status.window_days,
        min_days: omnion_events::MIN_RETENTION_DAYS,
        max_days: omnion_events::MAX_RETENTION_DAYS,
        events: status.events,
        due: status.due,
        last_run: status.last_run.as_ref().map(RetentionRunBody::build),
        recent_runs: recent_runs.iter().map(RetentionRunBody::build).collect(),
    }))
}

/// `PATCH /api/v1/events/retention` — set the window.
///
/// A **platform** account (`organization_id = None`) is refused by name rather than silently
/// writing a window onto a column it does not own: the sweeper applies each organization's own
/// window, and a platform account has no window to set, so accepting the write would answer
/// `200` and change nothing. The refusal says which account shape the route wants.
pub async fn set_retention(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SetRetentionRequest>,
) -> Result<Json<RetentionStatusBody>, ApiError> {
    let Some(organization_id) = current.user.organization_id else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "retention_scope_required",
            "a retention window belongs to an organization; this account is platform level",
        ));
    };

    // Validated before the write, so the refusal names the field and the range rather than
    // arriving as a check-constraint violation from the database.
    let days = validation::validate_retention_window(body.window_days)?;
    let previous = store::retention_window(state.db().pool(), Some(organization_id)).await?;
    let stored = store::set_retention_window(state.db().pool(), organization_id, days).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.retention.changed")
            .target("organization", organization_id.to_string())
            .metadata(json!({ "previous_window_days": previous, "window_days": stored }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    // The audit row is the record of the *change*; the bus is where the modules watch for one.
    // The event is emitted to nobody by fan-out unless an endpoint subscribes to it, which is
    // the same rule every other lifecycle event follows.
    let _ = bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.retention.changed")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "previous_window_days": previous,
                "window_days": stored,
            })),
    )
    .await;

    let status = store::retention_status(state.db().pool(), Some(organization_id)).await?;
    let recent_runs = store::list_retention_runs(
        state.db().pool(),
        Some(organization_id),
        RETENTION_RUN_HISTORY,
    )
    .await?;

    Ok(Json(RetentionStatusBody {
        organization_id: Some(organization_id),
        window_days: status.window_days,
        min_days: omnion_events::MIN_RETENTION_DAYS,
        max_days: omnion_events::MAX_RETENTION_DAYS,
        events: status.events,
        due: status.due,
        last_run: status.last_run.as_ref().map(RetentionRunBody::build),
        recent_runs: recent_runs.iter().map(RetentionRunBody::build).collect(),
    }))
}

/// `POST /api/v1/events/retention/sweep` — run one sweep now.
///
/// The button exists because "the weekly worker will get to it" is not an answer an operator can
/// act on the morning they need the disk back, and because a policy nobody can run on demand
/// is a policy that is only ever tested by a failure. It writes the same run log the worker
/// writes, so a manual sweep and a scheduled one are the same record — which is what makes the
/// log trustworthy.
pub async fn sweep_retention(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
) -> Result<Json<SweepBody>, ApiError> {
    let Some(organization_id) = current.user.organization_id else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "retention_scope_required",
            "a retention sweep belongs to an organization; this account is platform level",
        ));
    };

    let window_days = store::retention_window(state.db().pool(), Some(organization_id)).await?;
    let report = store::sweep_events(state.db().pool(), Some(organization_id), window_days).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "webhook.retention.swept")
            .target("organization", organization_id.to_string())
            .metadata(json!({
                "window_days": report.window_days,
                "events_deleted": report.events_deleted,
                "deliveries_deleted": report.deliveries_deleted,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    // A manual sweep emits the same event a scheduled one would, and the answer says **no
    // counts** were routed to endpoints rather than staying silent about it. The reason is
    // that the counts are already in the run log the caller gets back in the response, and a
    // payload that duplicated them is a second place for the two to disagree — but the
    // *occurrence* is a fact a subscriber subscribed to, and a fan-out that queues nothing
    // because a module decided the event "is only an audit fact" is a fan-out that silently
    // drops events.
    let _ = bus::emit(
        state.db().pool(),
        NewEvent::new("webhook.retention.swept")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "window_days": report.window_days })),
    )
    .await;

    Ok(Json(SweepBody {
        organization_id: Some(organization_id),
        window_days: report.window_days,
        cutoff: report.cutoff,
        events_deleted: report.events_deleted,
        deliveries_deleted: report.deliveries_deleted,
        run_id: report.run_id,
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
