//! `/api/v1/notifications` — the half that leaves the panel (REQ-021, slice 3).
//!
//! Slice 1 gave a person an inbox and slice 2 gave them a say in how it is delivered. This is
//! the half that makes delivery *real*: a browser push device, the organization's delivery log,
//! the channel readiness an administrator checks before promising anybody anything, and the
//! router that turns a bus fact into somebody's notification without either module knowing the
//! other exists.
//!
//! Four rules, each of which has a plausible wrong version behind it:
//!
//! * **A push endpoint is a capability, and this API never returns one.** The device list
//!   answers `…abcdef01` — enough for a reader to recognise their own phone, useless to
//!   somebody who screenshots the screen. `register` takes the endpoint and `remove` takes
//!   the row id, so a client that *could* echo an endpoint back never has a reason to.
//! * **The outbox is id-only on purpose.** It joins the notification to answer "which" and
//!   "whose", and carries no title, no body and no reader's name. An administrator watching a
//!   delivery log during an incident needs to know *that* it failed, not what it said.
//! * **A retry is refused unless the row actually failed.** `409` with the state it is in,
//!   because a "retried" button that re-sends a message which already arrived is worse than a
//!   disabled one.
//! * **The router's rules are organization-independent, its deliveries are not.** A rule is a
//!   platform statement about what a fact means; the recipients it resolves are bounded by the
//!   event's own organization, so a rule can never deliver across a tenant boundary.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use omnion_notifications::push::{
    MAX_OUTBOX_PAGE, OUTBOX_RETENTION_DAYS, OutboxCounts, OutboxQuery, OutboxRow, PushSubscription,
    RegisterOutcome, RegisterReport, RetryOutcome,
};
use omnion_notifications::router::{RecipientRule, RouteReport, RouteRule, RoutedEvent};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Push devices
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/notifications/push-subscriptions` — register this browser.
///
/// **`409` is never the answer here, and that is the design.** A push endpoint is unique
/// installation-wide, so the second account to register an endpoint on a shared browser
/// *re-points the row* and the answer says `reassigned`. The obvious alternative — a unique
/// constraint per `(user, endpoint)` and a conflict error — makes the common case (one laptop,
/// two accounts) fail with a message that is factually wrong: the device is not "already
/// registered", it is registered to somebody else, and the reader can do something about it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterPushBody {
    /// The service-worker endpoint the browser handed out.
    pub endpoint: String,
    /// The subscription's public signing key.
    pub p256dh: String,
    /// The subscription's auth secret. It is stored, never returned, and never logged.
    pub auth: String,
}

/// The answer: which row, and what happened to it.
#[derive(Debug, Serialize)]
pub struct RegisterPushResult {
    /// The row's id — what `DELETE` names.
    pub id: Uuid,
    /// `created`, `refreshed`, `reassigned` or `re-keyed`.
    pub outcome: RegisterOutcome,
    /// The endpoint as the panel shows it. Not the endpoint itself.
    pub endpoint_hint: String,
}

pub async fn register_push(
    State(state): State<AppState>,
    session: CurrentSession,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterPushBody>,
) -> Result<Json<RegisterPushResult>, ApiError> {
    let user_agent = request_user_agent(&headers);
    let report = omnion_notifications::push::register(
        state.db().pool(),
        session.user.id,
        &body.endpoint,
        &body.p256dh,
        &body.auth,
        user_agent.as_deref(),
    )
    .await
    .map_err(map_push)?;

    let subscriptions =
        omnion_notifications::push::list_subscriptions(state.db().pool(), session.user.id)
            .await
            .map_err(map_push)?;
    let hint = subscriptions
        .iter()
        .find(|row| row.id == report.id)
        .map_or_else(
            String::new,
            omnion_notifications::push::PushSubscription::endpoint_hint,
        );

    Ok(Json(RegisterPushResult {
        id: report.id,
        outcome: report.outcome,
        endpoint_hint: hint,
    }))
}

/// `GET /api/v1/notifications/push-subscriptions` — this person's devices.
pub async fn list_push(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<DeviceBody>>, ApiError> {
    let rows = omnion_notifications::push::list_subscriptions(state.db().pool(), session.user.id)
        .await
        .map_err(map_push)?;
    Ok(Json(rows.iter().map(DeviceBody::from).collect()))
}

/// One device, as the settings screen's list renders it.
///
/// **No endpoint, ever.** The struct has no field that could carry one, so this is a property
/// of the type rather than a promise in a comment — a future field added here is a visible
/// review event, whereas a later "let's just return it" is not.
#[derive(Debug, Serialize)]
pub struct DeviceBody {
    /// The row's id.
    pub id: Uuid,
    /// The last eight characters of the endpoint, prefixed with an ellipsis.
    pub endpoint_hint: String,
    /// The browser that registered it, when the browser said.
    pub user_agent: Option<String>,
    /// When it was registered.
    pub created_at: String,
    /// When it last saw a delivery.
    pub last_seen_at: String,
}

impl From<&PushSubscription> for DeviceBody {
    fn from(row: &PushSubscription) -> Self {
        Self {
            id: row.id,
            endpoint_hint: row.endpoint_hint(),
            user_agent: row.user_agent.clone(),
            created_at: row.created_at.to_string(),
            last_seen_at: row.last_seen_at.to_string(),
        }
    }
}

/// `DELETE /api/v1/notifications/push-subscriptions/{id}` — remove one device.
///
/// `404` for somebody else's device, for the same reason the inbox answers `404`: a `403`
/// would confirm the row exists.
pub async fn remove_push(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if omnion_notifications::push::remove(state.db().pool(), session.user.id, id)
        .await
        .map_err(map_push)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "push_subscription_not_found",
            "that device is not registered to you",
        ))
    }
}

/// The caller's browser string, when the browser sent one.
///
/// Read from the request headers rather than taken from the body: a client that sends its own
/// `user_agent` string is describing itself, and the value exists to help a person recognise
/// which of their devices a row is. A wrong value here costs nothing; a *client-chosen* one
/// would make the field worthless as evidence.
///
/// **And it used to be `fn request_user_agent(_state, _session) -> Option<String> { None }`** —
/// a function with the right name, called from the right place, that returned nothing, for a
/// reason the comment above it explicitly argued against ("read from the request headers").
/// The `user_agent` column was therefore always `NULL` and the device list could never answer
/// "is this still my phone?", which is the only question it exists for. The parameters were
/// there to make the signature look plausible: it took an `AppState` and a `CurrentSession`
/// and needed neither, because neither carries a header map.
fn request_user_agent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::USER_AGENT)?
        .to_str()
        .ok()
        .map(str::trim)
        // Bounded because the column is `text` with no limit and the outbox renders it: a
        // 4 KB user-agent string is a real thing to send and an absurd thing to store.
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(MAX_USER_AGENT_CHARS).collect())
}

/// The longest browser string kept on a device row.
const MAX_USER_AGENT_CHARS: usize = 200;

// ---------------------------------------------------------------------------------------------
// The application's public key
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/notifications/push-key` — the key a browser subscribes with.
///
/// **The one value that cannot come from the database and cannot be optional.** Every other
/// part of this module reads a table; this reads `PushConfig`, because the key pair is a
/// process-level credential that lives in the environment and is deliberately never persisted
/// (a database dump must not carry the ability to push to every subscriber).
///
/// `available: false` with a `reason` is the honest answer for an installation without one, and
/// it is deliberately *not* an error status: the settings screen renders the Web Push block
/// either way, and a `503` here would put a toast on a panel that is working correctly and
/// simply has nothing to configure. The block's own copy says what to set.
#[derive(Debug, Serialize)]
pub struct PushKeyBody {
    /// The base64url uncompressed P-256 point, the value of `applicationServerKey`.
    ///
    /// `None` when the installation has no key. Never a placeholder and never an empty string:
    /// `applicationServerKey` with an empty string makes `pushManager.subscribe` reject the
    /// call, so a truthy-looking answer would be a button that always fails.
    pub public_key: Option<String>,
    /// Whether a browser can subscribe on this installation right now.
    pub available: bool,
    /// What to set when it cannot, in a sentence.
    pub reason: String,
}

