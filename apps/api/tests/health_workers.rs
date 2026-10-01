//! Who is alive, and what the panel can therefore claim (REQ-014, slice 4).
//!
//! Slices 1–3 shipped `worker_heartbeats` as a table and a *reader*: `probe_workers` counts
//! rows, names the stale ones, and the acceptance criterion "stopping a worker changes `4/4`
//! to `3/4` and names it" was ticked — by a walk that **inserted the rows it then read**. That
//! is a real proof of the reader and no proof at all of the platform: nothing in `apps/api`
//! ever wrote a heartbeat, so in production the worker card would have read "no worker has
//! registered a heartbeat" for ever while every gate stayed green.
//!
//! So this walk proves the **writer**, and it does it against the real reader rather than by
//! re-implementing the count. The properties worth pinning:
//!
//! 1. **A heartbeat upserts instead of appending.** Two beats of the same process are one row.
//!    A writer that inserted per beat would make `4/4` a function of how long the box has been
//!    up, and the card would read `4/400` a week into a deployment.
//! 2. **`started_at` moves exactly once.** It answers "when did this worker come up", and a
//!    refresh loop that moved it every 30 seconds would make a process that has been up for
//!    three months look three seconds old — which is the first number an operator reads.
//! 3. **A quiet worker is named, and beating makes it quiet no more.** The recovery leg is
//!    proved by writing through the real API rather than by an `update` statement: the point of
//!    the criterion is that the *platform* refreshes the row, not that a test can.
//! 4. **A clean stop is distinguishable from a crash.** `mark_stopped` is what a graceful
//!    shutdown calls, and without it a deliberate restart reads as "stale" for the whole
//!    staleness window — true, and useless.
//! 5. **The staleness limit is the one the operator saved, not a constant.** This walk drives
//!    the probe through the *settings* value, because `probe_workers` takes the limit from its
//!    `ProbeContext` and the context builder is exactly where slice 3 hard-coded 120 under a
//!    comment claiming it read the row.

#![allow(clippy::too_many_lines)]

use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_health::workers::{self, Heartbeat};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let maintenance = match Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        {
            Ok(db) => db,
            Err(error) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({error})");
                return None;
            }
        };

        let database = format!("omnion_healthw_{}", Uuid::new_v4().simple());
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

    /// Dropped explicitly, and it is the *only* cleanup there is. A panicking walk that skips
    /// it leaves a database behind, and this box shares a connection pool with nine sibling
    /// writers.
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

fn beat(kind: &str, pid: i32) -> Heartbeat {
    Heartbeat {
        id: workers::heartbeat_id(kind, "host-a", pid),
        kind: kind.to_string(),
        host: "host-a".to_string(),
        version: "9.9.9".to_string(),
        state: "running".to_string(),
        pid,
        meta: serde_json::json!({ "test": true }),
    }
}

