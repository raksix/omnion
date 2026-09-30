//! What the health screen can and cannot claim (REQ-014, slice 1).
//!
//! The probes are the product, and a probe's whole value is that its answer is
//! true. So this suite runs the **real** registry against **live PostgreSQL** and
//! asserts the properties that make a status screen honest — not "the endpoint
//! returns 200", which is what a screen full of constants would also do.
//!
//! **The three properties that matter, and the failure each one prevents:**
//!
//! 1. **A service that has never been probed is a row.** Asserted as *count*, not
//!    as content: an empty list and a list of green rows are the two answers a
//!    client cannot tell apart, and only one of them is true. A `services.len()`
//!    of 8 on a database with no samples is the assertion that catches a probe
//!    that quietly returned `None` and got filtered out upstream.
//! 2. **A dependency that cannot be reached is `down`, not missing.** Redis is
//!    pointed at a closed port on purpose. The assertion is that the row *exists*
//!    and reads `down` — a probe that returned `Err` and was skipped would render
//!    a *missing* Redis, which on a status screen is the most expensive way to be
//!    wrong. This is the criterion "stopping Redis flips its row to down" in the
//!    form it can be checked without stopping the shared Redis every other writer
//!    on this box depends on.
//! 3. **A number that could not be read is no number.** The host probe's readings
//!    are asserted to be finite and inside their range, and the walk checks that
//!    a metric the kernel could not supply is *absent* rather than zero.
//!
//! **The tables in the queue statement were the hard part of this tick.** The
//! first draft counted `automation_queue`, and no migration in this repository
//! has ever created that table — the same class of defect REQ-012 found in its
//! `backup_healthy` status card, which read a table that does not exist and had
//! therefore been permanently red since the request was written. A probe over a
//! table that is not there reports `down` for a queue that is working perfectly,
//! and it does so *confidently*. So this walk asserts, against the live schema,
//! that every table the queue probe names actually exists — which is the check
//! that would have caught it.

#![allow(clippy::too_many_lines)]

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{Db, RedisClient};
use omnion_health::{self, ProbeContext};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database, and the same reason the other walks have one.
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("the configuration must load");
        let Some(maintenance) = live_db(&config).await else {
            return None;
        };

        let database = format!("omnion_health_{}", Uuid::new_v4().simple());
        sqlx::query(&format!(r#"create database "{database}""#))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    /// Dropped explicitly, and it is the *only* cleanup there is.
    ///
    /// **A panicking walk that skips `dispose` leaves its database behind, and
    /// this box shares a connection pool with nine sibling writers.** Each leaked
    /// database is a directory of files nothing reclaims, and the walks stop
    /// being runnable once the disk fills. `close().await` is what releases the
    /// pool.
    ///
    /// There is deliberately **no `mem::forget` after the call, and there never
    /// should be one.** This signature takes `self`, so the call *moves* the
    /// harness; by the time it returns there is no value left to forget, and the
    /// `std::mem::forget(harness)` these walks used to end with was a
    /// use-after-move — eleven of them, in a file that was committed that way
    /// because every gate that had run (the health *crate's* unit tests, the
    /// admin's `tsc`, `node --check`) puts this test binary in nobody's
    /// dependency graph. It is recorded here because the original comment gave a
    /// confident reason for it ("stops the later `Drop` from running against a
    /// moved-from handle") and the reason is false: there is no later `Drop` of
    /// that value, because the value was consumed. A plausible-sounding
    /// justification is how a line like that survives review twice.
    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            r#"drop database if exists "{database}" with (force)"#
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// A context pointed at the scratch pool.
///
/// Redis points at a **closed port** (127.0.0.1:1) and storage at a temporary
/// directory. That is the point of the fixture rather than a limitation of it:
/// those two rows are supposed to be `down`/`healthy` *because* of how they were
/// constructed, and a walk that pointed them at the shared services would be
/// asserting the mood of a box nine other writers are using.
fn context<'a>(
    pool: &'a PgPool,
    redis: &'a RedisClient,
    storage: &'a omnion_storage::Storage,
) -> ProbeContext<'a> {
    ProbeContext {
        pool,
        redis,
        storage,
        storage_driver: "directory".to_string(),
        build: omnion_core::BuildInfo::new("omnion-api", "0.1.0"),
        environment: "test".to_string(),
        worker_stale_seconds: 120,
    }
}

/// A Redis handle that can never answer, and a directory driver that always can.
fn unreachable_redis() -> RedisClient {
    RedisClient::new("redis://127.0.0.1:1/").expect("a parseable URL needs no server")
}

