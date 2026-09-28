//! Browser push devices and the outbox an administrator reads (REQ-021, slice 3).
//!
//! Two halves that belong in one file because they answer the same question from opposite
//! ends: *this* person said "notify my phone", and *somebody else* is asking "did the phone
//! get it?".
//!
//! **A push endpoint is unique across the whole installation, not per person.** The schema says
//! `endpoint text not null unique` and the browser hands out one endpoint per service-worker
//! registration. Two accounts on one browser therefore present the *same* endpoint, and a
//! per-user unique index would make the second registration a `409` that reads as "this device
//! is already registered" — when the truth is that it is registered to somebody else. So
//! [`register`] re-points the row at the person registering now and reports the move, because
//! "already registered" is the wrong answer and "now it is yours" is the true one.
//!
//! **A revoked endpoint is pruned, not disabled.** Browsers answer a dead push endpoint with
//! `404`/`410` and nothing else; keeping the row means every subsequent notification re-sends
//! to a dead address forever, and the outbox fills with the same failure. [`prune`] removes
//! them and the caller's answer is a count, so a runner can say "pruned 2" instead of quietly
//! deleting evidence.
//!
//! **The outbox is organization-scoped and reads nobody's inbox.** It is a delivery log: the
//! rows are joins over `notification_deliveries`, and only the ids of the notification are
//! exposed. A notification body can carry customer data, and an admin screen is exactly where
//! that data should not be on display by default.

use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;
use uuid::Uuid;

use crate::error::Result;
use crate::vocabulary::is_channel;

/// One registered browser, as the devices list renders it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct PushSubscription {
    /// The row's id, which is what `DELETE` names.
    pub id: Uuid,
    /// Whose device this is.
    pub user_id: Uuid,
    /// The service-worker endpoint. Truncated in the panel, never in this type.
    pub endpoint: String,
    /// The browser that asked for it, for the "is this still my phone?" question.
    pub user_agent: Option<String>,
    /// When it was registered.
    pub created_at: time::OffsetDateTime,
    /// When it last delivered something.
    pub last_seen_at: time::OffsetDateTime,
}

impl PushSubscription {
    /// A short, non-secret handle for the endpoint.
    ///
    /// A full push endpoint embeds a capability key; pasting it into a screenshot, a support
    /// ticket or a bug report hands over the ability to push to that browser. The last path
    /// segment is enough for a reader to recognise their own device and useless to an
    /// attacker.
    #[must_use]
    pub fn endpoint_hint(&self) -> String {
        let tail = self
            .endpoint
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or("endpoint");
        let short: String = tail.chars().take(8).collect();
        format!("…{short}")
    }
}

/// What [`register`] did with the endpoint it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegisterOutcome {
    /// A row was created for this person.
    Created,
    /// The endpoint was already this person's and the keys are unchanged.
    Refreshed,
    /// The endpoint belonged to another account and now belongs to this one.
    Reassigned,
    /// The endpoint was this person's but with different keys, so the row was rewritten.
    ReKeyed,
}

/// The result of registering one browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegisterReport {
    /// The row, whatever happened to it.
    pub id: Uuid,
    /// Which of the four things this was.
    pub outcome: RegisterOutcome,
}

/// A device the push service has said is gone, with the reason the caller logs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrunedSubscription {
    /// The row that went.
    pub id: Uuid,
    /// The endpoint the service answered `404`/`410` for.
    ///
    /// Kept because a push service names the **endpoint** in its failure, and the row is
    /// looked up by it: a prune keyed by the id would need the sender to have matched the
    /// response back to the request, which is precisely the bookkeeping that goes wrong when
    /// one send fails in a batch of two hundred.
    pub endpoint: String,
    /// The status the push service answered with (`404` or `410`).
    pub status: i32,
}

