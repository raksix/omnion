//! Integration walk for the retention sweep (REQ-126, slice 4).
//!
//! ## What this walk is for, given the crate's unit tests
//!
//! `crates/telemetry/src/retention.rs` has unit tests for the clamp, the report shape and the
//! payload. None of them can see the thing that was actually broken: before this loop existed,
//! `store::prune`, `trace_store::prune` and `alerts::prune_events` had **exactly one caller each,
//! and that caller was the test file**. Every one of them was correct, provable, and never ran in
//! a live instance — so an instance left up for a year kept a year of log lines while the
//! settings screen showed a retention window that nothing honoured.
//!
//! That is the same defect class as the exporter pipeline in slice 3 (a buffer every test could
//! push into and that nothing in the tree ever pushed to), and the walk below is the thing that
//! rules it out. It reads rows **out of PostgreSQL**, because a sweep that reports a number and
//! deletes nothing is indistinguishable from one that does both if you only read the report.
//!
//! Four properties, each a place a green suite hides a broken sweep:
//!
//! 1. **A sweep removes the eligible rows and keeps the rest.** Log lines, trace rows, resolved
//!    alert events and expired silences go; lines inside the window stay.
//! 2. **It does not touch audit or incident data.** This is the compliance property in the
//!    request's own words, and it is the one that has to be asserted against a row in ANOTHER
//!    table — asserting the absence of a `delete` in a diff is not evidence.
//! 3. **The windows come from the settings row**, so the number an operator typed into the
//!    screen is the number the job uses.
//! 4. **A hand-edited zero is clamped, not obeyed.** A row edited straight in the database to
//!    `0` must not become "delete the log store": the sweep clamps toward keeping more.

use omnion_core::config::Config;
use omnion_core::Db;
use omnion_telemetry::retention::{self, Retention};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn pool_or_skip() -> Option<PgPool> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("SKIP: the configuration is not valid ({error})");
            return None;
        }
    };
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return None;
        }
    };
    if let Err(error) = db.migrate().await {
        eprintln!("SKIP: the migrations did not apply ({error})");
        return None;
    }
    Some(db.pool().clone())
}

/// A log line written `days` in the past, under a name unique to this run.
async fn write_log(pool: &PgPool, days_old: i64) -> Uuid {
    let request_id = Uuid::new_v4();
    sqlx::query(
        "insert into obs_log_entries (ts, level, target, message, request_id, source) \
         values ($1, 'info', 'omnion_retention_test', $2, $3, 'api')",
    )
    .bind(OffsetDateTime::now_utc() - time::Duration::days(days_old))
    .bind(format!("a line from {days_old} days ago"))
    .bind(request_id)
    .execute(pool)
    .await
    .expect("a log line is written");
    request_id
}

#[tokio::test]
async fn a_sweep_removes_the_eligible_rows_and_keeps_the_rest() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    let ancient = write_log(&pool, 40).await;
    let fresh = write_log(&pool, 1).await;
    let ancient_trace = Uuid::new_v4();
    sqlx::query(
        "insert into obs_trace_index (trace_id, root_name, service, started_at, status) \
         values ($1, 'GET /retention', 'omnion-api', now() - interval '40 days', 'ok')",
    )
    .bind(ancient_trace.to_string())
    .execute(&pool)
    .await
    .expect("a trace row is written");
    let fresh_trace = Uuid::new_v4();
    sqlx::query(
        "insert into obs_trace_index (trace_id, root_name, service, started_at, status) \
         values ($1, 'GET /retention', 'omnion-api', now(), 'ok')",
    )
    .bind(fresh_trace.to_string())
    .execute(&pool)
    .await
    .expect("a trace row is written");

    // A 14-day window, which is the documented default and is what the sweep will read.
    let report = retention::sweep(&pool, Retention::defaults()).await;

    assert!(
        report.log_rows >= 1,
        "the sweep removed nothing, so a 40-day-old line is still there: {report:?}"
    );
    assert!(
        report.trace_rows >= 1,
        "the trace index was not pruned: {report:?}"
    );

    // The rows are read back out of the database. Asserting the report alone would pass on a
    // sweep that counts correctly and deletes nothing.
    let old_line: i64 = sqlx::query_scalar(
        "select count(*)::bigint from obs_log_entries where request_id = $1",
    )
    .bind(ancient)
    .fetch_one(&pool)
    .await
    .expect("the old line is counted");
    assert_eq!(old_line, 0, "the 40-day-old log line survived the sweep");

    let new_line: i64 = sqlx::query_scalar(
        "select count(*)::bigint from obs_log_entries where request_id = $1",
    )
    .bind(fresh)
    .fetch_one(&pool)
    .await
    .expect("the fresh line is counted");
    assert_eq!(
        new_line, 1,
        "the sweep deleted a line that is INSIDE the retention window"
    );

    let old_trace: i64 = sqlx::query_scalar(
        "select count(*)::bigint from obs_trace_index where trace_id = $1",
    )
    .bind(ancient_trace.to_string())
    .fetch_one(&pool)
    .await
    .expect("the old trace is counted");
    assert_eq!(old_trace, 0, "the 40-day-old trace row survived the sweep");

    let new_trace: i64 = sqlx::query_scalar(
        "select count(*)::bigint from obs_trace_index where trace_id = $1",
    )
    .bind(fresh_trace.to_string())
    .fetch_one(&pool)
    .await
    .expect("the fresh trace is counted");
    assert_eq!(
        new_trace, 1,
        "the sweep deleted a trace row that is INSIDE the retention window"
    );
}