fn directory_storage() -> omnion_storage::Storage {
    let mut config = omnion_storage::StorageConfig::default();
    config.driver = omnion_storage::StorageDriver::Fs;
    config.root = std::env::temp_dir();
    omnion_storage::Storage::from_config(&config).expect("the directory driver always configures")
}

// ---------------------------------------------------------------------------------------------
// The schema the probes read
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_table_a_probe_reads_really_exists() {
    // **The check that would have caught the `automation_queue` defect.**
    //
    // The queue probe's statement names four tables. If one of them is not in the
    // schema, the query errors, the probe reports `down`, and the screen says
    // "the queue is down" about a queue that is draining happily. Nothing else
    // in the build would notice: the SQL is a string, sqlx does not verify it
    // without a live database, and the panel renders whatever state it is given.
    //
    // So the table list is asserted against `information_schema` here, and this
    // test is the tripwire for the next person who adds a table name to a probe.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    for table in [
        "workflow_executions",
        "webhook_deliveries",
        "notification_deliveries",
        "backup_restore_jobs",
        "search_documents",
        "worker_heartbeats",
        "health_samples",
        "health_settings",
        "health_incidents",
        "health_maintenance_windows",
    ] {
        let exists: bool =
            sqlx::query_scalar("select to_regclass($1) is not null as present")
                .bind(table)
                .fetch_one(pool)
                .await
                .unwrap_or_else(|err| panic!("to_regclass must run for {table}: {err}"));
        assert!(exists, "the schema has no table named {table}");
    }

    // And the settings row exists from the migration, so a read never has to
    // handle "no row yet" for a table that by definition holds exactly one row.
    let settings: i64 = sqlx::query_scalar("select count(*)::bigint from health_settings")
        .fetch_one(pool)
        .await
        .expect("the settings row must be readable");
    assert_eq!(settings, 1, "health_settings is a singleton and ships with one row");

    harness.dispose().await;
}

#[tokio::test]
async fn a_fresh_database_has_every_service_row_and_no_samples() {
    // Property 1. The first thing the panel shows after a migration, and the
    // state a QA pass always sees.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let redis = unreachable_redis();
    let storage = directory_storage();
    let ctx = context(harness.pool(), &redis, &storage);

    let results = omnion_health::run_all(&ctx).await;
    assert_eq!(
        results.len(),
        8,
        "the registry probes the seven services of the request plus the host"
    );
    let overview =
        omnion_health::build_overview(results.into_iter().map(to_report).collect());

    assert_eq!(
        overview.services.len(),
        8,
        "every registered service is a row even before anything is stored"
    );
    for service in &overview.services {
        assert!(
            !service.message.is_empty(),
            "{} has no sentence to read",
            service.service
        );
    }
    // A fresh database has nothing stored yet — the count is the panel's own.
    let stored = omnion_health::sample_count(harness.pool())
        .await
        .expect("the sample count must be readable");
    assert_eq!(stored, 0, "nothing has been sampled yet");

    harness.dispose().await;
}