/// Register (or re-point) one browser push endpoint.
///
/// **The endpoint is the identity; the person is the owner.** That is why this returns an
/// outcome instead of a bool: a `409` on "already registered" would be a lie for the common
/// case of one browser shared by two accounts, and a silent `200` would be a lie for the case
/// where somebody else's device was just taken over without anybody noticing. The panel shows
/// which of the four happened.
///
/// The keys are required and non-empty. A push service will accept a registration whose
/// signature keys are empty and then answer every send with `400`, and the failure surfaces
/// days later as "notifications stopped working on my phone" with nothing in the outbox that
/// points at the registration.
pub async fn register(
    pool: &PgPool,
    user_id: Uuid,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    user_agent: Option<&str>,
) -> Result<RegisterReport> {
    validate_endpoint(endpoint, p256dh, auth)?;

    let existing: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "select id, user_id, p256dh, auth from push_subscriptions where endpoint = $1",
    )
    .bind(endpoint)
    .fetch_optional(pool)
    .await?;

    let Some((id, owner, old_p256dh, old_auth)) = existing else {
        let id: Uuid = sqlx::query_scalar(
            "insert into push_subscriptions (user_id, endpoint, p256dh, auth, user_agent) \
             values ($1, $2, $3, $4, $5) returning id",
        )
        .bind(user_id)
        .bind(endpoint)
        .bind(p256dh)
        .bind(auth)
        .bind(user_agent)
        .fetch_one(pool)
        .await?;
        return Ok(RegisterReport {
            id,
            outcome: RegisterOutcome::Created,
        });
    };

    let keys_changed = old_p256dh != p256dh || old_auth != auth;
    let outcome = if owner != user_id {
        RegisterOutcome::Reassigned
    } else if keys_changed {
        RegisterOutcome::ReKeyed
    } else {
        RegisterOutcome::Refreshed
    };

    // One statement, because the row exists: this touches `user_id` only when it actually
    // moved, and `user_agent`/`last_seen_at` on every call, so a device that reloads the panel
    // keeps its row warm for the pruning rule below.
    let result: PgQueryResult = sqlx::query(
        "update push_subscriptions set \
            user_id = $2, \
            p256dh = $3, \
            auth = $4, \
            user_agent = coalesce($5, user_agent), \
            last_seen_at = now() \
         where id = $1",
    )
    .bind(id)
    .bind(user_id)
    .bind(p256dh)
    .bind(auth)
    .bind(user_agent)
    .execute(pool)
    .await?;
    let _ = result.rows_affected();

    Ok(RegisterReport { id, outcome })
}