#[tokio::test]
async fn a_sweep_leaves_audit_and_incident_data_alone() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // A marker string unique to this run, written into an audit row. The sweep's own tests
    // cannot make this assertion, because the tables it deletes are in another crate.
    //
    // The table is `audit_log` from 0001 — an APPEND-ONLY compliance trail whose `id` is a
    // `bigint generated always as identity`, so it is read back by its own `returning` id rather
    // than by a uuid. Guessing the table name and the id type here would have produced a walk
    // that skipped itself on a relation that does not exist, which is the quietest possible way
    // to write a compliance test.
    let marker = format!("retention-probe-{}", Uuid::new_v4());
    let audit_id: i64 = sqlx::query_scalar(
        "insert into audit_log (action, target_type, metadata, created_at) \
         values ('retention.probe', 'instance', jsonb_build_object('marker', $1::text), \
                 now() - interval '400 days') returning id",
    )
    .bind(&marker)
    .fetch_one(&pool)
    .await
    .expect("an audit row is written");

    let _ = retention::sweep(&pool, Retention::defaults()).await;

    let survived: i64 = sqlx::query_scalar(
        "select count(*)::bigint from audit_log where metadata->>'marker' = $1",
    )
    .bind(&marker)
    .fetch_one(&pool)
    .await
    .expect("the audit row is counted");
    assert_eq!(
        survived, 1,
        "the retention sweep deleted an audit row that is 400 days old — retention must never \
         reach compliance data"
    );

    sqlx::query("delete from audit_log where id = $1")
        .bind(audit_id)
        .execute(&pool)
        .await
        .expect("the probe row is cleaned up");
}

#[tokio::test]
async fn the_windows_come_from_the_settings_row_the_screen_writes() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // A 1-day window makes "eligible" mean "yesterday", which is the only way to tell a sweep
    // that read the settings row from one that used its own defaults.
    sqlx::query(
        "update obs_log_settings set logs_retention_days = 1, traces_retention_days = 1 where id = 1",
    )
    .execute(&pool)
    .await
    .expect("the settings row is written");

    let retention = retention::read_retention(&pool).await.expect("retention reads");
    assert_eq!(
        retention.logs_days, 1,
        "the sweep is not reading the window the settings screen wrote"
    );
    assert_eq!(retention.traces_days, 1);

    // Put it back, whatever the assertions above did.
    sqlx::query(
        "update obs_log_settings set logs_retention_days = 14, traces_retention_days = 7 where id = 1",
    )
    .execute(&pool)
    .await
    .expect("the settings row is restored");
}