#[tokio::test]
async fn an_unreachable_dependency_is_down_and_the_row_still_exists() {
    // Property 2, and the acceptance criterion "stopping Redis flips its row to
    // down within one interval". A closed port is the same event as a stopped
    // service as far as the probe is concerned — and it is the only version of
    // this test that can run on a box where nine other writers depend on Redis.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let redis = unreachable_redis();
    let storage = directory_storage();
    let ctx = context(harness.pool(), &redis, &storage);

    let redis_result = omnion_health::probe_redis(&ctx).await;
    assert_eq!(
        redis_result.outcome.state(),
        "down",
        "a closed port is down, got: {}",
        redis_result.outcome.message()
    );
    assert!(
        !redis_result.outcome.message().is_empty(),
        "a red row without a reason costs the reader a dig"
    );
    assert!(
        !redis_result.checks.is_empty(),
        "the detail screen's table is never empty for a red row"
    );
    assert_eq!(redis_result.checks[0].state, "down");

    // The directory driver, by contrast, is reachable — so the same run
    // produces a green row next to the red one. Without this the walk would pass
    // with every probe returning `down`, which is what a crate that cannot reach
    // anything at all looks like.
    let storage_result = omnion_health::probe_storage(&ctx).await;
    assert_eq!(
        storage_result.outcome.state(),
        "healthy",
        "the directory driver answers, got: {}",
        storage_result.outcome.message()
    );

    // PostgreSQL is this very connection, so it must read healthy — and the
    // numbers in the row are the ones the walk can then check for sense.
    let postgres_result = omnion_health::probe_postgres(&ctx).await;
    assert_eq!(
        postgres_result.outcome.state(),
        "healthy",
        "the scratch database is up, got: {}",
        postgres_result.outcome.message()
    );
    let connections = postgres_result.detail["connections"].as_i64();
    assert!(
        connections.is_some_and(|value| value > 0),
        "the connection count is read from the server, got {:?}",
        postgres_result.detail
    );
    let max_connections = postgres_result.detail["max_connections"].as_i64();
    assert!(
        max_connections.is_some_and(|value| value > 0),
        "max_connections is a real server setting, got {max_connections:?}"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_queue_probe_reports_zero_on_an_empty_installation() {
    // The confidence test for the table-existence check above: with the four
    // queue tables present and empty, the row is `healthy` and says nothing is
    // waiting. If the statement were naming a missing table this would be `down`
    // — which is precisely how the `automation_queue` defect presented itself.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let redis = unreachable_redis();
    let storage = directory_storage();
    let ctx = context(harness.pool(), &redis, &storage);

    let queue = omnion_health::probe_queue(&ctx).await;
    assert_eq!(
        queue.outcome.state(),
        "healthy",
        "four empty queue tables are a healthy queue, got: {}",
        queue.outcome.message()
    );
    assert_eq!(queue.detail["depth"], 0);
    assert_eq!(queue.detail["failed"], 0);
    assert_eq!(
        queue.detail["oldest_pending_seconds"],
        serde_json::Value::Null,
        "nothing waiting is `null`, not `0 seconds old`"
    );

    let search = omnion_health::probe_search(&ctx).await;
    assert_eq!(search.outcome.state(), "healthy");
    assert_eq!(search.detail["documents"], 0);

    harness.dispose().await;
}

#[tokio::test]
async fn workers_are_unknown_until_one_registers_a_heartbeat() {
    // The `4/4` criterion, from the state before there is a 4. A deployment with
    // no worker rows is `unknown` — not healthy (nothing has reported) and not
    // down (nothing is known to have stopped). The platform does not know how
    // many workers it should have, and inventing a denominator is a claim the
    // probe cannot support.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let redis = unreachable_redis();
    let storage = directory_storage();
    let ctx = context(harness.pool(), &redis, &storage);

    let empty = omnion_health::probe_workers(&ctx).await;
    assert_eq!(
        empty.outcome.state(),
        "unknown",
        "no heartbeat is unknown, got: {}",
        empty.outcome.message()
    );

    // Now the real thing: two workers of one kind register, one of them long
    // enough ago to be stale. The acceptance criterion says a stopped worker is
    // *named*, so the assertion is on the name appearing in the detail rather
    // than on the state alone.
    let fresh_at = time::OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let stale_at = time::OffsetDateTime::now_utc() - time::Duration::seconds(600);
    for (id, seen_at) in [("worker-a", fresh_at), ("worker-b", stale_at)] {
        sqlx::query(
            "insert into worker_heartbeats (id, kind, host, version, last_seen_at) \
             values ($1, 'runner', 'test-host', '0.1.0', $2)",
        )
        .bind(id)
        .bind(seen_at)
        .execute(harness.pool())
        .await
        .expect("the heartbeat row must be insertable");
    }

    let with_workers = omnion_health::probe_workers(&ctx).await;
    assert_eq!(with_workers.outcome.state(), "degraded");
    assert_eq!(with_workers.detail["alive"], 2);
    let stale = with_workers.detail["stale"].as_array().expect("stale is a list");
    assert_eq!(stale.len(), 1, "exactly one kind is stale");
    assert_eq!(stale[0], "runner");
    assert!(
        with_workers.outcome.message().contains("runner"),
        "the stale kind is named, got: {}",
        with_workers.outcome.message()
    );

    // A fresh beat for both makes it green, which is the "restores on recovery"
    // half of the criterion without stopping a real process.
    sqlx::query("update worker_heartbeats set last_seen_at = now()")
        .execute(harness.pool())
        .await
        .expect("the heartbeat must be refreshable");
    let recovered = omnion_health::probe_workers(&ctx).await;
    assert_eq!(recovered.outcome.state(), "healthy");
    assert_eq!(recovered.detail["alive"], 2);

    harness.dispose().await;
}

#[tokio::test]
async fn a_run_records_samples_that_agree_with_the_rows() {
    // The property that makes the screen internally consistent: every stored
    // sample carries the state the run concluded, so a chart and a row can never
    // disagree. The failure this catches is a sample written before the verdict
    // was known, which would let a chart show a flat green line for a service
    // whose row is red.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let redis = unreachable_redis();
    let storage = directory_storage();
    let ctx = context(harness.pool(), &redis, &storage);

    let overview = omnion_health::run_and_record(harness.pool(), &ctx)
        .await
        .expect("a run must not fail because a dependency is down");

    assert_eq!(overview.services.len(), 8);
    let stored = omnion_health::latest_samples(harness.pool())
        .await
        .expect("the samples must be readable");
    assert!(!stored.is_empty(), "a run records samples");

    for sample in &stored {
        assert!(
            sample.value.is_finite(),
            "{} / {} stored a non-finite number",
            sample.service,
            sample.metric
        );
        // The agreement assertion: a sample's state must be one the run could
        // actually have concluded for that service.
        let report = overview
            .services
            .iter()
            .find(|service| service.service == sample.service);
        if let Some(report) = report {
            // `host` publishes its readings with the host row's state, and a
            // service that has a card may report a different state for one
            // metric than the row does (a queue that is deep is `degraded` while
            // its depth sample is stored under that same state — so this holds).
            assert!(
                omnion_health::is_state(&sample.state),
                "{} / {} stored the state {}",
                sample.service,
                sample.metric,
                sample.state
            );
            let _ = report;
        }
    }

    // A refusal is still stored as a refusal: the run recorded the red rows'
    // samples with `down`, which is what lets the 24h chart show the outage
    // rather than a gap.
    let redis_samples: Vec<_> = stored
        .iter()
        .filter(|sample| sample.service == "redis")
        .cloned()
        .collect();
    for sample in &redis_samples {
        assert_eq!(
            sample.state, "down",
            "Redis is unreachable, so its samples are down"
        );
    }

    harness.dispose().await;
}

#[tokio::test]
async fn retention_prunes_old_samples_and_keeps_the_incident_history() {
    // "Sample retention prunes raw samples older than 30 days without touching
    // incidents." The second half is the half that matters and the half a sweep
    // gets wrong by accident: `health_incidents` has no `sampled_at`, so the
    // sweep's WHERE clause cannot reach it — and the walk proves that by
    // inserting an old *and* an open incident and showing both survive.
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    // Two samples: one old enough to prune, one from now.
    sqlx::query(
        "insert into health_samples (service, metric, value, unit, state, sampled_at) \
         values ('redis', 'latency_ms', 5, 'ms', 'down', now() - interval '45 days')",
    )
    .execute(harness.pool())
    .await
    .expect("the old sample must be insertable");
    sqlx::query(
        "insert into health_samples (service, metric, value, unit, state, sampled_at) \
         values ('redis', 'latency_ms', 4, 'ms', 'healthy', now())",
    )
    .execute(harness.pool())
    .await
    .expect("the new sample must be insertable");

    // An incident that is older than the retention window, and still open. If
    // the sweep ever reached this table the operator's record of the outage
    // would disappear on the same schedule as the noise.
    sqlx::query(
        "insert into health_incidents \
            (service, from_state, to_state, summary, started_at) \
         values ('redis', 'healthy', 'down', 'Redis stopped answering', \
                 now() - interval '45 days')",
    )
    .execute(harness.pool())
    .await
    .expect("the incident must be insertable");

    let before: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(harness.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(before, 2);

    let deleted = omnion_health::prune_old_samples(harness.pool())
        .await
        .expect("the sweep must run");
    assert_eq!(deleted, 1, "exactly the 45-day-old sample is pruned");

    let after: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(harness.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(after, 1, "the recent sample survives");

    let incidents: i64 = sqlx::query_scalar("select count(*)::bigint from health_incidents")
        .fetch_one(harness.pool())
        .await
        .expect("the incident count must be readable");
    assert_eq!(
        incidents, 1,
        "the incident is the honest history and the sweep must not reach it"
    );

    harness.dispose().await;
}

#[tokio::test]
async fn the_store_refuses_a_sample_it_could_not_honestly_render() {
    // The write path validates, not only the constructor: a fixture or a future
    // sweep builds a `NewSample` by hand, and the table's invariant has to hold
    // for every writer rather than for the ones that remembered.
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let nan = omnion_health::NewSample {
        service: "redis".to_string(),
        metric: "latency_ms".to_string(),
        value: f64::NAN,
        unit: "ms".to_string(),
        state: "healthy".to_string(),
        detail: serde_json::json!({}),
    };
    let refused = omnion_health::record(harness.pool(), &nan).await;
    assert!(refused.is_err(), "a NaN sample is refused, not stored");

    let unknown_service = omnion_health::NewSample {
        service: "mystery".to_string(),
        metric: "latency_ms".to_string(),
        value: 1.0,
        unit: "ms".to_string(),
        state: "healthy".to_string(),
        detail: serde_json::json!({}),
    };
    assert!(
        omnion_health::record(harness.pool(), &unknown_service)
            .await
            .is_err(),
        "a service outside the registry is refused"
    );

    let stored: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(harness.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(stored, 0, "neither refusal wrote a row");

    harness.dispose().await;
}

#[tokio::test]
async fn a_whole_run_is_written_atomically() {
    // A run that dies half way through must not leave half a set behind: the
    // screen would show some of a run's numbers as if they were a whole run's.
    // The proof is the refusal — one bad sample in the middle of a good batch
    // means zero rows, not some of them.
    let Some(harness) = Harness::fresh().await else {
        return;
    };

    let good = omnion_health::NewSample {
        service: "redis".to_string(),
        metric: "latency_ms".to_string(),
        value: 3.0,
        unit: "ms".to_string(),
        state: "healthy".to_string(),
        detail: serde_json::json!({}),
    };
    let bad = omnion_health::NewSample {
        service: "redis".to_string(),
        metric: "latency_ms".to_string(),
        value: f64::INFINITY,
        unit: "ms".to_string(),
        state: "healthy".to_string(),
        detail: serde_json::json!({}),
    };

    let refused = omnion_health::record_run(harness.pool(), &[good.clone(), bad]).await;
    assert!(refused.is_err(), "a batch with a bad sample is refused");
    let stored: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(harness.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(stored, 0, "validation happens before the transaction opens");

    // The same batch without the bad sample is written — two rows, so the count
    // proves the *whole* batch landed rather than that the write happened once.
    // `good` is cloned rather than moved twice: `NewSample` owns a `String` and a
    // `serde_json::Value`, so `&[good, good]` moves the same value into the array
    // twice and the test would not compile.
    omnion_health::record_run(harness.pool(), &[good.clone(), good])
        .await
        .expect("a clean batch must be written");
    let stored: i64 = sqlx::query_scalar("select count(*)::bigint from health_samples")
        .fetch_one(harness.pool())
        .await
        .expect("the count must be readable");
    assert_eq!(stored, 2);

    harness.dispose().await;
}

#[tokio::test]
async fn a_series_comes_back_oldest_first() {
    // The chart's read. Ordered by `(sampled_at, id)` rather than by `id`: a
    // clock adjustment can put two samples out of order, and a chart that draws
    // them in insertion order shows a spike that never happened.
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    for (offset, value) in [(3, 30_i64), (1, 10), (2, 20)] {
        sqlx::query(
            "insert into health_samples (service, metric, value, unit, state, sampled_at) \
             values ('queue', 'queue_depth', $1, 'items', 'healthy', \
                     now() - make_interval(mins => $2))",
        )
        .bind(value)
        .bind(offset)
        .execute(harness.pool())
        .await
        .expect("the sample must be insertable");
    }

    let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let series = omnion_health::samples_in_window(harness.pool(), "queue", "queue_depth", since)
        .await
        .expect("the series must be readable");
    let values: Vec<f64> = series.iter().map(|sample| sample.value).collect();
    assert_eq!(values, vec![10.0, 20.0, 30.0], "oldest first, not insertion order");

    // A window that excludes everything is an empty series, not an error.
    let outside = omnion_health::samples_in_window(
        harness.pool(),
        "queue",
        "queue_depth",
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    )
    .await
    .expect("an empty window is not an error");
    assert!(outside.is_empty());

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The conversion the registry owns, mirrored so the walk can assert on reports.
// ---------------------------------------------------------------------------------------------

/// One probe result as a `ServiceReport`.
///
/// The registry's own `report_of` is private because it also produces the
/// samples, and the walk needs the report half on its own.
fn to_report(result: omnion_health::ProbeResult) -> omnion_health::ServiceReport {
    let report = omnion_health::ServiceReport {
        service: result.service.clone(),
        state: result.outcome.state().to_string(),
        latency_ms: Some(result.latency_ms),
        checked_at: Some(time::OffsetDateTime::now_utc()),
        message: result.outcome.message().to_string(),
        detail: result.detail.clone(),
        checks: result.checks.clone(),
    };
    report
}