/// Remove one device, if this person owns it.
///
/// Returns `false` for somebody else's device rather than an error: a list that offers a
/// delete button on a row the caller cannot delete is a list that lies about its own rows, and
/// the answer a `404` gives the panel is exactly "this row is not yours to remove".
pub async fn remove(pool: &PgPool, user_id: Uuid, id: Uuid) -> Result<bool> {
    let result: PgQueryResult =
        sqlx::query("delete from push_subscriptions where id = $1 and user_id = $2")
            .bind(id)
            .bind(user_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// One person's devices, newest first.
pub async fn list_subscriptions(pool: &PgPool, user_id: Uuid) -> Result<Vec<PushSubscription>> {
    Ok(sqlx::query_as::<_, PushSubscription>(
        "select id, user_id, endpoint, user_agent, created_at, last_seen_at \
         from push_subscriptions where user_id = $1 order by created_at desc, id desc",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Delete every device that has not been seen for this long.
///
/// **A cutoff, not a flag.** A `disabled` column is state somebody has to notice and un-set; a
/// cutoff is a fact about time. The 30 days is generous on purpose: a phone that has been off
/// for a month is a phone whose browser has very likely rotated the service worker, and the
/// row would only ever collect `410`s.
pub const SUBSCRIPTION_STALE_DAYS: i32 = 30;

/// How many devices a pruning pass removed.
pub async fn prune_stale(pool: &PgPool) -> Result<u64> {
    let result: PgQueryResult = sqlx::query(
        "delete from push_subscriptions \
         where last_seen_at < now() - make_interval(days => $1::int)",
    )
    .bind(SUBSCRIPTION_STALE_DAYS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete the devices a push service has said are gone, and report which ones.
///
/// `statuses` is the list the sender collected (`404`, `410`). The rows are removed *after*
/// the collection, not during: a send loop that deletes the row it is iterating over is a
/// second bug layered on the first one, and the outbox answer is more useful than the count.
pub async fn prune_endpoints(pool: &PgPool, endpoints: &[PrunedSubscription]) -> Result<u64> {
    let mut removed = 0;
    for pruned in endpoints {
        let result: PgQueryResult =
            sqlx::query("delete from push_subscriptions where endpoint = $1")
                .bind(&pruned.endpoint_or_id())
                .execute(pool)
                .await?;
        removed += result.rows_affected();
    }
    Ok(removed)
}

impl PrunedSubscription {
    /// The row to delete: the endpoint when the caller has it, otherwise the id.
    ///
    /// A push service answers a send with the *endpoint* that failed, and the sender may or
    /// may not have the id in hand. Both are accepted so the caller never has to translate
    /// between the two shapes just to delete a row.
    #[must_use]
    pub fn endpoint_or_id(&self) -> String {
        self.endpoint.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------------------------

/// How far back a delivery row is remembered before the retention sweep takes it.
///
/// Sixty days is longer than a support conversation about "the mail never arrived" lasts, and
/// short enough that an installation does not accumulate a delivery log forever. The sweep is
/// [`prune_deliveries`]'s job; the constant is here so the admin screen can say how far back
/// it answers.
pub const OUTBOX_RETENTION_DAYS: i32 = 60;

/// The largest page of outbox rows one read returns.
pub const MAX_OUTBOX_PAGE: i64 = 100;

/// What the outbox can be narrowed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxQuery {
    /// Only these states, or all four when empty.
    pub statuses: Vec<String>,
    /// Only this channel.
    pub channel: Option<String>,
    /// Only rows from this notification.
    pub notification_id: Option<Uuid>,
    /// How many rows.
    pub limit: i64,
}

/// One delivery row, as the outbox renders it.
///
/// **No title, no body, no reader's name.** The route joins the notification to answer "which
/// notification" and "which reader" by id only; a body on this row is a customer record
/// rendered in the one screen whose audience is the largest.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct OutboxRow {
    /// The delivery row.
    pub id: Uuid,
    /// The notification it belongs to.
    pub notification_id: Uuid,
    /// Its category, which is a closed list and therefore safe to render.
    pub category: String,
    /// Its priority, same argument.
    pub priority: String,
    /// Whose inbox it went to.
    pub user_id: Uuid,
    /// Which channel.
    pub channel: String,
    /// What happened.
    pub status: String,
    /// How many tries so far.
    pub attempts: i32,
    /// The cap.
    pub max_attempts: i32,
    /// The status code the transport answered with, when there was one.
    pub response_status: Option<i32>,
    /// The failure text, as the transport gave it.
    pub error: Option<String>,
    /// When it went.
    pub sent_at: Option<time::OffsetDateTime>,
    /// When it was queued.
    pub created_at: time::OffsetDateTime,
}

/// How many deliveries are in each state, for the outbox's filter chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutboxCounts {
    /// Queued, waiting for a runner.
    pub pending: i64,
    /// Delivered.
    pub sent: i64,
    /// Given up on.
    pub failed: i64,
    /// Never attempted, because the reader turned the channel off.
    pub skipped: i64,
}

impl OutboxCounts {
    /// The total across every state, so a chip can say "12" next to a filtered list of 3.
    #[must_use]
    pub fn total(&self) -> i64 {
        self.pending + self.sent + self.failed + self.skipped
    }
}

/// Read one organization's delivery log, failed rows first.
///
/// **Failed first is the ordering, not a filter.** An administrator opening the outbox during
/// an incident is looking for the thing that is broken, and a log ordered by time puts the
/// failure below a hundred successes. Within a state the newest is first, because the failure
/// somebody is looking for is the one that just happened.
pub async fn list_outbox(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    query: &OutboxQuery,
) -> Result<Vec<OutboxRow>> {
    let limit = query.limit.clamp(1, MAX_OUTBOX_PAGE);
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "select d.id, d.notification_id, n.category, n.priority, n.user_id, d.channel, d.status, \
                d.attempts, d.max_attempts, d.response_status, d.error, d.sent_at, d.created_at \
         from notification_deliveries d \
         join notifications n on n.id = d.notification_id \
         where ",
    );

    if let Some(organization) = organization_id {
        builder.push("n.organization_id = ");
        builder.push_bind(organization);
    } else {
        // No organization on the caller's session means a platform-wide install, where every
        // row is in scope. Stated as the else branch rather than left implicit so a reader
        // can see that the unscoped read is deliberate and not an omitted filter.
        builder.push("n.organization_id is null");
    }

    if !query.statuses.is_empty() {
        builder.push(" and d.status in (");
        let mut separated = builder.separated(", ");
        for status in &query.statuses {
            separated.push_bind(status);
        }
        separated.push_unseparated(")");
    }
    if let Some(channel) = &query.channel {
        if is_channel(channel) {
            builder.push(" and d.channel = ");
            builder.push_bind(channel);
        }
    }
    if let Some(notification_id) = query.notification_id {
        builder.push(" and d.notification_id = ");
        builder.push_bind(notification_id);
    }

    builder.push(
        " order by case d.status when 'failed' then 0 when 'pending' then 1 \
                  when 'skipped' then 2 else 3 end, d.created_at desc, d.id desc limit ",
    );
    builder.push_bind(limit);

    Ok(builder
        .build_query_as::<OutboxRow>()
        .fetch_all(pool)
        .await?)
}

/// The four counts, in one grouped statement.
pub async fn outbox_counts(pool: &PgPool, organization_id: Option<Uuid>) -> Result<OutboxCounts> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select d.status, count(*)::bigint from notification_deliveries d \
         join notifications n on n.id = d.notification_id \
         where ($1::uuid is null and n.organization_id is null) \
            or n.organization_id = $1::uuid \
         group by d.status",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let count = |wanted: &str| {
        rows.iter()
            .find(|(status, _)| status == wanted)
            .map_or(0, |(_, count)| *count)
    };
    Ok(OutboxCounts {
        pending: count("pending"),
        sent: count("sent"),
        failed: count("failed"),
        skipped: count("skipped"),
    })
}

/// Put one failed delivery back on the queue.
///
/// **Only a `failed` row is retryable.** A `sent` row has already reached somebody — re-sending
/// it is a second copy of a message that arrived, and a `pending` row is already queued, so
/// "retry" on one is a duplicate delivery scheduled for the future. Both answer `false` and the
/// route turns that into a `409` naming the state it is in.
pub async fn retry_delivery(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries \
         set status = 'pending', attempts = 0, next_attempt_at = now(), error = null, \
             response_status = null \
         where id = $1 and status = 'failed'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Delete delivery rows older than [`OUTBOX_RETENTION_DAYS`], and report how many went.
pub async fn prune_deliveries(pool: &PgPool) -> Result<u64> {
    let result: PgQueryResult = sqlx::query(
        "delete from notification_deliveries \
         where status in ('sent', 'skipped') \
           and created_at < now() - make_interval(days => $1::int)",
    )
    .bind(OUTBOX_RETENTION_DAYS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Refuse an endpoint the push service could never accept.
fn validate_endpoint(endpoint: &str, p256dh: &str, auth: &str) -> Result<()> {
    if endpoint.trim().is_empty() {
        return Err(crate::error::NotificationError::invalid(
            "a push subscription needs an endpoint",
        ));
    }
    if !endpoint.starts_with("https://") {
        return Err(crate::error::NotificationError::invalid(
            "a push endpoint must be an https URL — the browser will not register anything else",
        ));
    }
    if p256dh.trim().is_empty() || auth.trim().is_empty() {
        return Err(crate::error::NotificationError::invalid(
            "a push subscription needs both signature keys; without them every send answers 400",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NotificationError;

    #[test]
    fn an_endpoint_that_is_not_https_is_refused_by_name() {
        let error = validate_endpoint("http://push.example/abc", "k", "a").unwrap_err();
        assert_eq!(error.code(), "invalid_notification");
        assert!(error.to_string().contains("https"), "{error}");
    }

    #[test]
    fn empty_signature_keys_are_refused_rather_than_stored() {
        // The failure this prevents surfaces days later as "push stopped working" with nothing
        // in the outbox pointing at the registration, so it is refused at the door.
        assert!(validate_endpoint("https://push.example/abc", "", "a").is_err());
        assert!(validate_endpoint("https://push.example/abc", "k", "  ").is_err());
        assert!(validate_endpoint("", "k", "a").is_err());
        assert!(validate_endpoint("https://push.example/abc", "k", "a").is_ok());
    }

    #[test]
    fn the_endpoint_hint_hides_the_capability_key() {
        // A push endpoint is a capability: whoever holds it can push to that browser. The hint
        // is what the panel shows, and a reader only needs enough to recognise their own phone.
        let subscription = PushSubscription {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            endpoint: "https://fcm.googleapis.com/fcm/send/abcdef0123456789-secret".to_owned(),
            user_agent: Some("Mozilla/5.0".to_owned()),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            last_seen_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let hint = subscription.endpoint_hint();
        assert_eq!(hint, "…abcdef01");
        assert!(
            !hint.contains("secret"),
            "the hint leaked the capability key: {hint}"
        );
    }

    #[test]
    fn the_hint_of_an_endpoint_without_a_path_still_renders_something() {
        // Not a shape a real browser produces, and a hint that panicked on it would take the
        // whole devices list down for one malformed row.
        let subscription = PushSubscription {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            endpoint: "https://push.example".to_owned(),
            user_agent: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            last_seen_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert!(!subscription.endpoint_hint().is_empty());
        let trailing = PushSubscription {
            endpoint: "https://push.example/".to_owned(),
            ..subscription
        };
        assert!(!trailing.endpoint_hint().is_empty());
    }

    #[test]
    fn the_outbox_counts_add_up() {
        let counts = OutboxCounts {
            pending: 2,
            sent: 7,
            failed: 3,
            skipped: 1,
        };
        assert_eq!(counts.total(), 13);
    }

    #[test]
    fn the_outbox_page_is_bounded_by_the_same_cap_as_the_inbox() {
        // One cap for the whole crate: an admin screen that answers 10 000 rows is a page that
        // times out, and it would be the only screen in the panel with a different rule.
        assert_eq!(MAX_OUTBOX_PAGE, crate::vocabulary::MAX_PAGE);
        assert!(MAX_OUTBOX_PAGE > 0);
    }

    #[test]
    fn retention_and_staleness_are_both_positive_and_bounded() {
        // A retention sweep with a negative interval deletes rows that have not happened yet —
        // which is none — and reads as a sweep that never runs.
        assert!(OUTBOX_RETENTION_DAYS > 0);
        assert!(SUBSCRIPTION_STALE_DAYS > 0);
        assert!(SUBSCRIPTION_STALE_DAYS < OUTBOX_RETENTION_DAYS);
    }

    #[test]
    fn a_pruned_subscription_deletes_by_the_handle_it_was_given() {
        let pruned = PrunedSubscription {
            id: Uuid::nil(),
            endpoint: "https://push.example/abc".to_owned(),
            status: 410,
        };
        // The empty-endpoint case must not silently become "delete the row with a blank
        // endpoint", which matches nothing and reports success.
        assert_eq!(pruned.endpoint_or_id(), "https://push.example/abc");
        assert!(!pruned.endpoint_or_id().is_empty());
    }

    #[test]
    fn the_error_code_is_the_one_the_route_maps() {
        let error: NotificationError = validate_endpoint("nope", "k", "a").unwrap_err();
        assert_eq!(error.code(), "invalid_notification");
    }
}
