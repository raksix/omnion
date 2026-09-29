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
    DEFAULT_EVENT_RETENTION_DAYS, DEFAULT_MAX_ATTEMPTS, DELIVERY_COLUMNS, Delivery, DeliveryJob,
    ENDPOINT_COLUMNS, EVENT_COLUMNS, EndpointChanges, Event, NewEndpoint, NewEvent, RetentionRun,
    RetentionStatus, SweepReport, WebhookEndpoint,
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
///
/// **The subscription test carries the group wildcard.** `catalogue::reconcile` stores a
/// `page.*` subscription both as the wildcard and as today's expansion, so a plain
/// `= any (w.events)` would already find today's members. It tests the wildcard anyway, for
/// one honest reason: a row whose list carries only `page.*` — written before reconciliation
/// existed, or by an operator's own SQL — would otherwise silently stop receiving, and the
/// cost of a missing group test is a receiver that never hears about a page again with no
/// error anywhere. The clause is indexed on the same column either way.
pub async fn enqueue_fanout(executor: impl sqlx::PgExecutor<'_>, event: &Event) -> Result<u64> {
    let Some(organization_id) = event.organization_id else {
        return Ok(0);
    };

    let group = event.name.split('.').next().unwrap_or_default();
    let wildcard = format!("{group}.*");

    let result = sqlx::query(
        "insert into webhook_deliveries (endpoint_id, event_id, max_attempts) \
         select w.id, $1, $5 from webhook_endpoints w \
         where w.enabled and w.organization_id = $2 \
           and ($3 = any (w.events) or $4 = any (w.events)) \
         on conflict (endpoint_id, event_id) do nothing",
    )
    .bind(event.id)
    .bind(organization_id)
    .bind(event.name.as_str())
    .bind(wildcard.as_str())
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

    // `trigger` is stamped here rather than left to the column's default. The default is
    // `event`, which is right for the fan-out and wrong here: a row queued by an operator
    // pressing "Test" is not traffic, and the stats read deliberately keeps it out of the
    // success rate. Stamping it at the only place a test is queued is what makes the column
    // true rather than aspirational.
    let result = sqlx::query(
        "insert into webhook_deliveries (endpoint_id, event_id, max_attempts, trigger) \
         select w.id, $1, $3, 'test' from webhook_endpoints w where w.id = any ($2) \
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

/// What a delivery history read is narrowed to.
///
/// Every field is a conjunction except [`Self::statuses`], which is a disjunction: an operator
/// looking at "what broke" asks for `failed` *and* the `pending` rows that are about to break
/// the same way, and reading that as a list of alternatives is the only shape where the
/// question is answerable in one request.
#[derive(Debug, Clone, Default)]
pub struct DeliveryFilter {
    /// Statuses to keep; empty keeps all of them.
    pub statuses: Vec<String>,
    /// Exact event names, any of which may match. Empty matches everything.
    pub names: Vec<String>,
    /// Only deliveries queued at or after this instant.
    pub from: Option<OffsetDateTime>,
    /// Only deliveries queued at or before this instant.
    pub to: Option<OffsetDateTime>,
    /// Substring of the delivery id or the event name, as the operator typed it.
    pub search: Option<String>,
    /// Exclusive upper bound of the page: the created_at of the previous page's last row.
    pub before: Option<(OffsetDateTime, Uuid)>,
    /// How many rows this read may return.
    pub limit: i64,
}

/// One page of a delivery history.
#[derive(Debug)]
pub struct DeliveryPage {
    /// The rows, newest first.
    pub deliveries: Vec<Delivery>,
    /// Whether a further page exists behind the last row of this one.
    pub has_more: bool,
    /// How many rows the filter matches in total.
    ///
    /// A separate count from the list, and that is a deliberate trade: the list answers
    /// "what is on screen" and the count answers "how much of the history does this filter
    /// cover", and a delivery table whose header claims 40 of 40 while showing 25 rows is the
    /// number an operator checks first. The count is taken over the same predicate, so the two
    /// cannot describe different filters.
    pub total: i64,
}

/// The deliveries of one endpoint, newest first, narrowed by [`DeliveryFilter`].
///
/// The cursor is `(created_at, id)` rather than the id alone, and the reason is the ordering:
/// this read sorts by `created_at desc, id desc` because `created_at` is what a human thinks
/// in, and a cursor on a column that is not in the sort can skip or repeat rows whenever two
/// deliveries share a timestamp. The pair is unique, so `(<, =)` on it is a total order and
/// the boundary is exact.
pub async fn list_deliveries_filtered(
    pool: &PgPool,
    endpoint_id: Uuid,
    filter: &DeliveryFilter,
) -> Result<DeliveryPage> {
    let base = format!(
        "from webhook_deliveries d join events e on e.id = d.event_id \
         where d.endpoint_id = $1 \
           and (cardinality($2::text[]) = 0 or d.status = any ($2)) \
           and (cardinality($3::text[]) = 0 or e.name = any ($3)) \
           and ($4::timestamptz is null or d.created_at >= $4) \
           and ($5::timestamptz is null or d.created_at <= $5) \
           and ($6::text is null or d.id::text like $6 or e.name ilike '%' || $6 || '%') \
           and ($7::timestamptz is null or (d.created_at, d.id) < ($7, $8))"
    );

    let search = filter
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (cursor_at, cursor_id) = filter
        .before
        .map(|(at, id)| (Some(at), Some(id)))
        .unwrap_or((None, None));

    let sql = format!(
        "select {DELIVERY_COLUMNS} {base} \
         order by d.created_at desc, d.id desc \
         limit {}",
        (filter.limit.max(1) + 1).to_string()
    );

    let rows: Vec<Delivery> = sqlx::query_as(&sql)
        .bind(endpoint_id)
        .bind(&filter.statuses)
        .bind(&filter.names)
        .bind(filter.from)
        .bind(filter.to)
        .bind(search)
        .bind(cursor_at)
        .bind(cursor_id)
        .fetch_all(pool)
        .await?;

    // One row past the page is what makes `has_more` honest: a second count would be a second
    // query that could disagree with the list, and the disagreement is invisible until somebody
    // pages to the end.
    let has_more = rows.len() as i64 > filter.limit.max(1);
    let mut deliveries = rows;
    if has_more {
        deliveries.truncate(filter.limit.max(1) as usize);
    }

    let count_sql = format!(
        "select count(*) from webhook_deliveries d join events e on e.id = d.event_id \
         where d.endpoint_id = $1 \
           and (cardinality($2::text[]) = 0 or d.status = any ($2)) \
           and (cardinality($3::text[]) = 0 or e.name = any ($3)) \
           and ($4::timestamptz is null or d.created_at >= $4) \
           and ($5::timestamptz is null or d.created_at <= $5) \
           and ($6::text is null or d.id::text like $6 or e.name ilike '%' || $6 || '%')"
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(endpoint_id)
        .bind(&filter.statuses)
        .bind(&filter.names)
        .bind(filter.from)
        .bind(filter.to)
        .bind(search)
        .fetch_one(pool)
        .await?;

    Ok(DeliveryPage {
        deliveries,
        has_more,
        total,
    })
}

/// What one endpoint's delivery history says about its receiver.
///
/// Three windows rather than one, because "is it working" and "was it working an hour ago" are
/// different questions and an operator comparing them is usually chasing a regression.
// No `Eq`: `success_rate` is an `f64`, and `Eq` is not implemented for floats. `PartialEq` is
// all a summary of counts needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeliveryStats {
    /// Rows the receiver accepted, in the window.
    pub delivered: i64,
    /// Rows that ran out of attempts, in the window.
    pub failed: i64,
    /// Rows still waiting, in the window. Counted regardless of window: a queue that stopped
    /// draining is the failure a success rate cannot show, and it must not disappear from a
    /// 7-day window because it started five minutes ago.
    pub pending: i64,
    /// Rows queued, in the window.
    pub total: i64,
    /// Rows in the window an operator asked for by hand (the `test` trigger).
    ///
    /// Reported rather than folded in, because a test delivery is a probe somebody pressed and
    /// not traffic the platform produced. It is counted in `total` — it really is a row in the
    /// history — and left out of `success_rate`, where it would otherwise let an operator make
    /// a broken receiver look healthy by pressing the button.
    pub tests: i64,
    /// Share of settled **non-test** rows that were accepted, 0.0–1.0; `None` when none settled.
    pub success_rate: Option<f64>,
    /// 95th percentile receiver duration in the window; `None` when nothing ran.
    pub p95_duration_ms: Option<i32>,
}

/// Summarise one endpoint's history since `since`.
///
/// `delivered` and `failed` count **traffic** only — rows whose trigger is not `test` — and
/// `success_rate` is their ratio over the settled ones. Two exclusions, each for a reason:
///
/// * `pending` is not in the denominator. It is the single most misleading thing a webhook
///   dashboard can do: a queue that just took a thousand deliveries reads 0% while every one
///   of them is about to succeed, which sends the operator to debug a receiver that is working
///   perfectly.
/// * A `test` row is not in either. It was asked for by a button, and counting it would let an
///   operator make a broken receiver look healthy by pressing the button — the one number on
///   this screen that must not be under the operator's own control.
///
/// `total` still counts every row in the window, tests included, because the history really
/// does contain them and a header that hid them would make the table disagree with its own
/// count.
pub async fn endpoint_stats(
    pool: &PgPool,
    endpoint_id: Uuid,
    since: OffsetDateTime,
) -> Result<DeliveryStats> {
    // The two outcome columns and the two test-aware ones are counted separately: the rate is
    // built from traffic, and `total`/`tests` describe the whole history. `max` is absent on
    // purpose — the slowest single delivery is not a number anybody acts on, and the percentile
    // below is computed from the full sample where it is.
    let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "select \
             count(*) filter (where status = 'delivered' and trigger <> 'test'), \
             count(*) filter (where status = 'failed' and trigger <> 'test'), \
             count(*) filter (where status = 'pending'), \
             count(*), \
             count(*) filter (where trigger = 'test') \
         from webhook_deliveries \
         where endpoint_id = $1 and created_at >= $2",
    )
    .bind(endpoint_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let (delivered, failed, pending, total, tests) = row;

    // The rate's denominator is the *traffic* that actually reached a receiver and got an
    // answer. It excludes `pending` (a queue that just took a thousand deliveries would read
    // 0% while every one of them is about to succeed) and the test rows (a probe somebody
    // pressed is not the platform delivering anything, and counting it would let an operator
    // make a broken receiver look healthy by pressing the button).
    let settled = delivered + failed;

    // The percentile is a second, smaller read rather than a window function: the 95th
    // percentile of the *delivered* rows is the number, and filtering to delivered first means
    // a pile of slow failures cannot drag it — a slow failure is a retry, not a latency budget.
    let durations: Vec<i32> = sqlx::query_scalar(
        "select duration_ms from webhook_deliveries \
         where endpoint_id = $1 and status = 'delivered' and duration_ms is not null \
           and created_at >= $2 and trigger <> 'test' \
         order by duration_ms asc",
    )
    .bind(endpoint_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(DeliveryStats {
        delivered,
        failed,
        pending,
        total,
        tests,
        success_rate: (settled > 0).then(|| delivered as f64 / settled as f64),
        p95_duration_ms: percentile_95(&durations),
    })
}

/// The 95th percentile of a sorted sample, nearest-rank.
///
/// Nearest-rank rather than an interpolation because every value here is a measured
/// millisecond count: reporting `1_047ms` as a latency would be a number no receiver ever
/// produced. An empty sample has no percentile, and `None` is honest where `0` would be a
/// claim.
#[must_use]
pub fn percentile_95(sorted: &[i32]) -> Option<i32> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len()) - 1).copied()
}

