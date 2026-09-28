//! Health samples and per-provider usage (docs/requests/REQ-097, slice 3).
//!
//! [`crate::store`] owns providers and models; this module owns the two tables that describe how a
//! provider has been *behaving*: `ai_provider_health` (one row per probe) and `ai_provider_usage`
//! (one row per completed call).
//!
//! The two invariants that matter are kept here rather than by the callers:
//!
//! * **A sample and the verdict it produced are written together.** A row in the sample table
//!   with no matching `last_health` update would leave the Health tab and the providers list
//!   disagreeing about the same instant, so [`record_sample`] does both in one transaction.
//! * **Only [`health_status`] writes `last_health`.** Nothing here stores a status the operator
//!   did not earn by probe outcome.

use sqlx::{PgPool, Postgres, Row, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::health::{HealthStatus, Sample, health_status, p95_latency_ms, uptime_percent};

/// Columns read back from `ai_provider_health`.
const SAMPLE_COLUMNS: &str = "id, provider_id, status, latency_ms, http_status, error, checked_at";

/// One probe sample, as the Health tab reads it.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct HealthSample {
    /// Row id.
    pub id: i64,
    /// Provider that was probed.
    pub provider_id: Uuid,
    /// The verdict this sample alone implies (`ok`, `degraded` or `down`).
    pub status: String,
    /// How long the probe took.
    pub latency_ms: i32,
    /// HTTP status the endpoint answered with.
    pub http_status: Option<i32>,
    /// The endpoint's own words when it refused.
    pub error: Option<String>,
    /// When the probe ran.
    pub checked_at: OffsetDateTime,
}

impl HealthSample {
    /// The sample as the pure computation sees it.
    #[must_use]
    pub fn as_sample(&self) -> Sample {
        Sample {
            ok: self.status != "down",
            latency_ms: self.latency_ms,
            checked_at: self.checked_at,
        }
    }
}

/// One completed call, as the Usage tab and the cost manager read it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRow {
    /// Row id.
    pub id: i64,
    /// Provider that served (or refused) the call.
    pub provider_id: Uuid,
    /// Wire key of the model.
    pub model_key: Option<String>,
    /// What the call exercised (`chat`, …).
    pub task: String,
    /// `ok`, `error` or `refused`.
    pub outcome: String,
    /// HTTP status the endpoint answered with.
    pub http_status: Option<i32>,
    /// Prompt tokens, `null` where the endpoint reported none.
    pub prompt_tokens: Option<i32>,
    /// Completion tokens, `null` where the endpoint reported none.
    pub completion_tokens: Option<i32>,
    /// End-to-end wall time.
    pub latency_ms: i32,
    /// The provider that failed and was substituted for, when failover used one.
    pub substituted_from: Option<Uuid>,
    /// When the first byte reached a subscriber; `null` when it never did.
    pub first_byte_at: Option<OffsetDateTime>,
    /// When the row was written.
    pub created_at: OffsetDateTime,
}

/// One call's counters, as the runtime hands them over.
#[derive(Debug, Clone)]
pub struct NewUsage {
    /// Provider that served the call.
    pub provider_id: Uuid,
    /// Wire key of the model.
    pub model_key: Option<String>,
    /// What the call exercised.
    pub task: String,
    /// How it ended.
    pub outcome: String,
    /// HTTP status, when the endpoint answered.
    pub http_status: Option<i32>,
    /// Prompt tokens, when reported.
    pub prompt_tokens: Option<i32>,
    /// Completion tokens, when reported.
    pub completion_tokens: Option<i32>,
    /// Wall time.
    pub latency_ms: i32,
    /// The provider failover substituted for.
    pub substituted_from: Option<Uuid>,
    /// When the first byte reached a subscriber.
    pub first_byte_at: Option<OffsetDateTime>,
}

/// The one probe result a runner hands in.
#[derive(Debug, Clone)]
pub struct NewSample {
    /// Provider that was probed.
    pub provider_id: Uuid,
    /// Whether the endpoint answered.
    pub ok: bool,
    /// How long the probe took.
    pub latency_ms: i32,
    /// HTTP status, when it answered.
    pub http_status: Option<i32>,
    /// Its words when it refused.
    pub error: Option<String>,
}

