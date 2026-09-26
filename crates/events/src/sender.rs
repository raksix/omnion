//! One delivery's HTTP round trip.
//!
//! The sender signs the exact bytes it sends: the body is serialized once and both the request
//! body and the signature are computed from those bytes, so a receiver that verifies the
//! signature over its raw body always sees the same material — the failure mode where a
//! re-serialized body verifies in one language and not in another cannot happen.

use std::time::Duration as StdDuration;

use reqwest::header;
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{EventsError, Result};
use crate::model::DeliveryJob;
use crate::signature;

/// How much of a receiver's answer travels back into the delivery row.
const MAX_REPORTED_BODY: usize = 200;

/// The JSON body a receiver gets: the event exactly as recorded, no wrapping.
#[derive(Debug, Serialize)]
pub struct EventEnvelope<'a> {
    /// Event id.
    pub id: i64,
    /// Event name, e.g. `page.published`.
    pub name: &'a str,
    /// Organization the fact belongs to.
    pub organization_id: Option<Uuid>,
    /// Site the fact happened on.
    pub site_id: Option<Uuid>,
    /// Account that caused the fact.
    pub actor_user_id: Option<Uuid>,
    /// When the platform recorded it.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Structured detail.
    pub payload: &'a serde_json::Value,
}

/// How one delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The receiver accepted it (2xx).
    Delivered {
        /// Status the receiver answered with.
        status: u16,
    },
    /// The receiver answered without success, or could not be reached.
    Failed {
        /// Status the receiver answered with, when it answered at all.
        status: Option<u16>,
        /// Why the attempt failed, trimmed for the delivery row.
        message: String,
    },
}

/// Build the HTTP client deliveries go through.
pub fn client(request_timeout: StdDuration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(StdDuration::from_secs(5))
        .timeout(request_timeout)
        .user_agent(concat!("omnion-webhooks/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| EventsError::Client(error.to_string()))
}

/// POST one delivery, signed.
pub async fn deliver(client: &reqwest::Client, job: &DeliveryJob) -> DeliveryOutcome {
    let body = match serde_json::to_vec(&EventEnvelope {
        id: job.event_id,
        name: &job.event_name,
        organization_id: job.organization_id,
        site_id: job.site_id,
        actor_user_id: job.actor_user_id,
        created_at: job.event_created_at,
        payload: &job.event_payload,
    }) {
        Ok(body) => body,
        Err(error) => {
            return DeliveryOutcome::Failed {
                status: None,
                message: format!("the delivery body could not be serialized: {error}"),
            };
        }
    };

    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    let signature = signature::signature_header(&job.endpoint_secret, timestamp, &body);

    let response = client
        .post(&job.endpoint_url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(signature::EVENT_HEADER, &job.event_name)
        .header(signature::DELIVERY_HEADER, job.delivery_id.to_string())
        .header(signature::TIMESTAMP_HEADER, timestamp.to_string())
        .header(signature::SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => DeliveryOutcome::Delivered {
            status: response.status().as_u16(),
        },
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            DeliveryOutcome::Failed {
                status: Some(status),
                message: format!(
                    "the receiver answered {status}: {}",
                    trim(&body, MAX_REPORTED_BODY)
                ),
            }
        }
        Err(error) => DeliveryOutcome::Failed {
            status: None,
            message: format!("the receiver could not be reached: {error}"),
        },
    }
}

/// Trim a string to at most `max` characters, collapsing newlines so a delivery row stays one
/// readable line.
#[must_use]
pub fn trim(text: &str, max: usize) -> String {
    let collapsed: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = collapsed.trim();

    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }

    let mut short: String = trimmed.chars().take(max).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_keeps_one_readable_line() {
        assert_eq!(trim("all good", 20), "all good");
        assert_eq!(trim("first\nsecond", 40), "first second");
        assert_eq!(trim(&"x".repeat(30), 10), format!("{}…", "x".repeat(10)));
    }

    #[test]
    fn the_envelope_serializes_the_event_as_recorded() {
        let job = DeliveryJob {
            delivery_id: Uuid::nil(),
            endpoint_id: Uuid::nil(),
            endpoint_name: "Receiver".to_owned(),
            endpoint_url: "https://example.test/hook".to_owned(),
            endpoint_secret: "0123456789abcdef".to_owned(),
            event_id: 7,
            event_name: "page.published".to_owned(),
            event_payload: serde_json::json!({ "slug": "home" }),
            organization_id: Some(Uuid::nil()),
            site_id: None,
            actor_user_id: None,
            event_created_at: OffsetDateTime::UNIX_EPOCH,
            attempts: 1,
            max_attempts: 5,
        };

        let body = serde_json::to_vec(&EventEnvelope {
            id: job.event_id,
            name: &job.event_name,
            organization_id: job.organization_id,
            site_id: job.site_id,
            actor_user_id: job.actor_user_id,
            created_at: job.event_created_at,
            payload: &job.event_payload,
        })
        .expect("the envelope serializes");

        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(parsed["id"], serde_json::json!(7));
        assert_eq!(parsed["name"], serde_json::json!("page.published"));
        assert_eq!(parsed["payload"]["slug"], serde_json::json!("home"));
        assert_eq!(
            parsed["created_at"],
            serde_json::json!("1970-01-01T00:00:00Z")
        );
        assert!(
            parsed.get("attempts").is_none(),
            "a receiver learns about the event, not about Omnion's retry bookkeeping"
        );
    }
}