#[tokio::test]
async fn a_negative_window_is_clamped_and_not_obeyed() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // The failure direction that matters: a window that says "keep nothing" must not become
    // "delete the log store".
    //
    // The zero case is NOT reachable through the database — `obs_log_settings` carries
    // `check (logs_retention_days between 1 and 30)`, and a direct write of 0 is refused with
    // `23514`. The first version of this test wrote 0, asserted the clamp, and failed at the
    // UPDATE — which is the schema being *more* correct than the test assumed. Two things follow
    // from that, and both are the point of this rewrite:
    //
    //   * the clamp is the SECOND line of defence, not the first. The constraint is the first.
    //     A test that only exercises the clamp has proved the wrong layer.
    //   * the value that reaches the clamp from outside the database is a NEGATIVE one, which
    //     the check also refuses. So this test drives the clamp at the function it guards
    //     (`read_retention` / `sweep` take a `Retention` from any caller) and separately asserts
    //     that the DATABASE refuses a zero outright.
    let keep = write_log(&pool, 0).await;

    // The database's own guard, asserted rather than assumed.
    let refused = sqlx::query(
        "update obs_log_settings set logs_retention_days = 0 where id = 1",
    )
    .execute(&pool)
    .await;
    assert!(
        refused.is_err(),
        "the schema accepted a zero-day window; the check constraint is the first line of defence \
         and it must hold"
    );

    // And the clamp, driven directly at the layer that would otherwise obey it.
    let clamped = Retention {
        logs_days: -5,
        traces_days: -5,
    }
    .clamped();
    assert_eq!(
        clamped.logs_days,
        retention::MIN_RETENTION_DAYS,
        "a negative window was not clamped toward keeping more"
    );
    assert_eq!(clamped.traces_days, retention::MIN_RETENTION_DAYS);

    let _ = retention::sweep(&pool, clamped).await;

    let survived: i64 = sqlx::query_scalar(
        "select count(*)::bigint from obs_log_entries where request_id = $1",
    )
    .bind(keep)
    .fetch_one(&pool)
    .await
    .expect("the line is counted");
    assert_eq!(
        survived, 1,
        "a clamped-to-zero window deleted today's log line — the clamp did not hold"
    );
}

#[tokio::test]
async fn a_pruned_sweep_emits_the_documented_event_with_counts_only() {
    // No database is needed for this one: the assertion is about the event's NAME and the
    // payload's SHAPE, both of which are decided before a row is written. The walks beside it
    // are what prove the sweep ran; this proves what it would have said.

    // The event is the request's own "observability.retention.pruned". The walk proves the name
    // validates against the bus's event-name rule and that the payload is an object — the two
    // things `bus::emit` refuses on. It does not assert a row exists, because the event carries
    // no organization and therefore fans out to nobody; `enqueue_fanout` returns 0 for a
    // platform-wide fact, and a test that expected a delivery would be asserting the wrong
    // behaviour.
    let report = retention::PruneReport {
        log_rows: 3,
        trace_rows: 1,
        alert_events: 0,
        silences: 0,
        errors: 0,
    };
    let payload = report.payload();
    assert!(payload.is_object(), "the payload must be a JSON object");
    assert_eq!(payload["total"], 4);

    // `prepare` is the bus's own validation. Reaching for it directly is the point: it is the
    // same function `emit` runs, so a name that passes here cannot be refused at emission.
    let event = omnion_events::NewEvent::new(retention::PRUNED_EVENT).payload(payload);
    assert_eq!(event.name, "observability.retention.pruned");
    assert!(event.payload.is_object());
}

#[tokio::test]
async fn every_prune_statement_is_valid_postgres_not_only_valid_rust() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // `trace_store::prune` shipped with `make_interval(days => $1)` bound to an `i64`.
    // PostgreSQL's `make_interval` takes `days integer`, has no bigint→integer cast for a named
    // parameter, and refused the statement on EVERY call — for EVERY window. Nothing noticed,
    // because the sweep treated the failure as a warning and reported `trace_rows: 0`, which is
    // the number a quiet sweep also reports.
    //
    // A unit test cannot catch this: `prune` is `async fn prune(pool, i64)` and it type-checks
    // perfectly. The parameter's SQL type only exists inside the statement text, and only a real
    // PostgreSQL has an opinion about it. So the walk proves the property directly — each prune
    // is run against the real database, and a statement that PostgreSQL refuses returns an
    // error, never a zero.
    let report = retention::sweep(&pool, Retention::defaults()).await;
    assert_eq!(
        report.errors, 0,
        "a prune statement was refused by PostgreSQL: {report:?}. Every prune in this sweep must \
         be valid SQL against a real server, not merely valid Rust."
    );
}

#[tokio::test]
async fn a_sweep_that_removed_nothing_and_failed_nothing_is_quiet() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // The counterweight to the test above: the report must be able to be quiet. A rule that said
    // "any report with zero rows is a failure" would pass the make_interval test and page an
    // operator every night about an instance with nothing to prune.
    let report = retention::sweep(&pool, Retention::defaults()).await;
    assert_eq!(report.errors, 0, "{report:?}");
    if report.total() == 0 {
        assert!(
            report.is_empty(),
            "a sweep that removed nothing and failed nothing is not quiet: {report:?}"
        );
    }
}