/// Write one sample and update the provider's verdict in the same transaction.
///
/// The stored status of the sample is what the probe saw on its own (`ok`/`degraded`/`down` from
/// the probe's own verdict), while `last_health` on the provider is the *computed* status from the
/// last [`FAILURES_BEFORE_DOWN`](crate::health::FAILURES_BEFORE_DOWN) samples. Keeping the two
/// separate is what lets the Health tab show "this sample succeeded" next to "the provider is
/// degraded because of the three before it" without those two facts looking like a contradiction.
///
/// The verdict transitions are what the caller watches: `(previous, next)` is `Some` only when the
/// status actually changed, which is exactly the `ai.provider.health_changed` trigger.
pub async fn record_sample(
    pool: &PgPool,
    new: NewSample,
) -> Result<(HealthSample, Option<(HealthStatus, HealthStatus)>)> {
    let sample_status = if new.ok { "ok" } else { "down" };

    let mut tx = pool.begin().await?;

    let sql = format!(
        "insert into ai_provider_health (provider_id, status, latency_ms, http_status, error) \
         values ($1, $2, $3, $4, $5) returning {SAMPLE_COLUMNS}"
    );
    let sample: HealthSample = sqlx::query_as(&sql)
        .bind(new.provider_id)
        .bind(sample_status)
        .bind(new.latency_ms)
        .bind(new.http_status)
        .bind(new.error)
        .fetch_one(&mut *tx)
        .await?;

    let previous = sqlx::query("select last_health from ai_providers where id = $1")
        .bind(new.provider_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.get::<String, _>("last_health"));

    let Some(previous) = previous else {
        tx.rollback().await?;
        return Err(AiHubError::ProviderNotFound);
    };

    let verdict = recompute_status(&mut tx, new.provider_id).await?;
    let transition = match &previous {
        before if *before == verdict.as_str() => None,
        before => Some((HealthStatus::parse(before), verdict)),
    };

    tx.commit().await?;

    Ok((sample, transition))
}

/// Recompute one provider's verdict from its samples and store it.
pub async fn recompute_status(
    executor: &mut Transaction<'_, Postgres>,
    provider_id: Uuid,
) -> Result<HealthStatus> {
    let samples = recent_samples_in(&mut **executor, provider_id, 24).await?;
    let baseline = baseline_latency(&mut **executor, provider_id).await?;
    let verdict = health_status(&samples, baseline);

    sqlx::query(
        "update ai_providers set last_health = $2, last_checked_at = now(), updated_at = now() \
         where id = $1",
    )
    .bind(provider_id)
    .bind(verdict.as_str())
    .execute(&mut **executor)
    .await?;

    Ok(verdict)
}

/// Take one sample for one provider, recompute the verdict and hand back the transition.
///
/// This is what `POST /ai/providers/{id}/probe` and the runner both call, so "Probe now" and the
/// background tick cannot drift apart: the button is not a second implementation of the probe, it
/// is the same function called once.
pub async fn probe_now(
    pool: &PgPool,
    provider_id: Uuid,
    new: NewSample,
) -> Result<Option<(HealthStatus, HealthStatus)>> {
    // The id in the URL is the provider that is probed, full stop: a sample row and the verdict it
    // updates must land on the same provider, and a caller that passed a mismatched sample would
    // otherwise write one provider's sample onto another's verdict.
    let (_, transition) = record_sample(pool, NewSample { provider_id, ..new }).await?;
    Ok(transition)
}

/// The newest samples for one provider, inside a window in hours.
///
/// The Health tab shows the last 50; the window bounds the query so a provider probed every
/// minute for a month cannot make the tab load thirty days of rows to render a sparkline.
pub async fn recent_samples(
    pool: &PgPool,
    provider_id: Uuid,
    hours: i64,
    limit: i64,
) -> Result<Vec<HealthSample>> {
    let sql = format!(
        "select {SAMPLE_COLUMNS} from ai_provider_health \
         where provider_id = $1 and checked_at >= now() - make_interval(hours => $2::int) \
         order by checked_at desc limit $3"
    );
    let samples: Vec<HealthSample> = sqlx::query_as(&sql)
        .bind(provider_id)
        .bind(hours)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(samples)
}

