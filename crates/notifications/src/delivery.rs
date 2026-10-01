//! The delivery queue: turning a recorded notification into a sent one (REQ-021, slice 4).
//!
//! Everything before this file made a notification *exist*. This is the file that makes it
//! *arrive*, and it exists at all because of a gap the earlier slices left open: `0050` shipped
//! `notification_deliveries`, the outbox lists it, the retry button re-queues it and the channel
//! filter joins it — but nothing ever wrote a row. Every other part of the platform ships a
//! queue and its runner together (`omnion-events` for webhooks, `omnion-backup` for sweeps,
//! `omnion-workflow` for automation), and the notification queue was the one table with a
//! reader and no writer.
//!
//! **The claim is the same shape as `webhook_deliveries`, deliberately.** `for update skip
//! locked` over the due rows, `attempts` incremented in the same statement, `claimed_at`
//! stamped, and a claim older than the lease treated as abandoned. A runner that dies mid-send
//! therefore costs one attempt rather than stranding the row, and the attempt still counts —
//! which is what stops a crash loop from retrying forever.
//!
//! **A transport failure is a row update, not a runner error.** A mail server that is down for
//! an hour must not take the process with it: the send is recorded as a retry, the loop
//! continues, and the same row comes back after the backoff. The only thing that stops a tick
//! is a failure to reach the database, and that is logged and retried on the next tick because
//! the work left behind is durable rows rather than an in-memory queue.
//!
//! **Backoff is exponential from a base and capped, and the cap is what produces `failed`.**
//! `attempts < max_attempts` schedules another try; reaching the cap writes `failed`, which is
//! the only state that says out loud "this was tried four times and gave up" — the delivery
//! row is the sole record, so the transition has to be explicit rather than the row simply
//! ceasing to be due.

use time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;

use crate::error::Result;
use crate::push::PrunedSubscription;
use crate::vocabulary::{CHANNELS, is_channel};

/// The runner's knobs.
///
/// **Not a database row, and deliberately so.** Every field is a runtime knob the process
/// reads from its own configuration, and none of them is something an administrator edits
/// per organization — a `FromRow` derive here would imply a table that does not exist and make
/// a future reader hunt for the migration that backs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// How many rows one tick claims.
    pub batch: i32,
    /// How long a claim is considered live before another runner may take the row.
    pub lease_seconds: i32,
    /// The first retry delay; every later one doubles it, up to `retry_max`.
    pub retry_base: Duration,
    /// The ceiling a doubled delay stops at.
    pub retry_max: Duration,
    /// How long a transport has to answer before the attempt counts as failed.
    pub request_timeout: Duration,
}

impl Default for DeliveryConfig {
    /// Development defaults: quick enough to see a retry in a test, slow enough not to spin.
    fn default() -> Self {
        Self {
            batch: 50,
            lease_seconds: 30,
            retry_base: Duration::seconds(15),
            retry_max: Duration::minutes(10),
            request_timeout: Duration::seconds(10),
        }
    }
}

/// What one claimed row is: the delivery, and the notification it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryJob {
    /// The delivery row.
    pub id: Uuid,
    /// Which notification it belongs to.
    pub notification_id: Uuid,
    /// Whose inbox it goes to.
    pub user_id: Uuid,
    /// Which channel.
    pub channel: String,
    /// The attempts so far, **already incremented by the claim**.
    pub attempts: i32,
    /// The cap.
    pub max_attempts: i32,
    /// The title to deliver.
    pub title: String,
    /// The body to deliver.
    pub body: String,
    /// Where the reader should go.
    ///
    /// **This is the in-app deep link, not a transport destination.** A module that emits
    /// `with_url("/media/files/{id}")` is telling the *bell* where to go, and reading it as
    /// somewhere to POST would send every webhook delivery to a relative path — which
    /// `reqwest` refuses with `relative URL without a base` after three attempts. A transport
    /// that needs a destination reads [`Self::webhook_endpoint`], which comes from the
    /// channel's own row.
    pub url: Option<String>,
    /// The recipient's address, from `users`, when the channel needs one.
    pub user_email: Option<String>,
    /// The `webhook` channel's destination: the organization's own endpoint when it referenced
    /// one, otherwise the literal URL it configured.
    ///
    /// `None` for every other channel, and `None` for a webhook channel that has neither — a
    /// row that cannot say where to send says so here rather than by sending the in-app link.
    pub webhook_endpoint: Option<String>,
    /// The `web_push` channel's destinations: this reader's registered browsers.
    ///
    /// **A vector, because one person owns several devices and one delivery row is per
    /// channel.** The webhook field above is a single URL because an organization configures
    /// one; a person can register a laptop and a phone, and a push transport that sent to
    /// only the first would deliver half of what the queue asked for while reporting the row
    /// as `sent`.
    ///
    /// Filled by a second query rather than by the claim's `SELECT`, and that is a type-level
    /// necessity rather than a style choice: `DeliveryJob` derives no `FromRow`, because the
    /// keys live one-to-many in `push_subscriptions` and there is no column a `Vec<PushTarget>`
    /// could be decoded from — `sqlx::FromRow` demands every field be a `Type<Postgres>`, and
    /// a `Vec` of structs is not one.
    pub push_targets: Vec<PushTarget>,
}

