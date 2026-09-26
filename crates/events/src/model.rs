//! Rows and request shapes of the event bus.

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

/// Columns of `events` for one `select`, in [`Event`] order.
pub const EVENT_COLUMNS: &str = "id, name, organization_id, site_id, actor_user_id, payload, \
     created_at";

/// Columns of `webhook_endpoints` for one `select`, in [`WebhookEndpoint`] order.
pub const ENDPOINT_COLUMNS: &str = "id, organization_id, name, url, secret, events, enabled, \
     created_by, created_at, updated_at";

/// Columns of `webhook_deliveries` for one `select`, in [`Delivery`] order. The caller joins
/// `events` for the name, so both sides of the join carry their alias.
pub const DELIVERY_COLUMNS: &str = "d.id, d.endpoint_id, d.event_id, e.name as event_name, \
     d.status, d.attempts, d.max_attempts, d.next_attempt_at, d.response_status, d.error, \
     d.delivered_at, d.created_at";

/// How many attempts a delivery gets when it is queued.
///
/// The queue row carries the number, not the runner: an operator can look at a delivery and
/// see what it was allowed, and a future per-endpoint override changes the queued value only.
/// Five attempts with the default backoff cover a receiver that restarts without ever turning a
/// webhook queue into an unbounded retry loop.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 5;

/// One recorded fact of the platform.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Event {
    /// Creation order (identity column).
    pub id: i64,
    /// Dotted, lower-case event name, e.g. `page.published`.
    pub name: String,
    /// Organization the fact belongs to; fan-out matches endpoints on it.
    pub organization_id: Option<Uuid>,
    /// Site the fact happened on, when it happened on one.
    pub site_id: Option<Uuid>,
    /// Account that caused it, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// Structured detail a consumer reads; never carries secrets.
    pub payload: Value,
    /// When the platform recorded it.
    pub created_at: OffsetDateTime,
}

/// An event to record, built with the `with_*` style setters.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// Dotted, lower-case name (`page.published`).
    pub name: String,
    /// Organization the fact belongs to.
    pub organization_id: Option<Uuid>,
    /// Site the fact happened on.
    pub site_id: Option<Uuid>,
    /// Account that caused it.
    pub actor_user_id: Option<Uuid>,
    /// Structured detail.
    pub payload: Value,
}

impl NewEvent {
    /// An event of one name, with nothing attached yet.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            organization_id: None,
            site_id: None,
            actor_user_id: None,
            payload: Value::Object(serde_json::Map::new()),
        }
    }

    /// Set the organization the fact belongs to.
    #[must_use]
    pub fn organization(mut self, organization_id: impl Into<Option<Uuid>>) -> Self {
        self.organization_id = organization_id.into();
        self
    }

    /// Set the site the fact happened on.
    #[must_use]
    pub fn site(mut self, site_id: impl Into<Option<Uuid>>) -> Self {
        self.site_id = site_id.into();
        self
    }

    /// Set the account that caused the fact.
    #[must_use]
    pub fn actor(mut self, actor_user_id: impl Into<Option<Uuid>>) -> Self {
        self.actor_user_id = actor_user_id.into();
        self
    }

    /// Attach the structured detail.
    #[must_use]
    pub fn payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

/// One webhook endpoint: where an organization wants its events delivered.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct WebhookEndpoint {
    /// Primary key.
    pub id: Uuid,
    /// Organization the endpoint belongs to.
    pub organization_id: Uuid,
    /// Name the operator tells it apart by, unique per organization.
    pub name: String,
    /// URL the platform POSTs deliveries to.
    pub url: String,
    /// Shared secret deliveries are signed with. Stored, never sent out through the API.
    pub secret: String,
    /// Event names the endpoint subscribed to.
    pub events: Vec<String>,
    /// Whether the platform keeps delivering to it.
    pub enabled: bool,
    /// Account that connected it, when a person did.
    pub created_by: Option<Uuid>,
    /// When it was connected.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl WebhookEndpoint {
    /// Whether this endpoint subscribed to one event name.
    #[must_use]
    pub fn subscribed_to(&self, name: &str) -> bool {
        self.events.iter().any(|entry| entry == name)
    }
}

/// An endpoint to create, validated before it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEndpoint {
    /// Organization the endpoint belongs to.
    pub organization_id: Uuid,
    /// Display name, unique per organization.
    pub name: String,
    /// URL deliveries go to.
    pub url: String,
    /// Signing secret.
    pub secret: String,
    /// Subscribed event names.
    pub events: Vec<String>,
    /// Account connecting it.
    pub created_by: Option<Uuid>,
}