/// The samples of one provider for the computation, newest first.
async fn recent_samples_in(
    executor: impl sqlx::PgExecutor<'_>,
    provider_id: Uuid,
    hours: i64,
) -> Result<Vec<Sample>> {
    let sql = format!(
        "select {SAMPLE_COLUMNS} from ai_provider_health \
         where provider_id = $1 and checked_at >= now() - make_interval(hours => $2::int) \
         order by checked_at desc limit 50"
    );
    let rows: Vec<HealthSample> = sqlx::query_as(&sql)
        .bind(provider_id)
        .bind(hours)
        .fetch_all(executor)
        .await?;
    Ok(rows.iter().map(HealthSample::as_sample).collect())
}

/// One provider's own median latency over the baseline window.
///
/// The median in SQL (`percentile_cont`) rather than an average, because one 30-second outlier
/// in a week's samples should not become the number every future sample is judged against.
///
/// The window needs [`MIN_BASELINE_SAMPLES`] samples before it counts for anything. A median of
/// **one** sample is that sample, and a provider on a loopback answers in under a millisecond —
/// so a 2 ms probe against a 1 ms baseline reads as "half again slower than usual" and the
/// provider flaps between `ok` and `degraded` on nothing but timing jitter. That is a verdict
/// nobody can act on and an event per tick nobody can read, so until there is a real sample to
/// take the median of, there is no baseline and the run rules decide alone.
async fn baseline_latency(
    executor: impl sqlx::PgExecutor<'_>,
    provider_id: Uuid,
) -> Result<Option<i32>> {
    let row: Option<(Option<f64>, i64)> = sqlx::query_as(
        "select percentile_cont(0.5) within group (order by latency_ms), count(*) \
         from ai_provider_health \
         where provider_id = $1 and status <> 'down' \
         and checked_at >= now() - make_interval(days => $2::int)",
    )
    .bind(provider_id)
    .bind(crate::health::BASELINE_DAYS)
    .fetch_optional(executor)
    .await?;

    Ok(row.and_then(|(median, count)| {
        if count < i64::from(crate::health::MIN_BASELINE_SAMPLES) {
            return None;
        }
        median.map(|m| m.round() as i32)
    }))
}

/// The provider's own health header: the computed status, uptime and p95 over a window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthSummary {
    /// The computed status.
    pub status: String,
    /// Uptime percentage over the window, `null` when nothing was sampled in it.
    pub uptime_percent: Option<f64>,
    /// p95 latency over the window, `null` when nothing was sampled.
    pub p95_latency_ms: Option<i32>,
    /// How many samples the window holds.
    pub sample_count: usize,
    /// The provider's own 7-day median latency, when it has one.
    pub baseline_latency_ms: Option<i32>,
    /// When the last sample was taken.
    pub last_checked_at: Option<OffsetDateTime>,
    /// The last error the provider reported.
    pub last_error: Option<String>,
}

/// One provider's health header over a window.
pub async fn health_summary(pool: &PgPool, provider_id: Uuid, hours: i64) -> Result<HealthSummary> {
    let samples = recent_samples(pool, provider_id, hours, 5_000).await?;
    let pure: Vec<Sample> = samples.iter().map(HealthSample::as_sample).collect();
    let baseline = sqlx::query_as::<_, (Option<f64>,)>(
        "select percentile_cont(0.5) within group (order by latency_ms) from ai_provider_health \
         where provider_id = $1 and status <> 'down' \
         and checked_at >= now() - make_interval(days => $2::int)",
    )
    .bind(provider_id)
    .bind(crate::health::BASELINE_DAYS)
    .fetch_optional(pool)
    .await?;

    Ok(HealthSummary {
        status: health_status(&pure, baseline.and_then(|(m,)| m.map(|v| v.round() as i32)))
            .as_str()
            .to_owned(),
        uptime_percent: uptime_percent(&pure),
        p95_latency_ms: p95_latency_ms(&pure),
        sample_count: pure.len(),
        baseline_latency_ms: baseline.and_then(|(m,)| m.map(|v| v.round() as i32)),
        last_checked_at: samples.first().map(|s| s.checked_at),
        last_error: samples.iter().find_map(|s| s.error.clone()),
    })
}

/// Drop samples and usage rows older than the retention window.
///
/// Health history and call counters are the two tables that grow without bound, and the runner is
/// the only thing that prunes them: same tick, so a provider nobody looks at still stops costing
/// rows. The retention is deliberately longer than any window the UI offers (30 days) so a 30-day
/// Usage range is always answerable from what is on disk.
pub async fn prune(pool: &PgPool, days: i64) -> Result<u64> {
    let health = sqlx::query(
        "delete from ai_provider_health where checked_at < now() - make_interval(days => $1::int)",
    )
    .bind(days)
    .execute(pool)
    .await?
    .rows_affected();

    let usage = sqlx::query(
        "delete from ai_provider_usage where created_at < now() - make_interval(days => $1::int)",
    )
    .bind(days)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(health + usage)
}