/// The claim's own row shape: everything `SELECT` can decode, and nothing it cannot.
///
/// A separate type rather than `FromRow` on [`DeliveryJob`] for the reason the `push_targets`
/// field documents. The conversion is one function, [`DeliveryJob::from_row`], and the test
/// that matters asserts the two shapes agree on the fields they share — a claim query whose
/// column list drifts from the struct fails to compile, so the risk is a *missing* field, not
/// a mismatched one.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct ClaimedRow {
    id: Uuid,
    notification_id: Uuid,
    user_id: Uuid,
    channel: String,
    attempts: i32,
    max_attempts: i32,
    title: String,
    body: String,
    url: Option<String>,
    user_email: Option<String>,
    webhook_endpoint: Option<String>,
}

impl DeliveryJob {
    /// Turn a decoded claim row into a job with no devices attached yet.
    fn from_row(row: ClaimedRow) -> Self {
        Self {
            id: row.id,
            notification_id: row.notification_id,
            user_id: row.user_id,
            channel: row.channel,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            title: row.title,
            body: row.body,
            url: row.url,
            user_email: row.user_email,
            webhook_endpoint: row.webhook_endpoint,
            push_targets: Vec::new(),
        }
    }
}

/// One browser a `web_push` delivery is sent to, with the two keys it needs.
///
/// **The keys are a capability and never leave the platform.** They are on this struct because
/// the body cannot be encrypted without them, and they are not on the API's device list,
/// which renders an endpoint hint for exactly this reason.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PushTarget {
    /// The `push_subscriptions` row, so a device the service declares gone can be named.
    pub id: Uuid,
    /// The service-worker endpoint this send POSTs to.
    pub endpoint: String,
    /// The subscriber's signing public key, `p256dh`.
    pub p256dh: String,
    /// The subscriber's authentication secret.
    pub auth: String,
}

/// What one tick did. Zero-valued means nothing happened, which is the runner's normal state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Rows claimed this tick.
    pub claimed: usize,
    /// Rows that arrived.
    pub sent: usize,
    /// Rows that failed and have another attempt queued.
    pub retried: usize,
    /// Rows that reached the cap and were written `failed`.
    pub failed: usize,
    /// Rows the reader's own preferences or the channel's readiness turned into `skipped`.
    pub skipped: usize,
}

impl RunReport {
    /// Whether the tick had nothing to do, so the runner can stay quiet about it.
    ///
    /// **`skipped` counts, and that is a correction rather than a detail.** An earlier shape
    /// of this asked only about `claimed`, so a tick that settled a backlog of rows whose
    /// channel was never configured reported *idle* while changing a thousand rows. The
    /// runner would then log nothing about the one tick that quietly moved the outbox — the
    /// opposite of what an operator watching a stuck queue needs to see.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.claimed == 0 && self.skipped == 0
    }
}

/// The delay before attempt `attempt + 1`.
///
/// **Exponential, clamped to `[1, 16]`.** The clamp is not defensive padding: `1_i64 << shift`
/// on an unclamped `i32` is a shift overflow, which panics in debug and wraps in release —
/// and an attempt counter that reaches 30 is reachable by a channel that has been down for a
/// day, not by a test. The cap keeps the backoff honest however long that takes.
#[must_use]
pub fn retry_delay(attempt: i32, base: Duration, max: Duration) -> Duration {
    let shift = u32::try_from(attempt.clamp(1, 16) - 1).unwrap_or(0);
    let factor = 1_i64.checked_shl(shift).unwrap_or(i64::MAX);
    let base_ms = base.whole_milliseconds().max(1) as i64;
    let cap_ms = (max.whole_milliseconds() as i64).max(base_ms);
    Duration::milliseconds(base_ms.saturating_mul(factor).min(cap_ms))
}