/// The changes one `PATCH` may carry; `None` fields stay as they are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EndpointChanges {
    /// New name.
    pub name: Option<String>,
    /// New URL.
    pub url: Option<String>,
    /// New subscription list.
    pub events: Option<Vec<String>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// New signing secret (a rotation; the old one stops working).
    pub secret: Option<String>,
}

impl EndpointChanges {
    /// `true` when the change set would write nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.url.is_none()
            && self.events.is_none()
            && self.enabled.is_none()
            && self.secret.is_none()
    }
}

/// How a delivery ended; mirrors the `webhook_deliveries.status` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Queued, waiting for its next attempt.
    Pending,
    /// The receiver accepted it.
    Delivered,
    /// Out of attempts, or the endpoint was switched off before it went out.
    Failed,
}

impl DeliveryStatus {
    /// Value stored in `webhook_deliveries.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }

    /// Parse a stored status.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One delivery of one event to one endpoint, as the panel reads it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Delivery {
    /// Primary key.
    pub id: Uuid,
    /// Endpoint it belongs to.
    pub endpoint_id: Uuid,
    /// Event it carries.
    pub event_id: i64,
    /// Name of that event (joined for the panel).
    pub event_name: String,
    /// `pending`, `delivered` or `failed`.
    pub status: String,
    /// Attempts made so far.
    pub attempts: i32,
    /// Attempts allowed.
    pub max_attempts: i32,
    /// When the next attempt is due.
    pub next_attempt_at: OffsetDateTime,
    /// Status the receiver answered with, when it answered.
    pub response_status: Option<i32>,
    /// Why the delivery failed, when it did.
    pub error: Option<String>,
    /// When the receiver accepted it.
    pub delivered_at: Option<OffsetDateTime>,
    /// When it was queued.
    pub created_at: OffsetDateTime,
}

/// One delivery the runner claimed, with the endpoint and the event it needs to send it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryJob {
    /// Delivery id (goes in the `X-Omnion-Delivery` header).
    pub delivery_id: Uuid,
    /// Endpoint the delivery belongs to.
    pub endpoint_id: Uuid,
    /// Endpoint name, for logs.
    pub endpoint_name: String,
    /// URL to POST to.
    pub endpoint_url: String,
    /// Secret the delivery is signed with.
    pub endpoint_secret: String,
    /// Event id.
    pub event_id: i64,
    /// Event name.
    pub event_name: String,
    /// Event payload.
    pub event_payload: Value,
    /// Organization the event belongs to.
    pub organization_id: Option<Uuid>,
    /// Site the event happened on.
    pub site_id: Option<Uuid>,
    /// Account that caused the event.
    pub actor_user_id: Option<Uuid>,
    /// When the event was recorded.
    pub event_created_at: OffsetDateTime,
    /// Attempts made so far, including the one in flight.
    pub attempts: i32,
    /// Attempts allowed in total.
    pub max_attempts: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_matched_by_exact_name() {
        let endpoint = WebhookEndpoint {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Receiver".to_owned(),
            url: "https://example.test/hook".to_owned(),
            secret: "0123456789abcdef".to_owned(),
            events: vec!["page.published".to_owned()],
            enabled: true,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        assert!(endpoint.subscribed_to("page.published"));
        assert!(!endpoint.subscribed_to("page.updated"));
    }

    #[test]
    fn delivery_status_round_trips() {
        for status in [
            DeliveryStatus::Pending,
            DeliveryStatus::Delivered,
            DeliveryStatus::Failed,
        ] {
            assert_eq!(DeliveryStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(DeliveryStatus::parse("in-flight"), None);
    }

    #[test]
    fn an_empty_change_set_says_so() {
        assert!(EndpointChanges::default().is_empty());
        assert!(
            !EndpointChanges {
                enabled: Some(false),
                ..EndpointChanges::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn a_new_event_is_built_with_the_setters() {
        let organization = Uuid::new_v4();
        let event = NewEvent::new("page.published")
            .organization(organization)
            .payload(serde_json::json!({ "slug": "home" }));

        assert_eq!(event.name, "page.published");
        assert_eq!(event.organization_id, Some(organization));
        assert_eq!(event.payload["slug"], serde_json::json!("home"));
    }
}