/// Why one redelivery was refused, when it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeliverRefusal {
    /// No delivery with that id on this endpoint.
    Unknown,
    /// The row is `pending`: the runner already owns it or is about to, and resetting it would
    /// let a second runner pick up the same row.
    AlreadyPending,
    /// The row has been forced again ten times already.
    OverCap,
}

impl RedeliverRefusal {
    /// Stable, machine-readable code — the API hands this to the panel.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "delivery_not_found",
            Self::AlreadyPending => "delivery_already_pending",
            Self::OverCap => "redeliver_limit_reached",
        }
    }

    /// The sentence the operator reads.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Unknown => "no such delivery on this endpoint",
            Self::AlreadyPending => "this delivery is already queued for another attempt",
            Self::OverCap => "this delivery has already been sent again ten times",
        }
    }
}

/// How many times one delivery may be forced again.
pub const MAX_REDELIVERIES: i32 = 10;

/// Force one delivery again.
///
/// This **resets** the row rather than inserting a second one. The `(endpoint_id, event_id)`
/// unique index would refuse the insert anyway, and it should: a queue holding two rows for the
/// same fact sends it twice and the receiver cannot tell a replay from a duplicate. Resetting
/// is also what makes `attempts` mean "attempts in the current round" rather than "attempts
/// ever", which is the number an operator is comparing against `max_attempts`.
///
/// A `pending` row is refused rather than reset, and that is the one place this operation could
/// double-send: the runner has already claimed it and is holding a lease, so a reset would hand
/// the same row to the next claim while the first attempt is still in flight.
/// Returns the row's new `redeliver_count`, so the caller can report how many times it has now
/// been forced without a second read — and, more importantly, so the number it reports is the
/// one the *update* wrote rather than one a follow-up query might observe after somebody else
/// pressed the button again.
pub async fn redeliver(pool: &PgPool, endpoint_id: Uuid, delivery_id: Uuid) -> Result<i32> {
    // `query_scalar` rather than `query`: this is a single `integer` column, and the count it
    // returns is the number the *update* wrote — the same statement, so there is no window in
    // which a second reader could see a different value.
    //
    // `fetch_optional`, not `fetch_one`: the `where` clause is the whole refusal policy, so
    // "no row" is the normal answer for a pending row or a capped one, not an error.
    let updated: Option<i32> = sqlx::query_scalar(
        "update webhook_deliveries \
         set status = 'pending', attempts = 0, next_attempt_at = now(), \
             claimed_at = null, response_status = null, error = null, \
             delivered_at = null, duration_ms = null, \
             trigger = 'replay', \
             redeliver_count = redeliver_count + 1, replayed_at = now() \
         where id = $1 and endpoint_id = $2 \
           and status <> 'pending' \
           and redeliver_count < $3 \
         returning redeliver_count",
    )
    .bind(delivery_id)
    .bind(endpoint_id)
    .bind(MAX_REDELIVERIES)
    .fetch_optional(pool)
    .await?;

    if let Some(redeliver_count) = updated {
        return Ok(redeliver_count);
    }

    // Nothing was updated, so find out which of the three reasons it was — the operator's next
    // move differs completely for each ("it is not there" vs "wait a moment" vs "fix your
    // receiver instead of retrying"), and one opaque refusal would make all three look the same.
    //
    // The store error stays an error rather than becoming a fourth refusal: "the database did
    // not answer" is not one of the three answers, and folding it into `Unknown` would tell an
    // operator their delivery does not exist when in fact the platform could not look.
    let current: Option<(String, i32)> = sqlx::query_as(
        "select status, redeliver_count from webhook_deliveries where id = $1 and endpoint_id = $2",
    )
    .bind(delivery_id)
    .bind(endpoint_id)
    .fetch_optional(pool)
    .await
    .map_err(EventsError::Store)?;

    let refusal = match current {
        None => RedeliverRefusal::Unknown,
        Some((status, _)) if status == "pending" => RedeliverRefusal::AlreadyPending,
        Some(_) => RedeliverRefusal::OverCap,
    };

    // The refusal is carried *inside* the crate's error rather than in place of it, because
    // the API needs the code (`delivery_already_pending` is not `invalid_webhook_endpoint`) and
    // the panel needs the sentence — three distinct refusals behind one generic code would
    // leave all three looking the same in the error banner.
    Err(EventsError::RedeliveryRefused {
        code: refusal.code(),
        message: refusal.message(),
    })
}

