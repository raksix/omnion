//! The store: the SQL behind samples and their retention.
//!
//! Four rules, each one a way a status screen goes quietly wrong:
//!
//! * **The newest sample per (service, metric) is resolved in SQL, not in
//!   Rust.** The overview is a screen a person leaves open, and a read whose cost
//!   grows with how long the platform has been up is a read that eventually
//!   times out on the one tab nobody is allowed to close.
//! * **Writing a run is one statement, not a loop.** A run writes every metric it
//!   measured, and a per-metric insert means a run that dies halfway leaves the
//!   screen showing half a run's numbers as if they were a whole one.
//! * **Retention deletes by age, never by row count.** A `keep the newest N`
//!   pruner measures the table's recent *rate*, which is exactly the number that
//!   changes when the platform is busy — so the busiest day is the day the
//!   window silently shrinks.
//! * **Pruning never touches incidents.** They are the honest history, and a
//!   sweep that reached them would be the second bug of the same shape: an
//!   operator's record of the outage deleted by the retention job that was
//!   supposed to be tidying up samples.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::Result;
use crate::model::{NewSample, Sample};

/// The columns of a sample row, in the order the struct declares them.
const SAMPLE_COLUMNS: &str =
    "id, service, metric, value, unit, state, detail, sampled_at";

/// Raw samples older than this are pruned; incidents are not affected.
///
/// Thirty days, from the request's data model. The value is a constant rather
/// than a column because the migration does not put retention in a settings row
/// in slice 1 — slice 3's settings form owns the *thresholds*, and a retention
/// knob that nothing reads is a knob that lies.
pub const SAMPLE_RETENTION_DAYS: i64 = 30;

/// Every sample in the window, oldest first, for one metric.
///
/// The panel's sparkline is a series, so the ordering is the query's
/// responsibility rather than the client's: a client that sorts by id and a
/// client that sorts by time produce two different charts from one endpoint
/// whenever a clock adjustment puts two samples out of order.
pub async fn samples_in_window(
    pool: &PgPool,
    service: &str,
    metric: &str,
    since: OffsetDateTime,
) -> Result<Vec<Sample>> {
    let sql = format!(
        "select {SAMPLE_COLUMNS} from health_samples \
         where service = $1 and metric = $2 and sampled_at >= $3 \
         order by sampled_at asc, id asc"
    );
    let rows = sqlx::query_as::<_, Sample>(&sql)
        .bind(service)
        .bind(metric)
        .bind(since)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Every distinct `(service, metric)` pair that has ever been sampled, so the
/// metric table's rows come from real history rather than from a hard-coded list
/// of names nobody has measured.
pub async fn recorded_metrics(pool: &PgPool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "select distinct service, metric from health_samples order by service, metric",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The newest sample of every metric of every service, in one read.
///
/// `distinct on` rather than a window function: the panel's overview is a few
/// dozen rows and this shape lets PostgreSQL use the `(service, metric,
/// sampled_at desc)` index directly, instead of sorting the whole retained
/// history to find the newest row of each pair.
pub async fn latest_samples(pool: &PgPool) -> Result<Vec<Sample>> {
    let sql = format!(
        "select distinct on (service, metric) {SAMPLE_COLUMNS} from health_samples \
         order by service, metric, sampled_at desc, id desc"
    );
    let rows = sqlx::query_as::<_, Sample>(&sql).fetch_all(pool).await?;
    Ok(rows)
}

/// The newest sample of one metric of one service, or `None`.
pub async fn latest_sample(
    pool: &PgPool,
    service: &str,
    metric: &str,
) -> Result<Option<Sample>> {
    let sql = format!(
        "select {SAMPLE_COLUMNS} from health_samples \
         where service = $1 and metric = $2 \
         order by sampled_at desc, id desc limit 1"
    );
    let row = sqlx::query_as::<_, Sample>(&sql)
        .bind(service)
        .bind(metric)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// When anything was last sampled, whatever it was.
pub async fn last_sample_at(pool: &PgPool) -> Result<Option<OffsetDateTime>> {
    let at: Option<OffsetDateTime> =
        sqlx::query_scalar("select max(sampled_at) from health_samples").fetch_one(pool).await?;
    Ok(at)
}

/// Record one sample, after validating it.
///
/// The validation is here and not only at the constructor because a future write
/// path — a sweep, a fixture, an import — can build a [`NewSample`] by hand, and
/// the table's invariant has to hold for every writer.
pub async fn record(pool: &PgPool, sample: &NewSample) -> Result<()> {
    sample.validate()?;
    sqlx::query(
        "insert into health_samples (service, metric, value, unit, state, detail) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&sample.service)
    .bind(&sample.metric)
    .bind(sample.value)
    .bind(&sample.unit)
    .bind(&sample.state)
    .bind(&sample.detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a whole run's samples.
///
/// One transaction, and the `NewSample`s are validated *before* it is opened, so
/// a run that would be refused is refused without leaving an empty transaction
/// behind. Inside it, each row is a separate statement rather than one batched
/// `UNNEST`: the run is a handful of rows per service, and a batched insert
/// makes the failure message name a row nobody can identify.
pub async fn record_run(pool: &PgPool, samples: &[NewSample]) -> Result<()> {
    for sample in samples {
        sample.validate()?;
    }
    if samples.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for sample in samples {
        sqlx::query(
            "insert into health_samples (service, metric, value, unit, state, detail) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&sample.service)
        .bind(&sample.metric)
        .bind(sample.value)
        .bind(&sample.unit)
        .bind(&sample.state)
        .bind(&sample.detail)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Delete raw samples older than [`SAMPLE_RETENTION_DAYS`], and report how many
/// went.
///
/// Returns a count rather than `()` so the retention task can log what it
/// actually did: a pruner that reports nothing either because it deleted a
/// million rows or because it has been failing silently for a week are the same
/// line in a log.
pub async fn prune_old_samples(pool: &PgPool) -> Result<u64> {
    let deleted = sqlx::query("delete from health_samples where sampled_at < now() - ($1 || ' days')::interval")
        .bind(SAMPLE_RETENTION_DAYS.to_string())
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// How many samples the table holds, for the retention row on the settings
/// screen.
pub async fn sample_count(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(pool)
        .await?;
    Ok(count)
}
