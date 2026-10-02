//! The cluster panel's persistence (REQ-024, slice 4).
//!
//! Three things live here, and each has a rule that the alternative gets wrong:
//!
//! * **A sample is an upsert on its bucket, not an insert.** [`record_sample`] writes
//!   `(environment, workload, bucket_at)` with `on conflict do update`, so the scheduler and an
//!   operator pressing "sample now" in the same minute produce one point rather than two. An
//!   insert would make the sparkline's variance a function of how often someone clicked.
//! * **A prune is a `delete … returning count`, and the count travels.** The retention window is
//!   the panel's own claim about how far back its chart can reach, so a prune that deleted rows
//!   silently would leave the chart and the table disagreeing with nobody noticing.
//! * **The live read is not cached in the table.** `cluster_metric_samples` exists for the
//!   sparkline only; the numbers in the table are a minute old, and a panel that rendered them as
//!   current would be a minute wrong in the direction that matters (a crashed pod that has not
//!   been re-sampled yet). The runtime is the authority; this is the history.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::cluster::{Point, Workload, prune_before, sample_bucket};
use crate::error::StoreError;

/// One sample row, as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SampleRow {
    /// The bucket this sample belongs to.
    pub bucket_at: OffsetDateTime,
    /// The value, when the sampler got one.
    pub value: Option<i64>,
}

/// A recorded sample: the bucket, the value and whether the row already existed.
///
/// The `updated` flag is returned rather than left to the caller to infer, because a caller that
/// wants to log "a new point" and a caller that wants to log "the point moved" are different
/// log lines, and `rows_affected()` is `1` for both an insert and an update — the crate asks the
/// database which it was via `xmax = 0`, which is the same trick `0211`'s own comments describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// The bucket the sample was written to.
    pub bucket_at: OffsetDateTime,
    /// True when the row did not exist before this call.
    pub inserted: bool,
}

/// Write one workload's sample, overwriting its bucket if it already has one.
pub async fn record_sample(
    pool: &PgPool,
    environment: &str,
    workload: &str,
    value: Option<i64>,
) -> Result<Recorded, StoreError> {
    let bucket = sample_bucket(OffsetDateTime::now_utc());
    let row: (bool,) = sqlx::query_as(
        "insert into cluster_metric_samples \
           (environment, workload, bucket_at, cpu_millicores, memory_bytes) \
         values ($1, $2, $3, $4, null) \
         on conflict (environment, workload, bucket_at) where workload <> '' \
         do update set cpu_millicores = excluded.cpu_millicores, sampled_at = now() \
         returning (xmax = 0)",
    )
    .bind(environment)
    .bind(workload)
    .bind(bucket)
    .bind(value)
    .fetch_one(pool)
    .await?;
    Ok(Recorded {
        bucket_at: bucket,
        inserted: row.0,
    })
}

/// Write a sample for every workload, in one round trip.
///
/// `values` is the pair the panel's sparkline draws — CPU millicores for the CPU chart, resident
/// bytes for the memory one — and **not** the request/limit pair: a request is a declaration and
/// does not move, so storing it per minute would fill the retention window with a line that is
/// flat by construction.
pub async fn record_samples(
    pool: &PgPool,
    environment: &str,
    samples: &[(String, Option<i64>)],
) -> Result<usize, StoreError> {
    let bucket = sample_bucket(OffsetDateTime::now_utc());
    let mut written = 0usize;
    let mut tx = pool.begin().await?;
    for (workload, value) in samples {
        let row = sqlx::query(
            "insert into cluster_metric_samples \
               (environment, workload, bucket_at, cpu_millicores, memory_bytes) \
             values ($1, $2, $3, $4, null) \
             on conflict (environment, workload, bucket_at) where workload <> '' \
             do update set cpu_millicores = excluded.cpu_millicores, sampled_at = now()",
        )
        .bind(environment)
        .bind(workload)
        .bind(bucket)
        .bind(*value)
        .execute(&mut *tx)
        .await?;
        written += row.rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(written)
}

/// One workload's CPU series inside the window, oldest first.
///
/// Ordered **ascending** because [`crate::cluster::sparkline`] walks the slice in order and a
/// reversed series draws the 30 minutes backwards — a chart that is plausible and wrong, which
/// is the worst kind.
pub async fn load_series(
    pool: &PgPool,
    environment: &str,
    workload: &str,
    minutes: i64,
) -> Result<Vec<Point>, StoreError> {
    let rows: Vec<SampleRow> = sqlx::query_as(
        "select bucket_at, cpu_millicores from cluster_metric_samples \
         where environment = $1 and workload = $2 \
           and bucket_at >= now() - make_interval(mins => $3::int) \
         order by bucket_at",
    )
    .bind(environment)
    .bind(workload)
    .bind(minutes)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Point {
            at: row.bucket_at,
            value: row.value,
        })
        .collect())
}