/// Force several deliveries again, reporting each one separately.
///
/// Per-id outcomes rather than a batch that stops at the first refusal: an operator who
/// selected twenty rows and fixed one receiver wants to know which eleven moved, and a single
/// all-or-nothing answer makes them check the table by hand anyway. The refusals are values
/// rather than errors so one row's `pending` does not stop the other nineteen from moving.
pub async fn redeliver_many(
    pool: &PgPool,
    endpoint_id: Uuid,
    delivery_ids: &[Uuid],
) -> Result<Vec<(Uuid, Result<i32>)>> {
    let mut outcomes = Vec::with_capacity(delivery_ids.len());
    for id in delivery_ids {
        // A store error (the database not answering) *does* stop the batch, because nothing
        // after it could be reached either and a partial answer presented as a complete one is
        // worse than an error. A *refusal* does not stop it: those are per-row answers, and
        // the point of the batch is to report each one separately.
        match redeliver(pool, endpoint_id, *id).await {
            // A store error (the database not answering) *does* stop the batch, because
            // nothing after it could be reached either, and a partial answer presented as a
            // complete one is worse than an error. A *refusal* does not stop it: those are
            // per-row answers, and the point of the batch is to report each one separately.
            Ok(count) => outcomes.push((*id, Ok(count))),
            Err(refusal @ EventsError::RedeliveryRefused { .. }) => {
                outcomes.push((*id, Err(refusal)));
            }
            Err(other) => return Err(other),
        }
    }
    Ok(outcomes)
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

/// What the event feed is narrowed to.
///
/// Every field is optional and every field is a *conjunction*: a name list plus a site plus a
/// window together mean all three, which is the only reading an operator can predict without a
/// manual. `before` is a keyset cursor on the row id rather than a timestamp, because the id is
/// a monotonic sequence: two events recorded inside the same microsecond are still ordered, and
/// a page that arrives late cannot skip a row that landed behind it — exactly the failure a
/// timestamp cursor invites on a bus as fast as this one.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Organization the events belong to; `None` reads every organization's.
    pub organization_id: Option<Uuid>,
    /// Exact event names, any of which may match. An empty list matches everything.
    pub names: Vec<String>,
    /// Site the event happened on.
    pub site_id: Option<Uuid>,
    /// Account that caused it.
    pub actor_user_id: Option<Uuid>,
    /// Only events recorded at or after this instant.
    pub from: Option<OffsetDateTime>,
    /// Only events recorded at or before this instant.
    pub to: Option<OffsetDateTime>,
    /// Exclusive upper bound of the page: the id of the previous page's last row.
    pub before: Option<i64>,
    /// How many rows this read may return.
    pub limit: i64,
}

