//! SQL behind the event bus and the delivery queue.
//!
//! The store is deliberately explicit SQL rather than an ORM, because the queue's correctness
//! lives in a handful of statements: record the event and queue its fan-out in one transaction,
//! claim due deliveries so two runners never take the same row (`for update skip locked`, with
//! a lease), and settle a delivery exactly once (delivered, retried, failed).

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{EventsError, Result};
use crate::model::{
    DEFAULT_MAX_ATTEMPTS, DELIVERY_COLUMNS, Delivery, DeliveryJob, ENDPOINT_COLUMNS, EVENT_COLUMNS,
    EndpointChanges, Event, NewEndpoint, NewEvent, WebhookEndpoint,
};

/// Record one event inside an existing transaction.
pub async fn insert_event(executor: impl sqlx::PgExecutor<'_>, new: NewEvent) -> Result<Event> {
    let sql = format!(
        "insert into events (name, organization_id, site_id, actor_user_id, payload) \
         values ($1, $2, $3, $4, $5) returning {EVENT_COLUMNS}"
    );

    let event = sqlx::query_as(&sql)
        .bind(new.name.as_str())
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(new.actor_user_id)
        .bind(new.payload)
        .fetch_one(executor)
        .await?;

    Ok(event)
}

/// Queue one delivery per subscribed, enabled endpoint of the event's organization.
///
/// An event without an organization (a platform-level fact) fans out to nobody: endpoints
/// belong to organizations, and matching a tenant's endpoint against a fact that belongs to no
/// tenant would leak. The unique index on `(endpoint_id, event_id)` makes the statement
/// idempotent.
pub async fn enqueue_fanout(executor: impl sqlx::PgExecutor<'_>, event: &Event) -> Result<u64> {
    let Some(organization_id) = event.organization_id else {
        return Ok(0);
    };

    let result = sqlx::query(
        "insert into webhook_deliveries (endpoint_id, event_id, max_attempts) \
         select w.id, $1, $4 from webhook_endpoints w \
         where w.enabled and w.organization_id = $2 and $3 = any (w.events) \
         on conflict (endpoint_id, event_id) do nothing",
    )
    .bind(event.id)
    .bind(organization_id)
    .bind(event.name.as_str())
    .bind(DEFAULT_MAX_ATTEMPTS)
    .execute(executor)
    .await?;

    Ok(result.rows_affected())
}

/// Queue a delivery of one event to exactly these endpoints.
///
/// Used by the operator's test delivery, which is explicit by design: it reaches endpoints the
/// event name would not have matched, and it does not care whether the endpoint is enabled —
/// testing a receiver before switching it on is the point.
pub async fn enqueue_for_endpoints(
    executor: impl sqlx::PgExecutor<'_>,
    event: &Event,
    endpoint_ids: &[Uuid],
) -> Result<u64> {
    if endpoint_ids.is_empty() {
        return Ok(0);
    }

    let result = sqlx::query(
        "insert into webhook_deliveries (endpoint_id, event_id, max_attempts) \
         select w.id, $1, $3 from webhook_endpoints w where w.id = any ($2) \
         on conflict (endpoint_id, event_id) do nothing",
    )
    .bind(event.id)
    .bind(endpoint_ids.to_vec())
    .bind(DEFAULT_MAX_ATTEMPTS)
    .execute(executor)
    .await?;

    Ok(result.rows_affected())
}

/// Connect one endpoint.
pub async fn insert_endpoint(pool: &PgPool, new: NewEndpoint) -> Result<WebhookEndpoint> {
    let name = new.name.clone();
    let sql = format!(
        "insert into webhook_endpoints (organization_id, name, url, secret, events, created_by) \
         values ($1, $2, $3, $4, $5, $6) returning {ENDPOINT_COLUMNS}"
    );

    sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.name.as_str())
        .bind(new.url.as_str())
        .bind(new.secret.as_str())
        .bind(new.events)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| name_conflict(error, &name))
}

/// One endpoint by id.
pub async fn find_endpoint(pool: &PgPool, id: Uuid) -> Result<Option<WebhookEndpoint>> {
    let sql = format!("select {ENDPOINT_COLUMNS} from webhook_endpoints where id = $1");
    let endpoint = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(endpoint)
}