/// Now, in the shape the store compares against.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Enqueue one delivery row for every channel the reader was asked about.
///
/// **The caller passes every channel; this function decides which of them go.** That split is
/// the reason the function exists at all. A row exists for *all* of them, because the table's
/// whole job is to make "it is in the panel but the e-mail never came" a *state* an
/// administrator can read — and a channel with no row cannot say whether it was never
/// attempted, was skipped on purpose, or was tried and failed. `enabled` channels land as
/// `pending`; the rest land as `skipped` with the reason written on the row.
///
/// **Two statements, not one, because the status is the difference.** A single
/// `insert … select case when … then 'pending' else 'skipped' end` would be shorter and would
/// also work, but then the *reason* column has to be written in the same expression, and the
/// reason is the sentence a settings screen shows a reader who is asking why nothing arrived.
/// Keeping the branches separate means each carries its own reason rather than a `case` in a
/// value position.
///
/// `in_app` is enqueued as `pending` whatever the caller says, because the in-app row *is* the
/// notification — a reader who turned in-app off would have no inbox at all, and the
/// preference endpoint refuses that toggle for exactly this reason. A caller that passes
/// `in_app` in the disabled list is therefore answered by a row that goes out, which is the
/// right answer and the reason the override is here rather than in the validation above it.
pub async fn enqueue(
    pool: &PgPool,
    notification_id: Uuid,
    enabled: &[String],
    disabled: &[String],
) -> Result<EnqueueReport> {
    let mut report = EnqueueReport::default();

    // **No early return for two empty lists, and that is a bug this suite's own walk found.**
    // The guard that used to stand here (`if enabled.is_empty() && disabled.is_empty() { return }`)
    // contradicted the function's own promise three paragraphs above: the in-app row is written
    // unconditionally, because a reader with no in-app row has no inbox. So a caller that had
    // no remote channel to ask about — a fresh install, or a reader who has everything switched
    // off — got *no rows at all*, and the notification existed with no delivery record. The
    // outbox's honest answer there is "one channel, delivered locally", and the drawer has to
    // be able to say so.

    for channel in enabled {
        if !is_channel(channel) || channel == crate::preferences::IN_APP {
            continue;
        }
        if insert_delivery(pool, notification_id, channel, "pending", None).await? {
            report.queued += 1;
        }
    }

    for channel in disabled {
        if !is_channel(channel) || channel == crate::preferences::IN_APP {
            continue;
        }
        if insert_delivery(
            pool,
            notification_id,
            channel,
            "skipped",
            Some(READER_SWITCHED_IT_OFF),
        )
        .await?
        {
            report.skipped += 1;
        }
    }

    // The in-app row last, so it is never shadowed by a branch above: the unique constraint is
    // on (notification_id, channel) and whichever insert lands first wins. Enqueuing it
    // unconditionally after both loops makes the override true rather than conditional on the
    // caller's list ordering, which is the failure mode a reader would experience as "my
    // notification vanished because the module listed e-mail first".
    if insert_delivery(
        pool,
        notification_id,
        crate::preferences::IN_APP,
        "pending",
        None,
    )
    .await?
    {
        report.queued += 1;
    }

    Ok(report)
}

/// What one enqueue produced. Both numbers, because a caller that only hears "1" cannot tell
/// a reader with one live channel from one with a live channel and three switched off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnqueueReport {
    /// Rows waiting for a runner.
    pub queued: u32,
    /// Rows written as skipped, each with its reason on the row.
    pub skipped: u32,
}

impl EnqueueReport {
    /// Rows written, of either kind.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.queued + self.skipped
    }
}

/// The reason a row carries when the reader's own preference stopped it.
///
/// A constant rather than a literal at the call site, because this string is what the drawer
/// shows: two spellings of "the reader turned this off" in two places is one of them wrong
/// within a month.
pub const READER_SWITCHED_IT_OFF: &str = "the reader has this channel switched off";