/// One page of the event feed.
#[derive(Debug)]
pub struct EventPage {
    /// The rows, newest first.
    pub events: Vec<Event>,
    /// Whether a further page exists behind the last row of this one.
    pub has_more: bool,
}

/// Recorded events, newest first, narrowed by [`EventFilter`].
///
/// One row more than asked for is selected, so the caller can say whether a further page
/// exists without a second `count` query — and without the possibility of the count and the
/// list disagreeing, which is a list that is lying.
pub async fn list_events(pool: &PgPool, filter: &EventFilter) -> Result<EventPage> {
    let sql = format!(
        "select {EVENT_COLUMNS} from events \
         where ($1::uuid is null or organization_id = $1) \
           and (cardinality($2::text[]) = 0 or name = any ($2)) \
           and ($3::uuid is null or site_id = $3) \
           and ($4::uuid is null or actor_user_id = $4) \
           and ($5::timestamptz is null or created_at >= $5) \
           and ($6::timestamptz is null or created_at <= $6) \
           and ($7::bigint is null or id < $7) \
         order by id desc \
         limit $8"
    );
    let mut rows = sqlx::query_as(&sql)
        .bind(filter.organization_id)
        .bind(&filter.names)
        .bind(filter.site_id)
        .bind(filter.actor_user_id)
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.before)
        .bind(filter.limit + 1)
        .fetch_all(pool)
        .await?;

    let has_more = rows.len() as i64 > filter.limit;
    // The extra row is the existence proof, not content: keeping it would show the operator a
    // row the "next page" button is about to show again.
    if has_more {
        rows.truncate(filter.limit.max(0) as usize);
    }

    Ok(EventPage {
        events: rows,
        has_more,
    })
}