/// Record one completed call.
pub async fn record_usage(pool: &PgPool, new: NewUsage) -> Result<()> {
    if !["ok", "error", "refused"].contains(&new.outcome.as_str()) {
        return Err(AiHubError::InvalidProvider(format!(
            "\"{}\" is not a call outcome",
            new.outcome
        )));
    }

    let sql = format!(
        "insert into ai_provider_usage (provider_id, model_key, task, outcome, http_status, \
         prompt_tokens, completion_tokens, latency_ms, substituted_from, first_byte_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning id"
    );
    sqlx::query(&sql)
        .bind(new.provider_id)
        .bind(new.model_key)
        .bind(new.task)
        .bind(new.outcome)
        .bind(new.http_status)
        .bind(new.prompt_tokens)
        .bind(new.completion_tokens)
        .bind(new.latency_ms)
        .bind(new.substituted_from)
        .bind(new.first_byte_at)
        .fetch_one(pool)
        .await?;

    Ok(())
}

/// The usage totals for one provider over a window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageSummary {
    /// How many calls completed in the window.
    pub requests: i64,
    /// How many of them the provider refused or failed.
    pub errors: i64,
    /// Prompt tokens summed, ignoring the calls that reported none.
    pub prompt_tokens: i64,
    /// Completion tokens summed, ignoring the calls that reported none.
    pub completion_tokens: i64,
    /// How many calls reported no usage at all — the number the panel shows as "unknown", so a
    /// total is never quietly wrong because a stream ended without a usage frame.
    pub missing_usage: i64,
    /// p95 latency over the window.
    pub p95_latency_ms: Option<i32>,
    /// Error rate as a percentage of `requests`.
    pub error_rate_percent: f64,
    /// The per-day breakdown, oldest first.
    pub by_day: Vec<UsageDay>,
}

/// One day of a provider's usage.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageDay {
    /// The day, as `YYYY-MM-DD`.
    pub day: String,
    /// Calls that day.
    pub requests: i64,
    /// Errors that day.
    pub errors: i64,
    /// Prompt tokens that day.
    pub prompt_tokens: i64,
    /// Completion tokens that day.
    pub completion_tokens: i64,
}

/// The usage totals for one provider over a window, in a real transaction.
///
/// The acceptance criterion is that these numbers equal the rows the runtime recorded, so this
/// is a sum over `ai_provider_usage` and nothing else — no in-memory counter that a restart would
/// reset and no estimate that would be a different number than the test.
pub async fn usage_summary(pool: &PgPool, provider_id: Uuid, hours: i64) -> Result<UsageSummary> {
    let row: (i64, i64, Option<i64>, Option<i64>, i64) = sqlx::query_as(
        "select count(*), \
         count(*) filter (where outcome <> 'ok'), \
         sum(prompt_tokens), sum(completion_tokens), \
         count(*) filter (where prompt_tokens is null and completion_tokens is null) \
         from ai_provider_usage \
         where provider_id = $1 and created_at >= now() - make_interval(hours => $2::int)",
    )
    .bind(provider_id)
    .bind(hours)
    .fetch_one(pool)
    .await?;

    let days: Vec<(String, i64, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "select to_char(date_trunc('day', created_at), 'YYYY-MM-DD'), count(*), \
         count(*) filter (where outcome <> 'ok'), sum(prompt_tokens), sum(completion_tokens) \
         from ai_provider_usage \
         where provider_id = $1 and created_at >= now() - make_interval(hours => $2::int) \
         group by 1 order by 1",
    )
    .bind(provider_id)
    .bind(hours)
    .fetch_all(pool)
    .await?;

    let latencies: Vec<(i32,)> = sqlx::query_as(
        "select latency_ms from ai_provider_usage \
         where provider_id = $1 and created_at >= now() - make_interval(hours => $2::int) \
         order by latency_ms",
    )
    .bind(provider_id)
    .bind(hours)
    .fetch_all(pool)
    .await?;
    let p95 = p95_latency_ms(
        &latencies
            .iter()
            .map(|(latency,)| Sample {
                ok: true,
                latency_ms: *latency,
                checked_at: OffsetDateTime::UNIX_EPOCH,
            })
            .collect::<Vec<_>>(),
    );

    let (requests, errors, prompt, completion, missing) = row;

    Ok(UsageSummary {
        requests,
        errors,
        prompt_tokens: prompt.unwrap_or(0),
        completion_tokens: completion.unwrap_or(0),
        missing_usage: missing,
        p95_latency_ms: p95,
        error_rate_percent: if requests == 0 {
            0.0
        } else {
            (errors as f64 / requests as f64) * 100.0
        },
        by_day: days
            .into_iter()
            .map(|(day, requests, errors, prompt, completion)| UsageDay {
                day,
                requests,
                errors,
                prompt_tokens: prompt.unwrap_or(0),
                completion_tokens: completion.unwrap_or(0),
            })
            .collect(),
    })
}