/// The endpoints of one organization, name order — or every endpoint when no organization is
/// given (the platform view).
pub async fn list_endpoints(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<WebhookEndpoint>> {
    let sql = format!(
        "select {ENDPOINT_COLUMNS} from webhook_endpoints \
         where ($1::uuid is null or organization_id = $1) \
         order by lower(name), created_at"
    );
    let endpoints = sqlx::query_as(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(endpoints)
}

/// Apply a change set; `None` when no endpoint carries that id.
pub async fn update_endpoint(
    pool: &PgPool,
    id: Uuid,
    changes: EndpointChanges,
) -> Result<Option<WebhookEndpoint>> {
    let name = changes.name.clone();
    let sql = format!(
        "update webhook_endpoints set \
             name = coalesce($2, name), \
             url = coalesce($3, url), \
             events = coalesce($4, events), \
             enabled = coalesce($5, enabled), \
             secret = coalesce($6, secret), \
             updated_at = now() \
         where id = $1 returning {ENDPOINT_COLUMNS}"
    );

    sqlx::query_as(&sql)
        .bind(id)
        .bind(changes.name.as_deref())
        .bind(changes.url.as_deref())
        .bind(changes.events)
        .bind(changes.enabled)
        .bind(changes.secret.as_deref())
        .fetch_optional(pool)
        .await
        .map_err(|error| match &name {
            Some(name) => name_conflict(error, name),
            None => EventsError::Store(error),
        })
}

/// Disconnect one endpoint; `false` when it was already gone.
pub async fn delete_endpoint(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("delete from webhook_endpoints where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// The deliveries of one endpoint, newest first.
pub async fn list_deliveries(
    pool: &PgPool,
    endpoint_id: Uuid,
    limit: i64,
) -> Result<Vec<Delivery>> {
    let sql = format!(
        "select {DELIVERY_COLUMNS} from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where d.endpoint_id = $1 \
         order by d.created_at desc, d.id desc \
         limit $2"
    );
    let deliveries = sqlx::query_as(&sql)
        .bind(endpoint_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(deliveries)
}

/// Recorded events, newest first, optionally filtered by organization and exact name.
pub async fn list_events(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    name: Option<&str>,
    limit: i64,
) -> Result<Vec<Event>> {
    let sql = format!(
        "select {EVENT_COLUMNS} from events \
         where ($1::uuid is null or organization_id = $1) \
           and ($2::text is null or name = $2) \
         order by id desc \
         limit $3"
    );
    let events = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(name)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(events)
}

/// Claim up to `batch` deliveries that are due, and return them with everything needed to send.
///
/// A claim increments `attempts` and stamps `claimed_at`; a claim older than the lease is
/// treated as abandoned (the process that held it died), so the delivery comes back to another
/// runner. The attempt that died still counts — that is what keeps a crash loop from retrying
/// forever.
pub async fn claim_due(pool: &PgPool, batch: i64, lease_seconds: f64) -> Result<Vec<DeliveryJob>> {
    let claimed: Vec<Uuid> = sqlx::query_scalar(
        "with due as ( \
             select d.id from webhook_deliveries d \
             join webhook_endpoints w on w.id = d.endpoint_id \
             where d.status = 'pending' and w.enabled and d.next_attempt_at <= now() \
               and (d.claimed_at is null or d.claimed_at <= now() - make_interval(secs => $2)) \
             order by d.next_attempt_at asc, d.created_at asc \
             limit $1 \
             for update of d skip locked \
         ) \
         update webhook_deliveries d set attempts = d.attempts + 1, claimed_at = now() \
         from due where d.id = due.id \
         returning d.id",
    )
    .bind(batch)
    .bind(lease_seconds)
    .fetch_all(pool)
    .await?;

    if claimed.is_empty() {
        return Ok(Vec::new());
    }

    let sql = "select d.id as delivery_id, d.endpoint_id, w.name as endpoint_name, \
                      w.url as endpoint_url, w.secret as endpoint_secret, d.event_id, \
                      e.name as event_name, e.payload as event_payload, e.organization_id, \
                      e.site_id, e.actor_user_id, e.created_at as event_created_at, \
                      d.attempts, d.max_attempts \
               from webhook_deliveries d \
               join webhook_endpoints w on w.id = d.endpoint_id \
               join events e on e.id = d.event_id \
               where d.id = any ($1) \
               order by d.next_attempt_at asc, d.created_at asc";

    let jobs: Vec<DeliveryJob> = sqlx::query_as(sql).bind(claimed).fetch_all(pool).await?;
    Ok(jobs)
}

/// Settle the pending deliveries of endpoints that were switched off while the queue waited.
///
/// Left alone, those rows would sit pending forever and the panel would show a queue that
/// never moves; settling them as failed answers "why did nothing arrive" directly.
pub async fn cancel_pending_for_disabled(pool: &PgPool, lease_seconds: f64) -> Result<u64> {
    let result = sqlx::query(
        "update webhook_deliveries d \
         set status = 'failed', claimed_at = null, \
             error = 'the endpoint was switched off before the delivery went out' \
         from webhook_endpoints w \
         where w.id = d.endpoint_id and not w.enabled and d.status = 'pending' \
           and (d.claimed_at is null or d.claimed_at <= now() - make_interval(secs => $1))",
    )
    .bind(lease_seconds)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Record that a receiver accepted a delivery.
pub async fn mark_delivered(pool: &PgPool, id: Uuid, response_status: Option<i32>) -> Result<()> {
    sqlx::query(
        "update webhook_deliveries \
         set status = 'delivered', delivered_at = now(), response_status = $2, \
             error = null, claimed_at = null \
         where id = $1",
    )
    .bind(id)
    .bind(response_status)
    .execute(pool)
    .await?;

    Ok(())
}

/// Record one failed attempt and schedule the next one.
pub async fn mark_retry(
    pool: &PgPool,
    id: Uuid,
    response_status: Option<i32>,
    error: &str,
    next_attempt_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "update webhook_deliveries \
         set next_attempt_at = $2, response_status = $3, error = $4, claimed_at = null \
         where id = $1",
    )
    .bind(id)
    .bind(next_attempt_at)
    .bind(response_status)
    .bind(error)
    .execute(pool)
    .await?;

    Ok(())
}

/// Record that a delivery ran out of attempts (or was cancelled).
pub async fn mark_failed(
    pool: &PgPool,
    id: Uuid,
    response_status: Option<i32>,
    error: &str,
) -> Result<()> {
    sqlx::query(
        "update webhook_deliveries \
         set status = 'failed', response_status = $2, error = $3, claimed_at = null \
         where id = $1",
    )
    .bind(id)
    .bind(response_status)
    .bind(error)
    .execute(pool)
    .await?;

    Ok(())
}

/// Exponential backoff, the same ladder the workflow engine uses for a failing step: attempt 1
/// waits one base, attempt 2 two bases, attempt 3 four … never more than `max`, and a cap below
/// the base is raised to the base.
#[must_use]
pub fn retry_delay(attempt: i32, base: Duration, max: Duration) -> Duration {
    let shift = u32::try_from(attempt.clamp(1, 16) - 1).unwrap_or(0);
    let factor = 1_i64.checked_shl(shift).unwrap_or(i64::MAX);
    let base_ms = base.whole_milliseconds().max(1) as i64;
    let cap_ms = (max.whole_milliseconds() as i64).max(base_ms);
    Duration::milliseconds(base_ms.saturating_mul(factor).min(cap_ms))
}

/// Start of a claim window: the value the store compares against.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Turn a unique violation on the endpoint name into the API's own error.
fn name_conflict(error: sqlx::Error, name: &str) -> EventsError {
    if let sqlx::Error::Database(ref database) = error
        && database.constraint() == Some("webhook_endpoints_org_name_key")
    {
        return EventsError::EndpointNameTaken(name.to_owned());
    }

    EventsError::Store(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_ladder_doubles_and_caps() {
        let base = Duration::seconds(15);
        let cap = Duration::minutes(15);

        assert_eq!(retry_delay(1, base, cap), Duration::seconds(15));
        assert_eq!(retry_delay(2, base, cap), Duration::seconds(30));
        assert_eq!(retry_delay(3, base, cap), Duration::seconds(60));
        assert_eq!(retry_delay(4, base, cap), Duration::seconds(120));
        assert_eq!(retry_delay(20, base, cap), cap, "the cap holds");
        assert_eq!(
            retry_delay(1, base, Duration::milliseconds(0)),
            base,
            "a cap below the base is raised to the base"
        );
    }
}