// ---------------------------------------------------------------------------------------------
// Retention (REQ-016, slice 3)
// ---------------------------------------------------------------------------------------------

/// Read one organization's window, or the platform default when it has never set one.
///
/// `coalesce($2, $1)` rather than a plain read: the column is `not null default 30`, so a
/// `None` here can only mean the organization row does not exist yet, and answering with the
/// documented default is what lets the screen render before the first organization does.
pub async fn retention_window(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<i32> {
    let days: i32 = sqlx::query_scalar(
        "select coalesce((select event_retention_days from organizations where id = $1), $2)",
    )
    .bind(organization_id)
    .bind(DEFAULT_EVENT_RETENTION_DAYS)
    .fetch_one(pool)
    .await?;

    Ok(days)
}

/// Set one organization's window.
///
/// The write is refused by the **store** rather than only by the API, because the store is also
/// reached by a future import and by an operator's own SQL: a check constraint that holds the
/// range makes the rule true everywhere instead of true in one handler. The check constraint on
/// the column is the backstop; this read-back is what the caller returns to the screen.
pub async fn set_retention_window(
    pool: &PgPool,
    organization_id: Uuid,
    days: i32,
) -> Result<i32> {
    let stored: i32 = sqlx::query_scalar(
        "update organizations set event_retention_days = $2, updated_at = now() \
         where id = $1 returning event_retention_days",
    )
    .bind(organization_id)
    .bind(days)
    .fetch_optional(pool)
    .await?
    .ok_or(EventsError::OrganizationNotFound(organization_id))?;

    Ok(stored)
}

/// How much of one organization's history is on the bus, and how much of it is due.
///
/// Two counts in one statement, because the screen shows both and two round trips could be
/// answered by two different instants — the panel would then draw "3,412 events, 12 due" where
/// the 12 came from a moment after the 3,412, which is a pair of numbers that cannot both be
/// true.
///
/// `due` counts only events that **could** be swept, so it is the same predicate the sweep
/// itself uses: an event pinned by a pending delivery is counted in `events` and never in
/// `due`, and a screen that said "12 due" while the sweeper removes 0 would be lying.
pub async fn retention_counts(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "select count(*) as total, \
                count(*) filter (where e.created_at < now() - make_interval(days => o.window) \
                                  and not exists (select 1 from webhook_deliveries d \
                                                  where d.event_id = e.id and d.status = 'pending')) \
         as due \
         from events e \
         cross join (select coalesce((select event_retention_days from organizations where id = $1), $2) \
                     as window) o \
         where ($1::uuid is null or e.organization_id = $1)",
    )
    .bind(organization_id)
    .bind(DEFAULT_EVENT_RETENTION_DAYS)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// How much history this organization keeps, and the last sweep that ran against it.
pub async fn retention_status(pool: &PgPool, organization_id: Option<Uuid>) -> Result<RetentionStatus> {
    let window_days = retention_window(pool, organization_id).await?;
    let (events, due) = retention_counts(pool, organization_id).await?;

    let last_run = sqlx::query_as::<_, RetentionRun>(
        "select id, organization_id, started_at, finished_at, window_days, cutoff, \
                events_deleted, deliveries_deleted, error \
         from event_retention_runs \
         where organization_id is not distinct from $1 and finished_at is not null \
         order by started_at desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(RetentionStatus {
        organization_id,
        window_days,
        last_run,
        events,
        due,
    })
}