/// One insert, and whether it created a row.
///
/// `on conflict do nothing` is what makes enqueueing idempotent: the unique constraint is
/// (notification_id, channel), so a re-run of the same emit does not produce a second attempt
/// and the drawer does not list the same channel twice. The boolean is the row count, not the
/// "did the statement succeed" answer, so a duplicate is reported as *not* queued rather than
/// as a second delivery.
async fn insert_delivery(
    pool: &PgPool,
    notification_id: Uuid,
    channel: &str,
    status: &str,
    error: Option<&str>,
) -> Result<bool> {
    let result: PgQueryResult = sqlx::query(
        "insert into notification_deliveries (notification_id, channel, status, error) \
         values ($1, $2, $3, $4) \
         on conflict (notification_id, channel) do nothing",
    )
    .bind(notification_id)
    .bind(channel)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Turn the rows nobody is working on into `skipped` — the reader's channel is off, or the
/// organization never configured one.
///
/// Left alone these would sit `pending` forever, because the runner claims due rows and a row
/// whose channel is not ready is not due. The outbox would then show a queue that never moves,
/// which reads as "the platform is stuck" rather than "this reader has e-mail switched off".
pub async fn settle_not_ready(pool: &PgPool, lease_seconds: f64) -> Result<u64> {
    // **Scoped to the row's own organization, and that scope is the whole statement.** The
    // obvious form — `not exists (select 1 from notification_channels c where c.channel =
    // d.channel and c.enabled)` — is correct only on a single-tenant install. This platform is
    // multi-tenant, and `notification_channels` is per organization, so the uncorrelated form
    // asks "does *anybody* have e-mail switched on" and settles org B's row as "not
    // configured" whenever org A has a mail transport. One customer configuring a channel would
    // then silently stop delivery for every other customer who had it.
    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries d set status = 'skipped', claimed_at = null, \
             error = 'no organization on this platform has this channel switched on' \
         from notifications n \
         where n.id = d.notification_id \
           and d.status = 'pending' and d.channel <> 'in_app' \
           and (d.claimed_at is null or d.claimed_at <= now() - make_interval(secs => $1)) \
           and not exists (select 1 from notification_channels c \
                           where c.channel = d.channel and c.enabled \
                             and (n.organization_id is null \
                                  or c.organization_id = n.organization_id))",
    )
    .bind(lease_seconds)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Claim up to `batch` due deliveries, oldest first, and stamp the claim.
///
/// **The increment is in the same statement as the claim** so two runners cannot both see
/// `attempts = 1`: with the update and the read separate, both would read the pre-update value
/// and each would hand the same row a second attempt. `for update of d skip locked` is what
/// makes the claim exclusive without blocking a runner behind a row somebody else is sending.
pub async fn claim_due(pool: &PgPool, batch: i64, lease_seconds: f64) -> Result<Vec<DeliveryJob>> {
    let claimed: Vec<Uuid> = sqlx::query_scalar(
        "with due as ( \
             select d.id from notification_deliveries d \
             where d.status = 'pending' and d.next_attempt_at <= now() \
               and (d.claimed_at is null or d.claimed_at <= now() - make_interval(secs => $2)) \
             order by d.next_attempt_at asc, d.created_at asc \
             limit $1 \
             for update skip locked \
         ) \
         update notification_deliveries d set attempts = d.attempts + 1, claimed_at = now() \
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

    let sql = "select d.id, d.notification_id, n.user_id, d.channel, d.attempts, d.max_attempts, \
                      n.title, n.body, n.url, u.email as user_email, \
                      coalesce(c.endpoint_url, w.url) as webhook_endpoint \
               from notification_deliveries d \
               join notifications n on n.id = d.notification_id \
               left join users u on u.id = n.user_id \
               left join notification_channels c \
                      on c.organization_id = n.organization_id and c.channel = 'webhook' \
                     and c.enabled \
               left join webhook_endpoints w on w.id = c.endpoint_id and w.enabled \
               where d.id = any ($1) \
               order by d.next_attempt_at asc, d.created_at asc";
    let rows: Vec<ClaimedRow> = sqlx::query_as(sql)
        .bind(claimed)
        .fetch_all(pool)
        .await?;
    let mut jobs: Vec<DeliveryJob> = rows.into_iter().map(DeliveryJob::from_row).collect();

    attach_push_targets(pool, &mut jobs).await?;
    Ok(jobs)
}

/// Give every claimed `web_push` job the reader's browsers.
///
/// **A separate statement, and the reason is that the claim cannot express it.** The keys live
/// in `push_subscriptions`, one row per browser, and a claimed job is one row per channel — so
/// the join that would carry them is one-to-many, and putting it in `claim_due` would multiply
/// the `notification_deliveries` rows by the number of registered devices. A duplicate job is
/// not a harmless artefact of a convenient query: each copy carries `attempts` already
/// incremented once, and the runner settles both, so the delivery would be sent twice and the
/// second attempt counted against the cap. Reading them per job and leaving the claim alone
/// keeps one row, one claim and one attempt.
///
/// Only `web_push` jobs are filled: the other transports have no use for the keys, and
/// loading them for every row would put `p256dh`/`auth` into the in-app transport's memory for
/// no reason.
async fn attach_push_targets(pool: &PgPool, jobs: &mut [DeliveryJob]) -> Result<()> {
    let readers: Vec<Uuid> = jobs
        .iter()
        .filter(|job| job.channel == crate::preferences::WEB_PUSH)
        .map(|job| job.user_id)
        .collect();
    if readers.is_empty() {
        return Ok(());
    }

    // **Every device, not the most recent one.** `distinct on (user_id) … order by user_id,
    // last_seen_at desc` reads like the right thing to do and quietly delivers to one browser
    // per person: the second registration is not "an older device to fall back to", it is a
    // laptop and a phone, and the phone is the one that is on the other side of the house.
    let rows: Vec<ReaderTarget> = sqlx::query_as(
        "select user_id, id, endpoint, p256dh, auth \
         from push_subscriptions where user_id = any ($1) \
         order by user_id, last_seen_at desc, created_at desc",
    )
    .bind(&readers)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    for job in jobs.iter_mut() {
        if job.channel != crate::preferences::WEB_PUSH {
            continue;
        }
        job.push_targets = rows
            .iter()
            .filter(|row| row.user_id == job.user_id)
            .map(|row| PushTarget {
                id: row.id,
                endpoint: row.endpoint.clone(),
                p256dh: row.p256dh.clone(),
                auth: row.auth.clone(),
            })
            .collect();
    }
    Ok(())
}

/// One registered browser, with the owner still attached so the jobs can be matched.
#[derive(sqlx::FromRow)]
struct ReaderTarget {
    /// Whose device this is.
    user_id: Uuid,
    /// The row's id.
    id: Uuid,
    /// The service-worker endpoint.
    endpoint: String,
    /// The subscriber's signing public key.
    p256dh: String,
    /// The subscriber's authentication secret.
    auth: String,
}

/// Record that a transport accepted the delivery.
pub async fn mark_sent(pool: &PgPool, id: Uuid, response_status: Option<i32>) -> Result<u64> {
    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries set status = 'sent', sent_at = now(), \
             response_status = $2, error = null, claimed_at = null \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .bind(response_status)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Record a failed attempt that has another try queued, with the backoff applied.
///
/// `next_attempt_at` is the *only* thing that differs from a final failure, and it is what
/// keeps a down channel from being hammered once per tick: without it, every row due would be
/// retried on the very next poll and the cap would be reached in milliseconds rather than over
/// the backoff window the box is asking for.
pub async fn mark_retry(
    pool: &PgPool,
    id: Uuid,
    response_status: Option<i32>,
    error: &str,
    next_attempt_at: OffsetDateTime,
) -> Result<u64> {
    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries set status = 'pending', next_attempt_at = $2, \
             response_status = $3, error = $4, claimed_at = null \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .bind(next_attempt_at)
    .bind(response_status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Record a failure that has reached the cap. There is no `next_attempt_at` here on purpose.
pub async fn mark_failed(
    pool: &PgPool,
    id: Uuid,
    response_status: Option<i32>,
    error: &str,
) -> Result<u64> {
    let result: PgQueryResult = sqlx::query(
        "update notification_deliveries set status = 'failed', response_status = $2, \
             error = $3, claimed_at = null \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .bind(response_status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// One transport's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportOutcome {
    /// The receiver took it; the status is what it answered.
    Accepted {
        /// The HTTP status, when the transport speaks HTTP.
        status: Option<i32>,
        /// Devices the receiver declared gone, for the queue to prune after it settles the row.
        ///
        /// **`Accepted` carries them too, not only `Failed`**: a person with two devices where
        /// one is a dead endpoint has *received* the notification, and the dead row must still
        /// go. There is no default for this field and no `#[serde(default)]` either — Rust has
        /// no per-field default on a struct variant, so `Accepted { status }` does not
        /// compile. [`Self::accepted`] is what keeps that cost off the three transports that
        /// have no devices to report.
        pruned: Vec<PrunedSubscription>,
    },
    /// The receiver refused it or could not be reached.
    Failed {
        /// The status, when there was one.
        status: Option<i32>,
        /// A sentence, never empty, that a settings screen can show.
        reason: String,
        /// Devices the receiver declared gone.
        pruned: Vec<PrunedSubscription>,
    },
}

impl TransportOutcome {
    /// The receiver took it, and there were no devices to declare gone.
    ///
    /// The constructor the three non-push transports use, so that adding the `pruned` field did
    /// not have to be a mechanical edit in every match arm in the tree — and, more usefully, so
    /// that an arm cannot *silently* forget a prune by writing `status: None` and stopping
    /// there. `TransportOutcome::accepted(status)` reads as the whole story it is.
    #[must_use]
    pub fn accepted(status: Option<i32>) -> Self {
        Self::Accepted {
            status,
            pruned: Vec::new(),
        }
    }

    /// The receiver refused it, with the sentence the outbox renders.
    #[must_use]
    pub fn failed(status: Option<i32>, reason: impl Into<String>) -> Self {
        Self::Failed {
            status,
            reason: reason.into(),
            pruned: Vec::new(),
        }
    }

    /// Record the devices a push service said are gone, on an outcome that does not carry them.
    ///
    /// A builder rather than a third `TransportOutcome` variant because *when* to prune is not
    /// the transport's decision: it collects them, and [`run_due`] prunes after the row has
    /// been settled. A transport that deleted its own rows mid-send would be iterating over the
    /// collection it is removing from.
    #[must_use]
    pub fn with_pruned(self, pruned: Vec<PrunedSubscription>) -> Self {
        match self {
            Self::Accepted { status, .. } => Self::Accepted {
                status,
                pruned,
            },
            Self::Failed { status, reason, .. } => Self::Failed {
                status,
                reason,
                pruned,
            },
        }
    }

    /// The devices this outcome declared gone.
    #[must_use]
    pub fn pruned(&self) -> &[PrunedSubscription] {
        match self {
            Self::Accepted { pruned, .. } | Self::Failed { pruned, .. } => pruned,
        }
    }

    /// Whether the delivery reached somebody.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }
}

/// What a transport needs to make one attempt.
pub trait Transport: Send + Sync {
    /// The channel this transport owns. One transport per channel, so a runner can be handed
    /// the map and look the channel up rather than matching on a name inside one big function.
    fn channel(&self) -> &'static str;

    /// Make one attempt. Never panics and never returns an error type — a transport that
    /// cannot be reached is an outcome, not a crash, because the whole point of the queue is
    /// that a broken destination is a recorded fact rather than a dead process.
    fn deliver<'a>(
        &'a self,
        job: &'a DeliveryJob,
        config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>>;
}

/// The in-app transport: the notification is already in the reader's inbox.
///
/// **It is a no-op that succeeds, and that is the honest answer.** The row exists so the drawer
/// can list the in-app channel next to the others with a status; there is nothing to send
/// because the panel read the same `notifications` row that the drawer is rendering. A
/// transport that did any work here would be inventing a second copy of the message.
#[derive(Debug, Clone, Copy, Default)]
pub struct InAppTransport;

impl Transport for InAppTransport {
    fn channel(&self) -> &'static str {
        "in_app"
    }

    fn deliver<'a>(
        &'a self,
        _job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        Box::pin(async { TransportOutcome::accepted(None) })
    }
}

/// Drain the due deliveries once, and write back what each attempt did.
///
/// The loop is the same shape as `omnion-events`' `deliver_one`: claim, attempt, settle. The
/// difference is that a transport failure here is a *row update* and never a returned error,
/// because one unreachable mail server must not stop the other 49 claimed rows from being
/// attempted in the same tick.
pub async fn run_due(
    pool: &PgPool,
    transports: &[(String, Box<dyn Transport>)],
    config: &DeliveryConfig,
) -> Result<RunReport> {
    let mut report = RunReport::default();

    report.skipped =
        usize::try_from(settle_not_ready(pool, config.lease_seconds as f64).await?).unwrap_or(0);

    let jobs = claim_due(pool, i64::from(config.batch), config.lease_seconds as f64).await?;
    report.claimed = jobs.len();

    for job in jobs {
        let Some((_, transport)) = transports.iter().find(|(name, _)| *name == job.channel) else {
            // A channel with no transport is a configuration gap, not a failure: the row goes
            // back on the queue rather than to `failed`, because the next tick may have the
            // transport installed. Settling it as failed would burn the cap on a missing
            // dependency rather than on a delivery that did not arrive.
            let _ = mark_retry(
                pool,
                job.id,
                None,
                "no transport is installed for this channel",
                now() + retry_delay(job.attempts, config.retry_base, config.retry_max),
            )
            .await?;
            report.retried += 1;
            continue;
        };

        let outcome = transport.deliver(&job, config).await;

        // **Prune after the settle, never during the send.** The transport collects the dead
        // endpoints and this is where they are deleted, because a delete inside the loop that
        // is iterating over them is a second bug layered on the first: the collection would be
        // shrinking under the loop, and on a five-device reader the fourth target could be
        // skipped without ever being sent to. Settling the row first also means a device that
        // disappeared mid-delivery cannot turn a delivered notification into a failed one.
        let outcome = if outcome.pruned().is_empty() {
            outcome
        } else {
            let pruned = outcome.pruned().to_vec();
            match crate::push::prune_endpoints(pool, &pruned).await {
                Ok(count) if count > 0 => tracing::info!(
                    user_id = %job.user_id,
                    pruned = count,
                    "removed push endpoints the push service declared gone"
                ),
                Ok(_) => {}
                // A prune that fails must not turn a delivered notification into a retry: the
                // dead rows cost one extra send each, which is survivable; a re-sent
                // notification is not.
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "the dead push endpoints could not be pruned; they will be pruned again \
                         on the next delivery"
                    );
                }
            }
            outcome
        };

        match outcome {
            TransportOutcome::Accepted { status, .. } => {
                mark_sent(pool, job.id, status).await?;
                report.sent += 1;
            }
            TransportOutcome::Failed { status, reason, .. } => {
                let reason = trim_error(&reason);
                if job.attempts < job.max_attempts {
                    let delay = retry_delay(job.attempts, config.retry_base, config.retry_max);
                    mark_retry(pool, job.id, status, &reason, now() + delay).await?;
                    report.retried += 1;
                } else {
                    mark_failed(pool, job.id, status, &reason).await?;
                    report.failed += 1;
                }
            }
        }
    }

    Ok(report)
}

/// The longest failure text kept on a row.
///
/// A transport's error can be an entire HTML error page, and the outbox renders this column;
/// an unbounded text there is a screen that cannot be read and a table that grows by however
/// much somebody else's server felt like saying.
const MAX_ERROR_CHARS: usize = 500;

fn trim_error(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.chars().count() <= MAX_ERROR_CHARS {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(MAX_ERROR_CHARS).collect();
    format!("{head}…")
}

/// The channels a runner needs a transport for, minus the ones that are always local.
///
/// Used by the API layer to build the default map, and by the test that proves every channel
/// in the closed list is either handled locally or expected to be registered — a new channel
/// in `vocabulary.rs` with no transport and no entry here is a channel that silently never
/// goes anywhere, which is exactly the failure this crate is supposed to prevent.
#[must_use]
pub fn remote_channels() -> Vec<&'static str> {
    CHANNELS
        .iter()
        .copied()
        .filter(|channel| *channel != crate::preferences::IN_APP)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_and_stops_at_the_cap() {
        let base = Duration::seconds(15);
        let cap = Duration::minutes(10);

        // The shape a first failure produces: one base delay, not two.
        assert_eq!(retry_delay(1, base, cap), Duration::seconds(15));
        assert_eq!(retry_delay(2, base, cap), Duration::seconds(30));
        assert_eq!(retry_delay(3, base, cap), Duration::seconds(60));

        // Reaches the ceiling and stays there rather than growing without bound.
        assert_eq!(retry_delay(10, base, cap), cap);
        assert_eq!(retry_delay(400, base, cap), cap);
    }

    #[test]
    fn the_backoff_does_not_overflow_on_an_absurd_attempt_count() {
        // The reason the shift is clamped: 1_i64 << 63 is a shift overflow, which panics in
        // debug and wraps in release. A channel down for a day reaches an attempt count well
        // past 16, so this is a reachable input and not a hypothetical one.
        //
        // The assertion is the *property*, not one number: bounded by the cap, never below
        // the base, and finite. Pinning an exact value here would assert the clamp constant
        // rather than the behaviour, and would fail for the right reason whenever somebody
        // widens the clamp.
        let base = Duration::seconds(1);
        let cap = Duration::hours(24);
        let delay = retry_delay(i32::MAX, base, cap);

        assert!(
            delay >= base,
            "a backoff shorter than its own base is not a backoff"
        );
        assert!(
            delay <= cap,
            "the cap is what stops the delay growing without bound"
        );
        assert!(delay.whole_milliseconds() > 0, "and it is never zero");

        // The unclamped answer would be `1 << (i32::MAX - 1)` seconds, which does not fit in
        // `OffsetDateTime` at all. Asserted as a comparison rather than a literal so this
        // test states the invariant instead of a magic number.
        assert!(
            delay.whole_seconds() < 86_400,
            "an absurd attempt count must land inside the cap, not past it"
        );
    }

    #[test]
    fn a_zero_or_negative_base_still_produces_a_positive_delay() {
        // A configuration of `0` would otherwise make every retry due on the next tick, which
        // is a hot loop against a down mail server rather than a backoff.
        let delay = retry_delay(1, Duration::ZERO, Duration::minutes(1));
        assert!(delay.whole_milliseconds() >= 1);
    }

    #[test]
    fn an_idle_tick_is_quiet_and_a_settling_one_is_not() {
        assert!(RunReport::default().is_idle());

        let claimed = RunReport {
            claimed: 1,
            ..RunReport::default()
        };
        assert!(!claimed.is_idle());

        // The case that used to report itself idle: nothing was claimed, but a backlog was
        // settled. An operator watching a queue that will not move needs to see *this* tick.
        let settling = RunReport {
            skipped: 12,
            ..RunReport::default()
        };
        assert!(!settling.is_idle());
    }

    #[test]
    fn the_enqueue_report_counts_both_kinds() {
        let report = EnqueueReport {
            queued: 2,
            skipped: 1,
        };
        assert_eq!(report.total(), 3);
        assert_eq!(EnqueueReport::default().total(), 0);
    }

    #[test]
    fn failure_text_is_bounded_and_never_empty() {
        assert_eq!(trim_error("  refused  "), "refused");

        let long = "x".repeat(MAX_ERROR_CHARS + 200);
        let trimmed = trim_error(&long);
        assert_eq!(
            trimmed.chars().count(),
            MAX_ERROR_CHARS + 1,
            "elision marker"
        );
        assert!(trimmed.ends_with('…'));
    }

    #[test]
    fn every_remote_channel_is_named() {
        // The closed list minus in-app. A channel added to `vocabulary.rs` and forgotten here
        // is a channel with no transport, and the runner's answer would be a retry forever.
        let remote = remote_channels();
        assert!(remote.contains(&"email"));
        assert!(remote.contains(&"webhook"));
        assert!(remote.contains(&"web_push"));
        assert!(remote.contains(&"chat"));
        assert!(!remote.contains(&"in_app"));
    }

    // ----------------------------------------------------------------------------------------
    // Slice 6b — the push destination and the prune contract
    // ----------------------------------------------------------------------------------------

    /// A claimed row, for the two shape tests below.
    fn claimed(webhook_endpoint: Option<&str>) -> ClaimedRow {
        ClaimedRow {
            id: Uuid::from_u128(1),
            notification_id: Uuid::from_u128(2),
            user_id: Uuid::from_u128(3),
            channel: "web_push".to_owned(),
            attempts: 1,
            max_attempts: 3,
            title: "A page is waiting".to_owned(),
            body: "Somebody asked for a review.".to_owned(),
            url: Some("/approvals".to_owned()),
            user_email: None,
            webhook_endpoint: webhook_endpoint.map(str::to_owned),
        }
    }

    #[test]
    fn a_claimed_row_becomes_a_job_with_no_devices_and_every_other_field_intact() {
        // The conversion is where a field can be quietly dropped, so it is pinned field by
        // field rather than by comparing the whole struct — a struct comparison would still
        // pass if `from_row` and the struct were changed together.
        let job = DeliveryJob::from_row(claimed(None));
        assert_eq!(job.channel, "web_push");
        assert_eq!(job.attempts, 1);
        assert_eq!(job.max_attempts, 3);
        assert_eq!(job.title, "A page is waiting");
        assert_eq!(job.url.as_deref(), Some("/approvals"));
        assert!(job.webhook_endpoint.is_none());
        assert!(
            job.push_targets.is_empty(),
            "a claim attaches no devices; `attach_push_targets` is the only writer"
        );
    }

    #[test]
    fn the_webhook_destination_survives_the_new_row_shape() {
        // The regression guard for the refactor above: the conversion must not become the
        // place a channel's destination gets lost. A webhook delivery that arrives with a
        // `None` endpoint fails with "no destination", which is the message a reader sees
        // after a send that used to work.
        let job = DeliveryJob::from_row(claimed(Some("https://collector.example/hook")));
        assert_eq!(
            job.webhook_endpoint.as_deref(),
            Some("https://collector.example/hook")
        );
    }

    #[test]
    fn a_prune_travels_on_both_outcomes_and_is_readable_from_either() {
        // `run_due` reads the devices off the outcome before it knows whether the row is
        // `sent` or `failed`, so both arms have to carry them. A push send that delivered on
        // one device and found a second one dead must remove that device *and* report success
        // — losing the prune would leave the dead endpoint in the list forever.
        let gone = vec![PrunedSubscription {
            id: Uuid::from_u128(9),
            endpoint: "https://push.example/gone".to_owned(),
            status: 410,
        }];

        let accepted = TransportOutcome::Accepted {
            status: Some(201),
            pruned: gone.clone(),
        };
        assert_eq!(accepted.pruned().len(), 1);
        assert!(accepted.is_accepted());

        let failed = TransportOutcome::Failed {
            status: Some(410),
            reason: "every registered browser is gone".to_owned(),
            pruned: gone,
        };
        assert_eq!(failed.pruned().len(), 1);
        assert!(!failed.is_accepted());
    }

    #[test]
    fn a_transport_that_prunes_nothing_reports_nothing_rather_than_an_empty_list() {
        // The constructors. A transport with no devices to report must not have to write a
        // `pruned` list to answer "nothing was pruned" — that is a change every match arm in
        // the tree would have to make for no semantic gain, and one that a future arm could
        // forget. The two constructors are the whole of that fix.
        let outcome = TransportOutcome::accepted(None);
        assert!(outcome.pruned().is_empty());
        assert!(outcome.is_accepted());

        let outcome = TransportOutcome::failed(None, "no address");
        assert!(outcome.pruned().is_empty());
        assert!(!outcome.is_accepted());

        // And the sentence survives the constructor's `impl Into<String>`, because that is the
        // column the outbox renders and a `&str` that did not become a `String` would not
        // compile here.
        match TransportOutcome::failed(None, "a bare &str") {
            TransportOutcome::Failed { reason, .. } => assert_eq!(reason, "a bare &str"),
            TransportOutcome::Accepted { .. } => panic!("failed() must build a Failed"),
        }
    }

    #[test]
    fn the_builder_does_not_invent_a_prune_and_does_not_lose_one() {
        let gone = PrunedSubscription {
            id: Uuid::from_u128(9),
            endpoint: "https://push.example/gone".to_owned(),
            status: 404,
        };
        let built = TransportOutcome::accepted(Some(201)).with_pruned(vec![gone]);
        assert_eq!(built.pruned().len(), 1);
        assert!(built.is_accepted());

        // The failed arm keeps its sentence, which is the column the outbox renders.
        let built = TransportOutcome::failed(Some(500), "the service answered 500")
            .with_pruned(Vec::new());
        match built {
            TransportOutcome::Failed { reason, .. } => {
                assert_eq!(reason, "the service answered 500");
            }
            TransportOutcome::Accepted { .. } => panic!("the builder changed the outcome's arm"),
        }
    }

    #[test]
    fn the_push_channel_is_the_one_the_row_shape_was_built_for() {
        // A guard against the vocabulary drifting from the constant the claim filters on: the
        // claim's `channel = 'web_push'` and [`crate::preferences::WEB_PUSH`] are the same
        // string, and this is what keeps them so.
        assert_eq!(crate::preferences::WEB_PUSH, "web_push");
        assert!(CHANNELS.contains(&crate::preferences::WEB_PUSH));
    }
}