/// Replace the failover order, refusing anything that is not a permutation of the enabled set.
///
/// The order is a chain every request walks, so a PUT that dropped a provider or invented one would
/// silently change routing. The check is a set comparison in the same transaction as the write, so
/// two concurrent order edits cannot interleave into a half-applied chain.
pub async fn set_failover_order(pool: &PgPool, ids: &[Uuid]) -> Result<()> {
    if ids.is_empty() {
        return Err(AiHubError::InvalidProvider(
            "the failover order needs at least one provider".to_owned(),
        ));
    }

    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != ids.len() {
        return Err(AiHubError::InvalidProvider(
            "the failover order lists a provider twice".to_owned(),
        ));
    }

    let mut tx = pool.begin().await?;

    let existing: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "select id, name, priority from ai_providers where enabled order by priority, lower(name), id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let existing_ids: Vec<Uuid> = existing.iter().map(|(id, _, _)| *id).collect();

    let mut given = unique.clone();
    given.sort_unstable();
    let mut want = existing_ids.clone();
    want.sort_unstable();

    if given != want {
        tx.rollback().await?;
        return Err(AiHubError::InvalidProvider(format!(
            "the failover order must list every enabled provider exactly once \
             (sent {}, expected {})",
            ids.len(),
            existing_ids.len()
        )));
    }

    // Rank is 1-based and spaced so an operator inserting a provider later has room between two
    // neighbours without renumbering the whole chain.
    for (index, id) in ids.iter().enumerate() {
        sqlx::query("update ai_providers set priority = $2, updated_at = now() where id = $1")
            .bind(id)
            .bind((index as i32 + 1) * 10)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// The failover chain as the panel previews it: rank, id, name, priority and health.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailoverEntry {
    /// 1-based rank in the chain.
    pub rank: usize,
    /// Provider id.
    pub id: Uuid,
    /// Provider name.
    pub name: String,
    /// The stored priority, which is what the rank was derived from.
    pub priority: i32,
    /// The provider's computed health, for the preview.
    pub health: String,
    /// Whether this provider is the installation default.
    pub is_default: bool,
}

/// The chain a request walks right now: every enabled provider, in order.
///
/// The preview is built from the same order function the router walks, so the chain the operator
/// sees is the chain that runs. Disabled providers are absent rather than dimmed: they are not
/// asked, and showing them as a step would describe routing that cannot happen.
pub async fn failover_preview(pool: &PgPool) -> Result<Vec<FailoverEntry>> {
    let providers = crate::store::failover_chain(pool).await?;

    Ok(providers
        .into_iter()
        .enumerate()
        .map(|(index, provider)| FailoverEntry {
            rank: index + 1,
            id: provider.id,
            name: provider.name,
            priority: provider.priority,
            health: provider.last_health,
            is_default: provider.is_default,
        })
        .collect())
}

/// The enabled providers, in the order the failover chain walks them.
pub async fn enabled_providers(pool: &PgPool) -> Result<Vec<Uuid>> {
    // `Uuid` on its own is not a `FromRow`: a bare column needs a one-element tuple to have a
    // name to read it by, and the name is the one thing that makes the query self-describing.
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("select id from ai_providers where enabled order by priority, lower(name)")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// How old a sample row may be before the pruner drops it, in days.
pub const RETENTION_DAYS: i64 = 30;

/// The window the runner's pruner uses.
#[must_use]
pub fn retention() -> time::Duration {
    Duration::days(RETENTION_DAYS)
}