/// The last sweeps of one organization, newest first.
pub async fn list_retention_runs(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<RetentionRun>> {
    let runs = sqlx::query_as::<_, RetentionRun>(
        "select id, organization_id, started_at, finished_at, window_days, cutoff, \
                events_deleted, deliveries_deleted, error \
         from event_retention_runs \
         where organization_id is not distinct from $1 and finished_at is not null \
         order by started_at desc limit $2",
    )
    .bind(organization_id)
    .bind(limit.clamp(1, 50))
    .fetch_all(pool)
    .await?;

    Ok(runs)
}

/// Every organization that has events on the bus, with its window, oldest first.
///
/// The sweeper's work list. `oldest first` is deliberate: every instance of the API runs this
/// worker, and an ordering that is stable across instances is what stops two of them from
/// sweeping the same organization's head of the list on the same tick. Two runners racing is
/// not a correctness problem — the delete is idempotent and the counts are each truthful about
/// their own work — but it is wasted work, and a batch bound turns "wasted" into "the tail never
/// gets reached".
pub async fn organizations_with_events(pool: &PgPool, batch: i64) -> Result<Vec<(Option<Uuid>, i32)>> {
    let rows = sqlx::query_as::<_, (Option<Uuid>, i32)>(
        "select e.organization_id, \
                coalesce((select event_retention_days from organizations o where o.id = e.organization_id), $2) \
         from events e \
         group by e.organization_id \
         order by min(e.created_at) asc \
         limit $1",
    )
    .bind(batch.clamp(1, 500))
    .bind(DEFAULT_EVENT_RETENTION_DAYS)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Remove one organization's events that are older than its window, and log the run.
///
/// **The predicate is the whole design.** An event is swept only when it is past the window
/// *and* it has no `pending` delivery:
///
/// ```text
/// delete from events e
///  where e.organization_id is not distinct from $1
///    and e.created_at < $2
///    and not exists (select 1 from webhook_deliveries d
///                     where d.event_id = e.id and d.status = 'pending')
/// ```
///
/// The obvious query — "delete old events, let `on delete cascade` take the deliveries" — is
/// the bug the `not exists` exists to prevent. A `pending` row means the runner has not
/// delivered it yet: `next_attempt_at` is in the future, or a runner that claimed it died, or
/// the endpoint is disabled and the row has not been settled. Cascading that away deletes a
/// fact a receiver is still owed, and the receiver's only symptom is that the delivery never
/// arrived and nothing in the platform says why.
///
/// The delete and the run-log write share one transaction, so the log can never claim a sweep
/// that did not happen. The counters come from the statements themselves rather than from a
/// `count` before the delete: a count and a delete that disagree is a log that lies about its
/// own run.
pub async fn sweep_events(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    window_days: i32,
) -> Result<SweepReport> {
    let cutoff = now() - Duration::days(window_days as i64);
    let mut tx = pool.begin().await?;

    let run_id: Uuid = sqlx::query_scalar(
        "insert into event_retention_runs (organization_id, window_days, cutoff) \
         values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(window_days)
    .bind(cutoff)
    .fetch_one(&mut *tx)
    .await?;

    // The deliveries are counted, not selected: the cascade does the deleting, and a number
    // the log can carry is all the screen needs to say "the deliveries went with them".
    let swept = sqlx::query(
        "with due as ( \
             select e.id from events e \
             where e.organization_id is not distinct from $1 \
               and e.created_at < $2 \
               and not exists (select 1 from webhook_deliveries d \
                                where d.event_id = e.id and d.status = 'pending') \
         ) \
         delete from webhook_deliveries d using due where d.event_id = due.id",
    )
    .bind(organization_id)
    .bind(cutoff)
    .execute(&mut *tx)
    .await?;
    // `rows_affected` is a `u64` in this sqlx version and the report carries `i64`, so the
    // conversion saturates rather than wraps: a sweep that removed more than `i64::MAX` rows
    // is not reachable, and a wrapped negative count on a run log is a number nobody can
    // explain. Saturation is the honest answer and the branch is unreachable either way.
    let deliveries_deleted = i64::try_from(swept.rows_affected()).unwrap_or(i64::MAX);

    // The deliveries of *settled* rows only — the `pending` ones were excluded above, so this
    // count is what the cascade will actually remove and the two statements cannot disagree.
    let events_deleted = sqlx::query(
        "delete from events e \
         where e.organization_id is not distinct from $1 \
           and e.created_at < $2 \
           and not exists (select 1 from webhook_deliveries d \
                            where d.event_id = e.id and d.status = 'pending')",
    )
    .bind(organization_id)
    .bind(cutoff)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let events_deleted = i64::try_from(events_deleted).unwrap_or(i64::MAX);

    let finished_at: OffsetDateTime = sqlx::query_scalar(
        "update event_retention_runs set finished_at = now(), events_deleted = $2, \
                deliveries_deleted = $3 where id = $1 returning finished_at",
    )
    .bind(run_id)
    .bind(i32::try_from(events_deleted).unwrap_or(i32::MAX))
    .bind(i32::try_from(deliveries_deleted).unwrap_or(i32::MAX))
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(SweepReport {
        run_id,
        organization_id,
        window_days,
        cutoff,
        events_deleted,
        deliveries_deleted,
        finished_at,
    })
}

/// How many deliveries each event name collected since an instant.
///
/// The number is per *name* and not per id because the catalogue is about names: an operator
/// looking at `page.published` wants to know whether the name is alive on their own endpoints,
/// not how many rows one instance of it produced. The join keeps both ends honest — a delivery
/// whose event was swept by retention stops counting rather than leaving behind a number
/// nothing can explain.
pub async fn delivery_counts_since(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    since: OffsetDateTime,
) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "select e.name as name, count(*) as deliveries \
         from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where d.created_at >= $2 \
           and ($1::uuid is null or e.organization_id = $1) \
         group by e.name",
    )
    .bind(organization_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows)
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
pub async fn mark_delivered(
    pool: &PgPool,
    id: Uuid,
    response_status: Option<i32>,
    duration_ms: Option<i32>,
) -> Result<()> {
    // `duration_ms` is written on the delivered branch and on the retry branch, and *cleared*
    // by `redeliver`. It is not written by `mark_failed` because a failed attempt still has a
    // duration and throwing it away would make a receiver that fails fast and one that times
    // out look identical in the stats.
    sqlx::query(
        "update webhook_deliveries \
         set status = 'delivered', delivered_at = now(), response_status = $2, \
             duration_ms = coalesce($3, duration_ms), \
             error = null, claimed_at = null \
         where id = $1",
    )
    .bind(id)
    .bind(response_status)
    .bind(duration_ms)
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
    duration_ms: Option<i32>,
) -> Result<()> {
    // The duration is kept on a retry: a receiver that is timing out rather than refusing is
    // the case an operator needs to see, and clearing it on every retry would hide it behind
    // the very row that proves it.
    sqlx::query(
        "update webhook_deliveries \
         set next_attempt_at = $2, response_status = $3, error = $4, claimed_at = null, \
             duration_ms = coalesce($5, duration_ms) \
         where id = $1",
    )
    .bind(id)
    .bind(next_attempt_at)
    .bind(response_status)
    .bind(error)
    .bind(duration_ms)
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
    duration_ms: Option<i32>,
) -> Result<()> {
    sqlx::query(
        "update webhook_deliveries \
         set status = 'failed', response_status = $2, error = $3, claimed_at = null, \
             duration_ms = coalesce($4, duration_ms) \
         where id = $1",
    )
    .bind(id)
    .bind(response_status)
    .bind(error)
    .bind(duration_ms)
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
    fn the_percentile_never_invents_a_number() {
        // An empty sample has no percentile, and `None` is the honest answer: `0` would render
        // as "the receiver answered instantly" for an endpoint that has not run at all.
        assert_eq!(percentile_95(&[]), None);
        assert_eq!(percentile_95(&[42]), Some(42));

        // Nearest-rank over a hundred samples: the 95th value, not an interpolation between the
        // 95th and 96th, because every sample here is a measured millisecond count.
        let hundred: Vec<i32> = (1..=100).collect();
        assert_eq!(percentile_95(&hundred), Some(95));

        // A small sample must not reach past its end: `ceil(4 * 0.95) = 4`, and index 3 is the
        // last element, so the clamp is what makes a four-delivery history answerable at all.
        assert_eq!(percentile_95(&[1, 3, 5, 9]), Some(9));
    }

    #[test]
    fn the_refusals_name_three_different_problems() {
        // The whole point of the enum: three refusals whose next step differs, so they must not
        // collapse into one code on the way out.
        let codes = [
            RedeliverRefusal::Unknown.code(),
            RedeliverRefusal::AlreadyPending.code(),
            RedeliverRefusal::OverCap.code(),
        ];
        assert_eq!(codes.len(), 3);
        for (index, code) in codes.iter().enumerate() {
            assert!(
                !codes[index + 1..].contains(code),
                "refusal codes must be distinct: {code}"
            );
        }

        // And the cap is a real number, so a row that has been forced ten times says so.
        assert_eq!(MAX_REDELIVERIES, 10);
    }

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