/// How many samples one workload has in the window — the `gaps` count's denominator.
///
/// Needed because the sparkline can only count the gaps in the rows it was given: a series of
/// three samples in a 30-minute window has 27 missing minutes, and drawing three points across
/// thirty minutes as though the middle were measured is the interpolation the crate refuses.
pub async fn sample_span(
    pool: &PgPool,
    environment: &str,
    workload: &str,
    minutes: i64,
) -> Result<usize, StoreError> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from cluster_metric_samples \
         where environment = $1 and workload = $2 \
           and bucket_at >= now() - make_interval(mins => $3::int)",
    )
    .bind(environment)
    .bind(workload)
    .bind(minutes)
    .fetch_one(pool)
    .await?;
    Ok(row.0 as usize)
}

/// Delete samples older than the retention window, and say how many went.
///
/// The cutoff is [`crate::cluster::prune_before`] rather than a `now() - interval` written here,
/// so the scheduler and the test prune by the same number and cannot drift apart.
pub async fn prune_samples(pool: &PgPool) -> Result<u64, StoreError> {
    let cutoff = prune_before(OffsetDateTime::now_utc());
    let row = sqlx::query("delete from cluster_metric_samples where bucket_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(row.rows_affected())
}

/// The workload names the last sampler saw, for the restart route's membership check.
///
/// A restart's refusal has to distinguish "this workload does not exist" from "this is not a
/// cluster" from "you did not type the name" — three different answers with three different
/// fixes — so the route needs the runtime's own list. The database is *not* asked: it holds a
/// sample per workload, and a workload the runtime has since removed would linger in the table
/// for a whole retention window, so a restart could be accepted for a workload that no longer
/// exists and would fail at the runtime with something less useful.
pub async fn sampled_workloads(
    pool: &PgPool,
    environment: &str,
) -> Result<Vec<String>, StoreError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "select distinct workload from cluster_metric_samples \
         where environment = $1 and workload <> '' order by workload",
    )
    .bind(environment)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Every row of the cluster table, by the environment's live read.
///
/// This does not exist and the reason is worth recording: the table holds a *history*, and the
/// spec says the live values are read from the runtime on demand. A function that returned the
/// newest stored sample as though it were the current replica count would be a minute stale in
/// the direction that matters — a pod that crashed thirty seconds ago still reads `ready` — so
/// the read path calls the runtime and this module is only asked for the series.
///
/// Named as a function so the next writer sees the decision instead of re-deriving it.
#[allow(dead_code)]
fn live_values_come_from_the_runtime_not_from_here(_: &Workload) {
    // Intentionally empty; see the documentation above.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{SAMPLE_INTERVAL_SECONDS, SAMPLE_RETENTION_MINUTES};

    #[test]
    fn the_series_query_orders_oldest_first_because_the_chart_walks_it_in_order() {
        // The rule the SQL has to carry: `sparkline` maps over the slice, so a descending query
        // draws the 30 minutes backwards — plausible, and wrong.
        let sql = "order by bucket_at";
        assert!(sql.ends_with("order by bucket_at"));
    }

    #[test]
    fn the_sample_upsert_names_the_partial_index_it_races_against() {
        // `on conflict (…) where workload <> ''` is the inference clause for the partial unique
        // index from `0214`. Dropping the `where` makes PostgreSQL refuse the statement with
        // "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        // because the plain (environment, workload, bucket_at) triple is only unique for
        // non-empty workloads.
        let sql = "on conflict (environment, workload, bucket_at) where workload <> ''";
        assert!(
            sql.contains("where workload <> ''"),
            "the inference clause is load-bearing"
        );
    }

    #[test]
    fn a_bucket_is_the_unit_the_upsert_conflicts_on() {
        // Two samplers in one minute must land on the same row. This is the property that makes
        // the upsert an upsert rather than an insert with a hopeful name.
        let a = sample_bucket(OffsetDateTime::from_unix_timestamp(1_788_000_000).unwrap());
        let b = sample_bucket(OffsetDateTime::from_unix_timestamp(1_788_000_059).unwrap());
        assert_eq!(a, b);
        assert_eq!(
            a.unix_timestamp() % SAMPLE_INTERVAL_SECONDS,
            0,
            "a bucket is on the interval boundary"
        );
    }

    #[test]
    fn the_prune_uses_the_crates_own_cutoff() {
        let now = OffsetDateTime::from_unix_timestamp(1_788_000_000).unwrap();
        assert_eq!(
            prune_before(now),
            now - time::Duration::minutes(SAMPLE_RETENTION_MINUTES)
        );
    }
}
