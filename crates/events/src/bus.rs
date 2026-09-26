//! Recording events: one call writes the fact and queues its deliveries.
//!
//! Everything that happens on the platform that other software should hear about goes through
//! here. An emission is either both the event row and its queued deliveries, or neither — the
//! two writes run in one transaction, so a delivery queue can never reference an event that
//! does not exist, and an event can never exist without the deliveries its subscribers expect.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{EventsError, Result};
use crate::model::{Event, NewEvent};
use crate::{store, validation};

/// What one emission did.
#[derive(Debug, Clone, PartialEq)]
pub struct EmitReport {
    /// The event that was recorded.
    pub event: Event,
    /// Deliveries queued by this emission.
    pub deliveries: u64,
}

/// Record an event and queue one delivery per subscribed, enabled endpoint of its organization.
pub async fn emit(pool: &PgPool, new: NewEvent) -> Result<EmitReport> {
    let new = prepare(new)?;

    let mut transaction = pool.begin().await?;
    let event = store::insert_event(&mut *transaction, new).await?;
    let deliveries = store::enqueue_fanout(&mut *transaction, &event).await?;
    transaction.commit().await?;

    tracing::info!(
        event_id = event.id,
        name = %event.name,
        deliveries,
        "event recorded"
    );

    Ok(EmitReport { event, deliveries })
}

/// Record an event and queue it to exactly these endpoints.
///
/// The operator's test delivery uses this: it proves the receiver accepts a signed body before
/// the endpoint is switched on for real traffic.
pub async fn emit_to(pool: &PgPool, new: NewEvent, endpoint_ids: &[Uuid]) -> Result<EmitReport> {
    let new = prepare(new)?;

    let mut transaction = pool.begin().await?;
    let event = store::insert_event(&mut *transaction, new).await?;
    let deliveries = store::enqueue_for_endpoints(&mut *transaction, &event, endpoint_ids).await?;
    transaction.commit().await?;

    tracing::info!(
        event_id = event.id,
        name = %event.name,
        deliveries,
        targets = endpoint_ids.len(),
        "targeted event recorded"
    );

    Ok(EmitReport { event, deliveries })
}

/// Validate an event before anything is written.
fn prepare(mut new: NewEvent) -> Result<NewEvent> {
    new.name = validation::validate_event_name(&new.name)?;
    if !new.payload.is_object() {
        return Err(EventsError::invalid_event(
            "the payload must be a JSON object",
        ));
    }

    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_payload_must_be_an_object() {
        let bad = NewEvent::new("page.published").payload(json!(["not", "an", "object"]));
        assert!(prepare(bad).is_err());

        let good = NewEvent::new("page.published").payload(json!({ "slug": "home" }));
        assert!(prepare(good).is_ok());
    }

    #[test]
    fn the_event_name_is_validated() {
        assert!(prepare(NewEvent::new("PagePublished")).is_err());
        assert!(prepare(NewEvent::new("page.published")).is_ok());
    }
}