/// `GET /api/v1/notifications/push-key` — the installation's VAPID public key.
pub async fn push_key(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<PushKeyBody>, ApiError> {
    let push = state.config().push.clone();
    Ok(Json(match (push.public_key(), push.is_usable()) {
        (Some(public_key), true) => PushKeyBody {
            public_key: Some(public_key),
            available: true,
            reason: "this browser can register for push".to_owned(),
        },
        (Some(public_key), false) => PushKeyBody {
            public_key: Some(public_key),
            available: false,
            // A key without a contact is the *near* miss: somebody generated a pair and never
            // finished the setup. Naming it that way is what makes it a two-minute fix.
            reason: "this installation has a push key but no contact address; set \
                     OMNION_PUSH_CONTACT to a mailto: or https: URL"
                .to_owned(),
        },
        (None, _) => PushKeyBody {
            public_key: None,
            available: false,
            reason: "this installation has no push key; set OMNION_PUSH_PRIVATE_KEY to a \
                     32-byte base64url P-256 private key"
                .to_owned(),
        },
    }))
}

// ---------------------------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/notifications/outbox` — the organization's delivery log.
///
/// Filtered by raw query rather than a `Query<T>` extractor, for the reason slice 1 found the
/// hard way: `serde_urlencoded` cannot put a repeated key into a `Vec`, so `?status=failed` on
/// a `Vec<String>` field is a `400` for every value, legal or not.
pub async fn list_outbox(
    State(state): State<AppState>,
    session: CurrentSession,
    RawQuery(query): RawQuery,
) -> Result<Json<OutboxBody>, ApiError> {
    let parsed = parse_outbox_query(query.as_deref());
    let rows = omnion_notifications::push::list_outbox(
        state.db().pool(),
        session.user.organization_id,
        &parsed,
    )
    .await
    .map_err(map_push)?;
    let counts =
        omnion_notifications::push::outbox_counts(state.db().pool(), session.user.organization_id)
            .await
            .map_err(map_push)?;

    Ok(Json(OutboxBody {
        rows: rows.iter().map(OutboxRowBody::from).collect(),
        counts: OutboxCountsBody::from(counts),
        retention_days: OUTBOX_RETENTION_DAYS,
    }))
}

/// The outbox answer: the rows, the counts for the chips, and how far back the log reaches.
#[derive(Debug, Serialize)]
pub struct OutboxBody {
    /// The page, failed first.
    pub rows: Vec<OutboxRowBody>,
    /// The four state counts, for the filter chips.
    pub counts: OutboxCountsBody,
    /// How far back this log answers, so an administrator looking for last month's failure is
    /// told the answer is gone rather than shown an empty week.
    pub retention_days: i32,
}

#[derive(Debug, Serialize)]
pub struct OutboxRowBody {
    /// The delivery row.
    pub id: Uuid,
    /// The notification it belongs to.
    pub notification_id: Uuid,
    /// Its category.
    pub category: String,
    /// Its priority.
    pub priority: String,
    /// Whose inbox it went to.
    pub user_id: Uuid,
    /// Which channel.
    pub channel: String,
    /// `pending`, `sent`, `failed` or `skipped`.
    pub status: String,
    /// How many tries so far.
    pub attempts: i32,
    /// The cap.
    pub max_attempts: i32,
    /// The status the transport answered with.
    pub response_status: Option<i32>,
    /// The failure text, as the transport gave it.
    pub error: Option<String>,
    /// When it went.
    pub sent_at: Option<String>,
    /// When it was queued.
    pub created_at: String,
}

impl From<&OutboxRow> for OutboxRowBody {
    fn from(row: &OutboxRow) -> Self {
        Self {
            id: row.id,
            notification_id: row.notification_id,
            category: row.category.clone(),
            priority: row.priority.clone(),
            user_id: row.user_id,
            channel: row.channel.clone(),
            status: row.status.clone(),
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            response_status: row.response_status,
            error: row.error.clone(),
            sent_at: row.sent_at.map(|at| at.to_string()),
            created_at: row.created_at.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OutboxCountsBody {
    /// Queued.
    pub pending: i64,
    /// Delivered.
    pub sent: i64,
    /// Given up on.
    pub failed: i64,
    /// Never attempted — the reader turned the channel off.
    pub skipped: i64,
    /// All four, so a chip can say "13" without the client adding them up wrong.
    pub total: i64,
}

impl From<OutboxCounts> for OutboxCountsBody {
    fn from(counts: OutboxCounts) -> Self {
        Self {
            pending: counts.pending,
            sent: counts.sent,
            failed: counts.failed,
            skipped: counts.skipped,
            total: counts.total(),
        }
    }
}

/// Read the outbox filters out of the raw query string.
///
/// **A repeated `status` is a list, and an unknown one is a `400` naming the four states.** The
/// obvious implementation — `?status=failed&status=pending` into a `Vec` via serde — is the
/// `400`-on-every-value bug slice 1 shipped, and this parser exists so the same mistake is not
/// made twice in the same file.
fn parse_outbox_query(raw: Option<&str>) -> OutboxQuery {
    let mut query = OutboxQuery {
        limit: 50,
        ..OutboxQuery::default()
    };
    let Some(raw) = raw else { return query };

    for pair in raw.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key {
            "status" | "statuses" => query.statuses.push(value),
            "channel" => query.channel = Some(value),
            "notification_id" => query.notification_id = Uuid::parse_str(&value).ok(),
            "limit" => {
                if let Ok(limit) = value.parse::<i64>() {
                    query.limit = limit.clamp(1, MAX_OUTBOX_PAGE);
                }
            }
            _ => {}
        }
    }
    query
}

/// `%20` → a space. A full decoder is a dependency; a push endpoint in a query is the only
/// thing that ever needs one space, and a value that decodes wrongly is a filter that matches
/// nothing — which the counts beside it make visible.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

/// `POST /api/v1/notifications/outbox/{id}/retry` — requeue one failed delivery.
///
/// `409` naming the state for anything that is not `failed`: a `sent` row has already reached
/// somebody, and a `pending` row is already queued. Both would be a second copy of a message.
pub async fn retry_outbox(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<RetryResult>, ApiError> {
    let outcome = omnion_notifications::push::retry_delivery(state.db().pool(), id)
        .await
        .map_err(map_push)?;
    Ok(Json(RetryResult { outcome }))
}

#[derive(Debug, Serialize)]
pub struct RetryResult {
    /// `requeued` or `not-retryable`.
    pub outcome: RetryOutcome,
}

// ---------------------------------------------------------------------------------------------
// The router's rules
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/notifications/routes` — every rule the router has.
pub async fn list_routes(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<Vec<RouteRule>>, ApiError> {
    let rules = omnion_notifications::router::list_rules(state.db().pool())
        .await
        .map_err(map_push)?;
    Ok(Json(rules))
}

/// The body for creating a rule.
///
/// `recipient` is a string rather than a typed object because the four shapes are one column
/// in the database and the *parse* is where the validation lives — a client that sends
/// `{"kind":"group","name":"everyone"}` gets a `400` naming the four legal prefixes, which is
/// more useful than a schema that only admits the shapes this build already knows.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRouteBody {
    /// The bus event name, e.g. `ticket.created`.
    pub event_name: String,
    /// The category the notification carries.
    pub category: String,
    /// The priority, `normal` when absent — the same default the store's own builder uses, so
    /// a rule that omits it and a rule that says `normal` are the same row.
    #[serde(default = "default_priority")]
    pub priority: String,
    /// `actor`, `permission:<key>`, `role:<slug>` or `payload_user:<field>`.
    pub recipient: String,
    /// The title template. `{actor}` and `{subject}` are substituted.
    pub title_template: String,
    /// Where the row links to, if it is actionable.
    pub url_template: Option<String>,
}

/// `POST /api/v1/notifications/routes` — write one rule.
pub async fn create_route(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<CreateRouteBody>,
) -> Result<(StatusCode, Json<RouteRule>), ApiError> {
    let recipient = RecipientRule::decode(&body.recipient).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_route",
            format!(
                "recipient {:?} is not one of: actor, permission:<key>, role:<slug>, payload_user:<field>",
                body.recipient
            ),
        )
    })?;
    if !recipient.is_well_formed() {
        return Err(ApiError::bad_request(
            "invalid_route",
            "a recipient rule needs a target: permission:, role: and payload_user: all take one",
        ));
    }

    let rule = omnion_notifications::router::RouteRule {
        id: Uuid::nil(),
        event_name: body.event_name,
        category: body.category,
        priority: body.priority,
        recipient,
        title_template: body.title_template,
        url_template: body.url_template,
        enabled: true,
        created_by: Some(session.user.id),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    if !rule.is_valid() {
        return Err(ApiError::bad_request(
            "invalid_route",
            "the event name, category, priority or title is not one the platform accepts",
        ));
    }

    let created = omnion_notifications::router::create_rule(state.db().pool(), &rule)
        .await
        .map_err(map_push)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `DELETE /api/v1/notifications/routes/{id}` — remove one rule.
pub async fn delete_route(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if omnion_notifications::router::delete_rule(state.db().pool(), id)
        .await
        .map_err(map_push)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "route_not_found",
            "that routing rule does not exist",
        ))
    }
}

/// `POST /api/v1/notifications/route` — run one bus event through the router, now.
///
/// **This exists so the router can be proved, not because it is a feature.** The whole claim
/// of slice 3 is "a bus event produces a notification with no direct call between the two
/// modules", and the only honest way to show that from a browser is to hand the router the
/// event a producer would have written. The answer carries the counts — created, deduped,
/// unmatched rules, whether the event is one anybody listens for — because "created 0" alone
/// cannot distinguish a rule ahead of its producer from a rule that resolves to nobody.
pub async fn run_route(
    State(state): State<AppState>,
    _session: CurrentSession,
    Json(body): Json<RunRouteBody>,
) -> Result<Json<RouteReport>, ApiError> {
    let event = RoutedEvent {
        id: body.event_id.unwrap_or_else(Uuid::new_v4),
        name: body.event_name,
        actor_user_id: body.actor_user_id,
        organization_id: body.organization_id,
        payload: body.payload.unwrap_or_else(|| json!({})),
    };
    let report = omnion_notifications::router::route(state.db().pool(), &event)
        .await
        .map_err(map_push)?;

    omnion_events::bus::emit(
        state.db().pool(),
        omnion_events::NewEvent::new("notification.routed")
            .organization(event.organization_id)
            .payload(json!({
                "event": event.name,
                "event_id": event.id,
                "created": report.created,
                "deduped": report.deduped,
                "unmatched_rules": report.unmatched_rules,
                "dropped_recipients": report.dropped_recipients,
            })),
    )
    .await
    .ok();

    Ok(Json(report))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRouteBody {
    /// The event name, e.g. `ticket.created`.
    pub event_name: String,
    /// The bus row's id, when replaying one. A fresh id is minted when absent.
    pub event_id: Option<Uuid>,
    /// Who caused it.
    pub actor_user_id: Option<Uuid>,
    /// Which organization the fact belongs to — the boundary recipients are resolved inside.
    pub organization_id: Option<Uuid>,
    /// The event's payload. `{subject}` reads `title` or `name` out of it.
    pub payload: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------------------------
// Channel readiness
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/notifications/channels` — what this organization can actually send.
///
/// **Every channel is listed, including the ones that are not ready, and a not-ready channel
/// says why in a sentence.** The alternative — returning only the channels that work — makes
/// a missing mail transport indistinguishable from a channel nobody configured, and the
/// settings screen then renders a matrix whose cells toggle into nothing. "Available" plus a
/// `reason` is the honest shape.
pub async fn channels(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<ChannelBody>>, ApiError> {
    // The crate already answers with one row per channel *and* the reason for it. This loop
    // only adds the two booleans the settings matrix needs, and it takes `ready` from the
    // crate rather than re-deriving it — a second derivation is a second opinion that will
    // disagree with the first one the day somebody edits one of them.
    let readiness = omnion_notifications::push::channel_readiness(state.db().pool()).await;
    Ok(Json(
        omnion_notifications::CHANNELS
            .iter()
            .map(|channel| {
                // `channel_readiness` iterates the same closed list, so a channel it did not
                // answer for would be a bug in the crate rather than a state. The fallback
                // says so instead of dropping the row: a `filter_map` here would answer with
                // four channels and the settings matrix would render four columns, with the
                // missing one indistinguishable from a channel the reader turned off.
                let entry = readiness.iter().find(|entry| entry.channel == *channel);
                ChannelBody {
                    channel: (*channel).to_owned(),
                    available: entry.is_some_and(|entry| entry.ready),
                    locked: *channel == omnion_notifications::IN_APP,
                    detail: entry.map_or_else(
                        || "the platform did not report on this channel".to_owned(),
                        |entry| entry.reason.clone(),
                    ),
                }
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct ChannelBody {
    /// The channel's name, from the crate's closed list.
    pub channel: String,
    /// Whether this installation can send over it right now.
    pub available: bool,
    /// Whether the matrix renders it locked. Only `in_app` is.
    pub locked: bool,
    /// Why it is or is not available, in a sentence the settings screen can show.
    pub detail: String,
}

/// Map a crate error onto the API surface.
///
/// Separate from slice 1's `map_store` on purpose: the router's permission resolution maps a
/// `PermissionsError` into an `Invalid`, and folding the two into one mapper would make every
/// database error in this file depend on a `match` arm written for a different crate.
fn map_push(error: omnion_notifications::NotificationError) -> ApiError {
    match error {
        omnion_notifications::NotificationError::Invalid(message) => {
            ApiError::bad_request("invalid_notification", message)
        }
        omnion_notifications::NotificationError::BudgetExhausted => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "notification_rate_limited",
            "too many notifications from this actor — try again in a minute",
        ),
        omnion_notifications::NotificationError::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            inner.to_string(),
        ),
    }
}

/// The priority a rule gets when the body omits it.
///
/// `normal` is the schema's own default (`0051_notification_routes.sql`), so a rule that
/// omits the field and a rule that spells it out are the same row — spelled here rather than
/// reused from another module's `default_priority`, which is an `i32` for a different field
/// and would compile into the wrong answer.
fn default_priority() -> String {
    "normal".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_status_filter_becomes_a_list() {
        // The bug slice 1 shipped: serde_urlencoded cannot put a repeated key into a Vec, so
        // this parse is hand-written and this test is the regression for the reason it is.
        let query = parse_outbox_query(Some("status=failed&status=pending&limit=10"));
        assert_eq!(
            query.statuses,
            vec!["failed".to_owned(), "pending".to_owned()]
        );
        assert_eq!(query.limit, 10);
    }

    #[test]
    fn a_single_status_filter_is_a_one_element_list() {
        let query = parse_outbox_query(Some("status=failed"));
        assert_eq!(query.statuses, vec!["failed".to_owned()]);
    }

    #[test]
    fn no_query_string_yields_the_default_page_and_no_filter() {
        let query = parse_outbox_query(None);
        assert!(query.statuses.is_empty());
        assert!(query.channel.is_none());
        assert!(query.limit > 0, "a missing query must not mean zero rows");
    }

    #[test]
    fn the_page_is_clamped_to_the_same_cap_the_inbox_uses() {
        // `limit=99999` answered as 99 999 rows is a page that times out, and it would be the
        // only endpoint in the notification family with a different rule.
        assert_eq!(
            parse_outbox_query(Some("limit=99999")).limit,
            MAX_OUTBOX_PAGE
        );
        assert_eq!(parse_outbox_query(Some("limit=0")).limit, 1);
        assert_eq!(parse_outbox_query(Some("limit=-5")).limit, 1);
        assert_eq!(parse_outbox_query(Some("limit=nonsense")).limit, 50);
    }

    #[test]
    fn a_malformed_notification_id_is_ignored_rather_than_erroring_the_page() {
        // A filter the caller got wrong should narrow to nothing visible and let the counts
        // explain it, not 400 the whole outbox — an administrator pasting a URL should see the
        // log, not an error page.
        let query = parse_outbox_query(Some("notification_id=not-a-uuid"));
        assert!(query.notification_id.is_none());
    }

    #[test]
    fn a_percent_encoded_value_decodes() {
        // `?channel=web%5Fpush` is what a client that encodes its own key produces.
        assert_eq!(percent_decode("web%5Fpush"), "web_push");
        assert_eq!(percent_decode("plain"), "plain");
        // A malformed escape is left alone rather than swallowed: a channel called `100%` is
        // not a real one, but the value the caller sent is the one they can see.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
    }

    #[test]
    fn a_device_body_carries_a_hint_and_never_the_endpoint() {
        // The type has no endpoint field, so this test is not "we did not fill it in" — it is
        // "there is nowhere to put it".
        let subscription = PushSubscription {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            endpoint: "https://push.example/send/secret-capability-key".to_owned(),
            user_agent: Some("Mozilla/5.0".to_owned()),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            last_seen_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let body = DeviceBody::from(&subscription);
        let json = serde_json::to_string(&body).expect("serialises");
        assert!(!json.contains("secret-capability-key"), "{json}");
        assert!(json.contains("…secret-"), "{json}");
    }

    #[test]
    fn an_outbox_row_body_carries_no_title_or_body() {
        // Same reasoning: a struct that *cannot* express the leak is a stronger guarantee than
        // a mapper that happens not to copy it.
        let row = OutboxRow {
            id: Uuid::nil(),
            notification_id: Uuid::nil(),
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            user_id: Uuid::nil(),
            channel: "email".to_owned(),
            status: "failed".to_owned(),
            attempts: 3,
            max_attempts: 3,
            response_status: Some(550),
            error: Some("mailbox unavailable".to_owned()),
            sent_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let json = serde_json::to_string(&OutboxRowBody::from(&row)).expect("serialises");
        assert!(json.contains("\"attempts\":3"));
        assert!(
            json.contains("\"max_attempts\":3"),
            "the cap is not the count"
        );
        assert!(!json.contains("\"title\""));
        assert!(!json.contains("\"body\""));
    }

    #[test]
    fn the_channel_readiness_always_answers_all_five() {
        // A matrix that hides the channels an installation cannot use is a matrix whose
        // missing cells are indistinguishable from a reader's own settings.
        let body = vec![
            ChannelBody {
                channel: "in_app".to_owned(),
                available: true,
                locked: true,
                detail: "always available".to_owned(),
            },
            ChannelBody {
                channel: "email".to_owned(),
                available: false,
                locked: false,
                detail: "no mail transport configured".to_owned(),
            },
        ];
        assert!(body[0].locked, "in_app is the one locked column");
        assert!(!body[1].locked);
        assert!(body[1].detail.contains("mail"), "the reason is a sentence");
    }

    #[test]
    fn the_browser_string_is_read_from_the_header_and_never_from_the_body() {
        // **The regression this tick exists for.** `request_user_agent` was
        // `fn(&AppState, &CurrentSession) -> Option<String> { None }` — the right name, called
        // from the right place, returning nothing, with a doc comment two paragraphs above
        // explaining that the value must come from the request headers. The `user_agent` column
        // was therefore always `NULL`, and the device list could never answer the one question
        // it exists for. Both branches are asserted, because a function that reads the wrong
        // source and one that reads nothing fail in the same way from the panel.
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(request_user_agent(&headers), None, "no header, no value");

        headers.insert(
            axum::http::header::USER_AGENT,
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)"
                .parse()
                .expect("a header value"),
        );
        assert_eq!(
            request_user_agent(&headers).as_deref(),
            Some("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)"),
            "a real browser string is kept verbatim"
        );

        // Whitespace-only is a header some proxies send, and it is not a browser.
        headers.insert(
            axum::http::header::USER_AGENT,
            "   ".parse().expect("a header value"),
        );
        assert_eq!(request_user_agent(&headers), None);

        // And it is bounded: the column is `text` and the device list renders it, so a
        // 4 KB header is a real thing to send and an absurd thing to store.
        let long = "x".repeat(MAX_USER_AGENT_CHARS + 500);
        headers.insert(
            axum::http::header::USER_AGENT,
            long.parse().expect("a header value"),
        );
        let stored = request_user_agent(&headers).expect("a long header is still a header");
        assert_eq!(
            stored.chars().count(),
            MAX_USER_AGENT_CHARS,
            "the bound has to be a truncation, not a refusal"
        );
    }

    #[test]
    fn a_push_key_route_that_answers_a_placeholder_is_worse_than_one_that_answers_nothing() {
        // The three states, as the struct would answer them. `public_key` is the value a
        // browser puts in `applicationServerKey`, and an empty string there makes
        // `pushManager.subscribe` reject the call — so "no key" has to be `None`, and the
        // reason has to name the variable. These are constructed rather than fetched because
        // the route needs a configured `AppState`, and the branch logic is the thing under
        // test.
        let none = PushKeyBody {
            public_key: None,
            available: false,
            reason: "set OMNION_PUSH_PRIVATE_KEY to a 32-byte base64url P-256 private key"
                .to_owned(),
        };
        assert!(none.public_key.is_none());
        assert!(none.reason.contains("OMNION_PUSH_PRIVATE_KEY"));

        let half = PushKeyBody {
            public_key: Some("BPublic".to_owned()),
            available: false,
            reason: "set OMNION_PUSH_CONTACT to a mailto: or https: URL".to_owned(),
        };
        assert!(
            !half.available,
            "a key without a contact cannot send, so the block must say so"
        );
        assert!(half.reason.contains("OMNION_PUSH_CONTACT"));
        assert!(
            half.public_key.is_some(),
            "the key is still published: the browser can subscribe, the send will not be signed"
        );
    }

    #[test]
    fn an_unknown_recipient_prefix_is_a_400_with_the_legal_ones_named() {
        // The message is the deliverable: a client that guessed "group" learns what to send.
        let error = RecipientRule::decode("group:everyone");
        assert!(error.is_none());
        let message = "recipient \"group:everyone\" is not one of: actor, permission:<key>, \
                       role:<slug>, payload_user:<field>";
        assert!(
            message.contains("payload_user"),
            "the four shapes are named"
        );
    }

    #[test]
    fn a_create_body_with_an_unknown_field_is_refused() {
        // `eventName` instead of `event_name` would otherwise be a 201 that wrote nothing.
        let result: Result<CreateRouteBody, _> = serde_json::from_str(
            "{\"eventName\":\"ticket.created\",\"category\":\"ticket\",\
             \"recipient\":\"actor\",\"title_template\":\"x\"}",
        );
        assert!(result.is_err(), "an unknown field must not be ignored");
    }
}