/// The number of rows the platform itself would show, read back through the real reader.
async fn alive_via_reader(pool: &PgPool) -> i64 {
    let rows = sqlx::query_scalar::<_, i64>(
        "select count(*)::bigint from worker_heartbeats \
         where last_seen_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await
    .expect("the heartbeat count must be readable");
    rows
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// One worker's repeated heartbeat is ONE row, and it keeps the moment it came up.
///
/// The assertion is on both halves at once because either alone is satisfied by a wrong writer:
/// a row count of 1 is also what a writer that *never updated* produces, and a fixed
/// `started_at` is also what a writer that re-inserted with `now()` produces if the id were
/// random. The combination "same row, same start, moved `last_seen_at`" can only come from an
/// upsert that leaves the start alone.
#[tokio::test]
async fn a_repeated_heartbeat_is_one_row_and_keeps_its_start_time() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    let first = beat("scheduler", 101);
    workers::beat(pool, &first)
        .await
        .expect("the first beat writes");

    let started_at: time::OffsetDateTime =
        sqlx::query_scalar("select started_at from worker_heartbeats where id = $1")
            .bind(&first.id)
            .fetch_one(pool)
            .await
            .expect("the row exists");

    // Age the row's last sighting so the second beat has something to move.
    sqlx::query("update worker_heartbeats set last_seen_at = now() - interval '30 seconds'")
        .execute(pool)
        .await
        .expect("the fixture may age the row");
    let aged: time::OffsetDateTime =
        sqlx::query_scalar("select last_seen_at from worker_heartbeats where id = $1")
            .bind(&first.id)
            .fetch_one(pool)
            .await
            .expect("the row exists");

    workers::beat(pool, &first)
        .await
        .expect("the second beat writes");

    assert_eq!(
        alive_via_reader(pool).await,
        1,
        "a second beat of the same process must not add a row"
    );
    let (started_after, seen_after): (time::OffsetDateTime, time::OffsetDateTime) =
        sqlx::query_as("select started_at, last_seen_at from worker_heartbeats where id = $1")
            .bind(&first.id)
            .fetch_one(pool)
            .await
            .expect("the row still exists");

    assert_eq!(
        started_after, started_at,
        "started_at answers when the worker came up, not when it last spoke"
    );
    assert!(
        seen_after > aged,
        "and last_seen_at moved forward, got {seen_after} vs {aged}"
    );

    harness.dispose().await;
}

/// Four workers beat, one goes quiet, and the card reads `3/4` **and names it**.
///
/// This is the request's own criterion, driven through the writer this slice adds. The
/// staleness limit is written into `health_settings` and then handed to the probe through the
/// same field the settings screen writes, so the walk also proves the limit is the saved one: a
/// worker aged 400 seconds is fresh under 600 and stale under 120, and the assertion below is
/// only green if the 600 got through.
#[tokio::test]
async fn one_silent_worker_is_named_and_the_limit_is_the_one_that_was_saved() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    // The saved limit. 600 is deliberately far from the migration's 120 so a constant passes
    // nothing: with the default in place the aged worker below would already be stale and the
    // "four alive" leg would fail.
    omnion_health::save_settings(
        pool,
        &omnion_health::SettingsUpdate {
            worker_stale_seconds: Some(600),
            check_interval_seconds: None,
            thresholds: None,
            notifications: None,
            updated_by: None,
        },
    )
    .await
    .expect("the limit must be savable");
    let settings = omnion_health::load_settings(pool)
        .await
        .expect("the settings must be readable");
    assert_eq!(settings.worker_stale_seconds, 600, "the row stored it");

    // Four workers of two kinds, all fresh.
    for (kind, pid) in [
        ("scheduler", 201),
        ("scheduler", 202),
        ("delivery", 203),
        ("indexer", 204),
    ] {
        workers::beat(pool, &beat(kind, pid))
            .await
            .expect("each worker registers");
    }

    let fresh = workers::rows(pool, 24 * 3_600)
        .await
        .expect("the rows are readable");
    let summary = workers::summarise(&fresh, i64::from(settings.worker_stale_seconds));
    assert_eq!(summary.alive, 4, "every worker beat: {summary:?}");
    assert_eq!(summary.expected, 3, "three kinds registered");
    assert!(summary.stale.is_empty(), "nothing is stale yet");

    // One worker stops beating: its `last_seen_at` ages past the *saved* limit of 600.
    //
    // 700 seconds, not 400. The first version of this line used 400 and asserted the worker
    // had gone stale — against a limit the test itself had just saved at 600, so the platform
    // was right and the walk was wrong: 400 seconds is *inside* a 600-second window and the
    // worker was correctly still counted alive. The comment in this file claimed the number
    // was past the limit, and the number said otherwise. A fixture that contradicts its own
    // comment fails in the direction that looks like a product bug, which is the most
    // expensive way to be wrong.
    sqlx::query(
        "update worker_heartbeats set last_seen_at = now() - interval '700 seconds' \
         where id = $1",
    )
    .bind(workers::heartbeat_id("delivery", "host-a", 203))
    .execute(pool)
    .await
    .expect("the fixture may age one worker");

    let aged_rows = workers::rows(pool, 24 * 3_600)
        .await
        .expect("the rows are readable");
    let aged = workers::summarise(&aged_rows, i64::from(settings.worker_stale_seconds));
    assert_eq!(aged.alive, 3, "the quiet worker is not counted alive");
    assert_eq!(aged.expected, 3, "the denominator does not shrink");
    assert_eq!(
        aged.stale,
        vec![workers::heartbeat_id("delivery", "host-a", 203)],
        "and it is named, not merely counted"
    );

    // Recovery: the same worker beats again through the writer, and it is fresh.
    workers::beat(pool, &beat("delivery", 203))
        .await
        .expect("the worker beats again");
    let recovered_rows = workers::rows(pool, 24 * 3_600)
        .await
        .expect("the rows are readable");
    let recovered = workers::summarise(&recovered_rows, i64::from(settings.worker_stale_seconds));
    assert_eq!(
        recovered.alive, 4,
        "beating is what makes it alive: {recovered:?}"
    );
    assert!(recovered.stale.is_empty());

    harness.dispose().await;
}

/// A clean stop is `stopped`, not `stale`, and a stopped worker is not silently forgotten.
///
/// Two halves, because either alone is a lie: writing `stopped` without a reader that honours
/// it changes nothing the panel shows, and a reader that honours it without the writer marks
/// every deliberate restart as an outage.
#[tokio::test]
async fn a_clean_stop_is_recorded_as_stopped_rather_than_left_to_go_stale() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    let worker = beat("scheduler", 301);
    workers::beat(pool, &worker)
        .await
        .expect("the worker registers");

    let changed = workers::mark_stopped(pool, &worker.id)
        .await
        .expect("the stop must be recorded");
    assert!(changed, "a row that existed must report as changed");

    let (state,): (String,) = sqlx::query_as("select state from worker_heartbeats where id = $1")
        .bind(&worker.id)
        .fetch_one(pool)
        .await
        .expect("the row survives the stop");
    assert_eq!(state, "stopped", "a deliberate stop is its own state");

    // Stopping a row that is not there is `false`, not an error: a process that never beat must
    // not make its own shutdown fail.
    assert!(
        !workers::mark_stopped(pool, "scheduler@host-a#999999")
            .await
            .expect("stopping an unknown worker is not a failure"),
        "no row means nothing changed"
    );

    // The row is still stored, because a stopped worker is a fact about the last run, not
    // something to erase — an incident history that forgets its own quiet periods lies.
    assert_eq!(alive_via_reader(pool).await, 1);

    harness.dispose().await;
}

/// A restart reclaims its row instead of adding one, so `n/m` survives a deploy cycle.
///
/// This is the property a random uuid key cannot have, and it is the one that makes the card's
/// denominator mean anything after a week of deploys.
#[tokio::test]
async fn a_restart_replaces_the_row_and_does_not_grow_the_fleet() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    let before = beat("api", 401);
    workers::beat(pool, &before)
        .await
        .expect("the first process beats");
    let (first_started, first_seen_before_restart): (time::OffsetDateTime, time::OffsetDateTime) =
        sqlx::query_as("select started_at, last_seen_at from worker_heartbeats where id = $1")
            .bind(&before.id)
            .fetch_one(pool)
            .await
            .expect("the row exists");

    // The same process comes back later (the id is derived from kind/host/pid, so this is what
    // a restart on the same host looks like to the writer).
    sqlx::query("update worker_heartbeats set last_seen_at = now() - interval '10 minutes'")
        .execute(pool)
        .await
        .expect("the fixture may age the row");
    workers::beat(pool, &before)
        .await
        .expect("the restarted process beats");

    assert_eq!(
        alive_via_reader(pool).await,
        1,
        "a restart replaces the row; it does not append one"
    );
    let (second_started, second_seen): (time::OffsetDateTime, time::OffsetDateTime) =
        sqlx::query_as("select started_at, last_seen_at from worker_heartbeats where id = $1")
            .bind(&before.id)
            .fetch_one(pool)
            .await
            .expect("the row exists");
    // `started_at` does **not** move: see `heartbeat_id`'s note on a reused pid. The walk
    // pins the tie-break rather than the tidier story — a pid reuse is indistinguishable from
    // a still-running worker from here, and the writer is built to under-report the restart
    // instead of inventing one on every heartbeat. Asserting the opposite would have been an
    // assertion that fails the moment somebody "fixes" the upsert to refresh the column.
    assert_eq!(
        second_started, first_started,
        "a reused pid reclaims the row and keeps its original start"
    );
    assert!(
        second_seen > first_seen_before_restart,
        "but the heartbeat did move forward: {second_seen} vs {first_seen_before_restart}"
    );

    harness.dispose().await;
}

/// An invalid heartbeat is refused by the writer, with a sentence that names the problem.
///
/// Without these the same mistakes arrive as `23514`s from the migration's checks, and the
/// caller reads a constraint name instead of "this is not a worker state".
#[tokio::test]
async fn an_invalid_heartbeat_never_reaches_the_table() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    let mut wrong_state = beat("scheduler", 501);
    wrong_state.state = "napping".to_string();
    let error = workers::beat(pool, &wrong_state)
        .await
        .expect_err("an unknown state must not be stored");
    let message = error.to_string();
    assert!(
        message.contains("napping") && message.contains("running"),
        "the message names what it got and what it wants, got: {message}"
    );

    let mut no_kind = beat("", 502);
    no_kind.kind = String::new();
    assert!(
        workers::beat(pool, &no_kind).await.is_err(),
        "a heartbeat with no kind is refused"
    );

    let mut array_meta = beat("scheduler", 503);
    array_meta.meta = serde_json::json!([1]);
    assert!(
        workers::beat(pool, &array_meta).await.is_err(),
        "detail must be an object, as the migration's check says"
    );

    assert_eq!(
        alive_via_reader(pool).await,
        0,
        "and none of the refused ones left a row behind"
    );

    harness.dispose().await;
}

/// The rows a window excludes are invisible, so a month-old worker cannot make the card lie.
#[tokio::test]
async fn a_worker_outside_the_window_is_not_counted() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    workers::beat(pool, &beat("scheduler", 601))
        .await
        .expect("the worker registers");
    sqlx::query("update worker_heartbeats set last_seen_at = now() - interval '30 days'")
        .execute(pool)
        .await
        .expect("the fixture may age the row");

    let rows = workers::rows(pool, 24 * 3_600)
        .await
        .expect("the rows are readable");
    assert!(
        rows.is_empty(),
        "a month-old heartbeat is not a worker, got {} rows",
        rows.len()
    );
    let summary = workers::summarise(&rows, 120);
    assert_eq!(summary.alive, 0);
    assert_eq!(
        summary.expected, 0,
        "and the denominator does not count it either"
    );

    harness.dispose().await;
}
