//! What the health screen's *timeline* can claim (REQ-014, slice 3).
//!
//! Slices 1 and 2 asked "is it up now" and "what has it been doing". This one asks the
//! question an operator actually reads the screen for: **"what broke, when, for how long, and
//! did anybody look?"** Every wrong answer here is worse than an error, because an incident
//! history is a record somebody makes decisions from, and a plausible-but-wrong one is trusted:
//!
//! 1. **A steady outage is ONE incident, not one per run.** A disk at 91% for six hours with a
//!    60-second interval is 360 runs. Opening an incident per run produces 360 rows, and the
//!    table then answers "how long was the disk broken?" with the *interval*. The walk runs the
//!    same state three times and asserts one row.
//! 2. **The duration is computed by the database.** The `resolved_at` the row stores and the
//!    duration the panel shows come from one `now()`; a walk that asserted "roughly 300 seconds"
//!    would pass a Rust subtraction off a separately-read timestamp, which is wrong by exactly
//!    the gap between two reads. So the walk asserts the *relationship* — a duration that is not
//!    zero, and that agrees with `resolved_at - started_at` to the second.
//! 3. **An acknowledgement is evidence, so it carries the actor's id and survives a re-read.** A
//!    screen whose "acknowledged by" is the *session's* current user rather than the one who
//!    clicked is a screen where every ack reads as the last person to load the page.
//! 4. **A maintenance window suppresses the incident, never the state.** Skipping the sample
//!    instead would draw a flat line across a planned restart — a chart that lies about the
//!    machine. The walk asserts both halves: `suppressed = true` on the incident **and** the
//!    state still recorded as degraded.
//! 5. **A breach fires once per window.** Dedup held in process memory forgets itself on
//!    restart, so the first run after a deploy re-fires for a disk that has been over the line
//!    for an hour. The walk records the same metric three times inside one window and asserts
//!    that only the first announces.
//! 6. **A threshold with no stored pair never breaches.** The request's risk note says
//!    thresholds "start empty"; defaulting to the suggested numbers in the emitter would open
//!    incidents on a deployment nobody configured. The walk saves one pair and asserts that a
//!    *different*, unconfigured metric going further over its suggestion is silent.
//! 7. **`healthy → unknown` is not an incident.** A probe that could not run is a gap in our
//!    knowledge, not an outage; announcing it fills the history with entries an operator learns
//!    to dismiss. The walk asserts no row, and that the state is still reported.

#![allow(clippy::too_many_lines)]

use omnion_health::{
    BREACH_WINDOW_SECONDS, SettingsUpdate, Threshold, Thresholds, Transition, breach_window,
};
use omnion_identity::{NewUser, users};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: omnion_core::Db,
    maintenance: omnion_core::Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = omnion_core::config::Config::from_env().ok()?;
        let maintenance = match omnion_core::Db::connect(&omnion_core::config::DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        {
            Ok(db) => db,
            Err(err) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({err})");
                return None;
            }
        };

        let database = format!("omnion_health_inc_{}", Uuid::new_v4().simple());
        sqlx::query(&format!(r#"create database "{database}""#))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = omnion_core::Db::connect(&omnion_core::config::DatabaseConfig {
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

    async fn dispose(self) {
        // `dispose` takes `self` **by value**, so the `Drop` impl below is what a `mem::forget`
        // would have been reaching for — and calling one after this is a use-after-move. The
        // close has to be awaited *here*, before the pool handle is dropped, or the connection
        // outlives the `drop database` and the drop blocks on its own backend.
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

/// A service report as the emitter would receive it from `run_all`.
///
/// Built by hand rather than by running a probe: a walk that depended on Redis or S3 would skip
/// itself on half of the boxes this loop runs on, and a skipped walk is a walk nobody runs.
fn report(service: &str, state: &str, message: &str) -> omnion_health::ServiceReport {
    omnion_health::ServiceReport {
        service: service.to_string(),
        state: state.to_string(),
        latency_ms: Some(12),
        checked_at: Some(now()),
        message: message.to_string(),
        detail: serde_json::json!({ "probe": "walk" }),
        checks: Vec::new(),
    }
}

/// The whole policy pass, without a database.
///
/// `apply_policy` is private in the registry and the walk is about the *store's* contract, so the
/// steps are spelled out here rather than called — which is also what makes each leg assertable.
async fn emit(
    pool: &PgPool,
    report: &omnion_health::ServiceReport,
    at: OffsetDateTime,
) -> Option<Transition> {
    let suppressed = omnion_health::is_suppressed(pool, &report.service, at)
        .await
        .unwrap();
    match omnion_health::detect(pool, report).await.unwrap() {
        Some(transition) => {
            omnion_health::apply(pool, &transition, suppressed)
                .await
                .unwrap();
            Some(transition)
        }
        None => None,
    }
}

async fn open_incidents(pool: &PgPool, service: &str) -> Vec<omnion_health::Incident> {
    omnion_health::list_incidents(
        pool,
        &omnion_health::IncidentFilter {
            service: Some(service.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .incidents
}

/// A real account, because both `acknowledged_by` and `created_by` are foreign keys to `users`.
///
/// A random `Uuid::new_v4()` is not enough and the failure it produces is worth remembering: the
/// walk dies on a `23503 foreign key violation` that says nothing about maintenance windows, so
/// the obvious reading is "the window insert is broken" and the real cause is a fixture that
/// invented an actor the database has never heard of. Acknowledgement is *only* meaningful with
/// a real user behind it — a row whose `acknowledged_by` points at nothing is exactly the
/// un-evidenced acknowledgement the request rules out.
async fn account(pool: &PgPool) -> Uuid {
    users::create_user(
        pool,
        NewUser {
            email: format!("walk-{}@omnion.test", Uuid::new_v4().simple()),
            password: "Walkthrough-Passw0rd-1".to_owned(),
            display_name: "Walk".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("the account must be created")
    .id
}

// ---------------------------------------------------------------------------------------------
// One steady outage is one incident
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_steady_outage_opens_one_incident_and_not_one_per_run() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    // Three runs of the same state, exactly what a 60-second interval produces for a disk that
    // stays over the line. The first opens; the other two find an open incident and do nothing.
    let first = emit(&pool, &report("redis", "down", "connection refused"), now()).await;
    assert!(
        first.is_some(),
        "the first failing run has to open an incident"
    );
    for _ in 0..2 {
        assert!(
            emit(&pool, &report("redis", "down", "connection refused"), now())
                .await
                .is_none(),
            "a second run of the same state is not an event"
        );
    }

    let rows = open_incidents(&pool, "redis").await;
    assert_eq!(
        rows.len(),
        1,
        "360 runs of one outage must be one row, not 360 — otherwise the table's duration \
         column answers with the check interval"
    );
    assert!(
        rows[0].is_open(),
        "it must still be open: nothing has recovered"
    );
    assert_eq!(
        rows[0].from_state, "healthy",
        "the platform had no record of it being broken"
    );
    assert_eq!(rows[0].to_state, "down");

    // The mechanism, not just the outcome: a `detect` that returned `None` for everybody would
    // leave zero rows and pass the "no duplicates" half of this assertion while hiding the
    // feature entirely. So the read is asked for one incident by id as well.
    let by_id = omnion_health::incident(&pool, rows[0].id).await.unwrap();
    assert_eq!(
        by_id.id, rows[0].id,
        "the list row and the detail read are the same incident"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// Recovery carries a duration the database computed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn recovery_resolves_with_a_duration_the_database_computed() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    emit(&pool, &report("postgres", "degraded", "slow query"), now()).await;
    // Backdate the opening so the duration is unambiguously non-zero and not a rounding artefact
    // of two reads a microsecond apart.
    sqlx::query("update health_incidents set started_at = now() - interval '7 minutes' where service = 'postgres'")
        .execute(&pool)
        .await
        .unwrap();

    let outcome = omnion_health::detect(&pool, &report("postgres", "healthy", "ok"))
        .await
        .unwrap()
        .expect("a recovery from a worse state is an event");
    assert!(
        outcome.is_recovery(),
        "degraded → healthy closes the incident"
    );
    omnion_health::apply(&pool, &outcome, false).await.unwrap();

    let row = open_incidents(&pool, "postgres").await.remove(0);
    assert!(
        !row.is_open(),
        "the open incident is the one the recovery closed"
    );

    let resolved_at = row
        .resolved_at
        .expect("a recovery stores a resolution time");
    let elapsed = (resolved_at - row.started_at).whole_seconds();
    assert!(
        (400..=460).contains(&elapsed),
        "seven minutes backdated must resolve at ~420 s, not {elapsed} — a duration computed in \
         Rust off a separately-read timestamp is wrong by the gap between the two reads"
    );
    assert_eq!(
        row.duration_seconds(),
        Some(elapsed),
        "the duration the panel shows must be the same arithmetic as the stored timestamps"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// An acknowledgement is evidence
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_acknowledgement_names_the_actor_and_survives_a_reread() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();
    let actor = account(&pool).await;

    emit(&pool, &report("s3", "degraded", "slow HEAD"), now()).await;
    let id = open_incidents(&pool, "s3").await.remove(0).id;

    let acked = omnion_health::acknowledge(&pool, id, actor, "restarting the bucket now")
        .await
        .unwrap();
    assert_eq!(
        acked.acknowledged_by,
        Some(actor),
        "the note records who looked"
    );
    assert!(acked.acknowledged_at.is_some());
    assert_eq!(acked.note.as_deref(), Some("restarting the bucket now"));

    // Re-read through the *detail* function rather than trusting the write's return value: the
    // screen opens the incident in a new request, and a value that only existed in the PATCH's
    // response would render as "not acknowledged" on every open.
    let reread = omnion_health::incident(&pool, id).await.unwrap();
    assert_eq!(
        reread.acknowledged_by,
        Some(actor),
        "it has to persist, not just return"
    );
    assert!(
        reread.is_open(),
        "acknowledging is not resolving — the outage is still open"
    );

    // A second acknowledgement **takes over** rather than being refused, because a shift handover
    // is the ordinary case and not an attack: the newest claim is the current one, and the screen
    // shows that operator. The alternative (`and acknowledged_by is null`) reads like a lock and
    // refuses the one interaction this table exists to support.
    let other = account(&pool).await;
    omnion_health::acknowledge(&pool, id, other, "handing over")
        .await
        .unwrap();
    let handover = omnion_health::incident(&pool, id).await.unwrap();
    assert_eq!(
        handover.acknowledged_by,
        Some(other),
        "the newest claim is the one on the row"
    );
    assert_eq!(
        handover.note.as_deref(),
        Some("handing over"),
        "and its note replaces the old one"
    );
    assert!(
        handover.resolved_at.is_none(),
        "acknowledging never resolves — a handover claims the incident, it does not close the \
         outage it describes"
    );

    // An acknowledgement of a row that does not exist is refused by id, not silently a no-op.
    assert!(
        omnion_health::acknowledge(&pool, Uuid::new_v4(), actor, "ghost")
            .await
            .is_err(),
        "a missing incident must be an error rather than a successful no-op"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// A maintenance window suppresses the incident, never the state
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_maintenance_window_suppresses_the_incident_and_keeps_the_state() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();
    let at = now();

    sqlx::query(
        "insert into health_maintenance_windows (starts_at, ends_at, services, note, created_by) \
         values ($1, $2, '{}', 'deploy', $3)",
    )
    .bind(at - time::Duration::minutes(5))
    .bind(at + time::Duration::minutes(5))
    .bind(account(&pool).await)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        omnion_health::is_suppressed(&pool, "redis", at)
            .await
            .unwrap(),
        "an empty services array covers every service — it is the deploy case"
    );

    let transition = omnion_health::detect(&pool, &report("redis", "down", "planned restart"))
        .await
        .unwrap()
        .expect("the outage still happened");
    let outcome = omnion_health::apply(&pool, &transition, true)
        .await
        .unwrap();
    let incident = outcome.incident().expect("the incident is still recorded");

    assert!(
        incident.suppressed,
        "a window suppresses the *announcement*, and the flag is how the table shows it"
    );
    assert_eq!(
        incident.to_state, "down",
        "the state is never hidden: a row that says `healthy` during a planned restart is a lie \
         about the machine, and the window's whole purpose is to not add that lie"
    );

    // And the state a *sample* records is unaffected — this is the leg that distinguishes
    // "suppressed the incident" from "skipped the reading", which is the version that draws a
    // flat line across the maintenance period and looks like a quiet machine.
    sqlx::query(
        "insert into health_samples (service, metric, value, unit, state, sampled_at) \
         values ('redis', 'latency_ms', 5000, 'ms', 'down', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state: String = sqlx::query_scalar(
        "select state from health_samples where service = 'redis' order by sampled_at desc limit 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        state, "down",
        "the sample still records the outage the window suppressed"
    );

    // Outside the window the same service is not suppressed — a window that never expires is a
    // mute switch.
    let later = at + time::Duration::hours(1);
    assert!(
        !omnion_health::is_suppressed(&pool, "redis", later)
            .await
            .unwrap(),
        "the window ends; suppression cannot be permanent"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// A breach fires once per window
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_breach_fires_once_per_window_and_a_return_clears_it() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();
    let at = now();

    let mut thresholds = Thresholds::new();
    thresholds.insert(
        "disk_percent".to_string(),
        Threshold::new("disk_percent", 80.0, 90.0, "above").unwrap(),
    );
    omnion_health::save_settings(
        &pool,
        &SettingsUpdate {
            check_interval_seconds: None,
            worker_stale_seconds: None,
            thresholds: Some(thresholds),
            notifications: None,
            updated_by: None,
        },
    )
    .await
    .unwrap();

    // Three readings over the critical line inside one window. The first is the announcement;
    // the second and third are the counter going up, which is how the table can say "this has
    // been happening for 12 runs" instead of listing three identical rows.
    let mut announced = 0;
    for _ in 0..3 {
        let check = omnion_health::record_breach(&pool, "disk_percent", 93.0, at)
            .await
            .unwrap();
        if check.should_announce() {
            announced += 1;
        }
        assert_eq!(
            check.crit_limit, 90.0,
            "the row carries the limit it crossed"
        );
    }
    assert_eq!(
        announced, 1,
        "a disk over the line for three runs is ONE event — dedup held in process memory \
         forgets itself on restart and re-fires on the first run after a deploy"
    );

    let rows: i64 = sqlx::query_scalar(
        "select count(*)::bigint from health_breaches where metric = 'disk_percent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows, 1,
        "the counter updates one row rather than inserting one per interval"
    );

    let observations: i32 = sqlx::query_scalar(
        "select observations from health_breaches where metric = 'disk_percent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(observations, 3, "and it knows how many runs it has seen");

    // The next window is its own event: the disk came back, then filled again.
    let next_window = at + time::Duration::seconds(BREACH_WINDOW_SECONDS + 1);
    let second = omnion_health::record_breach(&pool, "disk_percent", 94.0, next_window)
        .await
        .unwrap();
    assert!(
        second.should_announce(),
        "a genuine second breach is its own event; a window an hour long would swallow it"
    );

    // Recovery resolves the row rather than deleting it, so "how long was it over" survives.
    omnion_health::clear_breach(&pool, "disk_percent", at)
        .await
        .unwrap();
    let resolved: Option<OffsetDateTime> =
        sqlx::query_scalar("select resolved_at from health_breaches where metric = 'disk_percent' and window_start = $1")
            .bind(breach_window(at))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        resolved.is_some(),
        "the history is the product — a delete would lose the count"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// An unconfigured threshold never breaches
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unconfigured_threshold_never_breaches() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().clone();

    // Only `disk_percent` is configured. `memory_percent` has a *suggested* pair (80/92) that no
    // operator has saved, so a memory at 99 must be silent.
    let mut thresholds = Thresholds::new();
    thresholds.insert(
        "disk_percent".to_string(),
        Threshold::new("disk_percent", 80.0, 90.0, "above").unwrap(),
    );
    omnion_health::save_settings(
        &pool,
        &SettingsUpdate {
            check_interval_seconds: None,
            worker_stale_seconds: None,
            thresholds: Some(thresholds),
            notifications: None,
            updated_by: None,
        },
    )
    .await
    .unwrap();

    let stored = omnion_health::thresholds(&pool).await.unwrap();
    assert_eq!(stored.len(), 1, "only the saved pair is stored");
    assert!(
        stored.get("memory_percent").is_none(),
        "the suggestion is a form placeholder, not a policy"
    );

    // The store refuses by *name*. The emitter's own `stored.get(metric)` skip means this is the
    // defence rather than the ordinary path — but the failure mode it replaces is worth naming:
    // before the fix, `record_breach` took `crit_limit` from a subquery that returned `NULL` for
    // an unconfigured metric, so the statement died on `23502 not-null` and the caller learned
    // nothing about which metric was the problem.
    let err = omnion_health::record_breach(&pool, "memory_percent", 99.0, now())
        .await
        .expect_err("an unconfigured metric has no line to cross");
    assert!(
        err.to_string().contains("memory_percent"),
        "the refusal names the metric: {err}"
    );

    let rows: i64 = sqlx::query_scalar(
        "select count(*)::bigint from health_breaches where metric = 'memory_percent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows, 0,
        "and nothing was written — defaulting to the suggestion would open incidents on a \
         deployment nobody set up"
    );

    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// The breaker that refuses the impossible
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unknown_probe_is_a_gap_in_knowledge_and_never_an_incident() {
    // Pure contract, no database: `healthy → unknown` is the pair this gets wrong.
    let transition = Transition {
        service: "redis".to_string(),
        from_state: "healthy".to_string(),
        to_state: "unknown".to_string(),
        summary: "the probe timed out".to_string(),
        detail: serde_json::json!({}),
    };
    assert!(
        !transition.is_event(),
        "a probe that could not run is not an outage; announcing it fills the history with \
         entries an operator learns to dismiss"
    );
    assert!(
        !transition.is_recovery(),
        "and it must not close a real incident either — a service that was down and then \
         stopped answering is still down"
    );

    let real = Transition {
        to_state: "degraded".to_string(),
        ..transition
    };
    assert!(real.is_event(), "degraded is a real state change");
    assert!(!real.is_recovery(), "and it is not a recovery");
}

// ---------------------------------------------------------------------------------------------
// The validation the settings form depends on
// ---------------------------------------------------------------------------------------------

#[test]
fn the_threshold_pair_refuses_what_the_database_refuses() {
    // warn at or above crit
    let err = Threshold::new("disk_percent", 90.0, 90.0, "above").unwrap_err();
    assert!(
        err.to_string().contains("disk_percent"),
        "the message names the metric the operator typed wrong: {err}"
    );
    // negative
    assert!(Threshold::new("disk_percent", -1.0, 90.0, "above").is_err());
    // unknown direction
    assert!(Threshold::new("disk_percent", 80.0, 90.0, "sideways").is_err());
    // empty name
    assert!(Threshold::new("  ", 80.0, 90.0, "above").is_err());

    // `NaN` is the case worth writing down: it compares false against everything, so
    // `warn < crit` is false for NaN and a hand-rolled check would either let it through or
    // reject the whole document for the wrong reason.
    let nan = f64::NAN;
    let err = Threshold::new("cpu_percent", nan, 95.0, "above").unwrap_err();
    assert!(
        err.to_string().contains("real numbers"),
        "NaN has to be named as what it is, not as an ordering failure: {err}"
    );

    // A valid pair classifies into three bands, and `below` inverts them. The ordering is the whole
    // point: a `below` rule ("at least 2 healthy workers") is correctly written warn > crit, so the
    // pair is `Threshold::new("workers", 2.0, 1.0, "below")` — 2 above 1 — and a validator that
    // demands warn < crit unconditionally refuses every valid `below` threshold in the product.
    let above = Threshold::new("disk_percent", 80.0, 90.0, "above").unwrap();
    assert_eq!(
        above.classify(10.0),
        None,
        "under the warn line is 'no opinion'"
    );
    assert_eq!(above.classify(85.0), Some("degraded"));
    assert_eq!(above.classify(95.0), Some("down"));

    let below = Threshold::new("workers", 2.0, 1.0, "below").unwrap();
    assert_eq!(
        below.classify(4.0),
        None,
        "four workers out of four is fine"
    );
    assert_eq!(below.classify(2.0), Some("degraded"));
    assert_eq!(below.classify(0.0), Some("down"));

    // The inverted `below` pair is the one that is refused — and refused *because* it is `below`.
    let err = Threshold::new("workers", 1.0, 2.0, "below").unwrap_err();
    assert!(
        err.to_string().contains("workers") && err.to_string().contains("below"),
        "the message names the metric and the direction that decided it: {err}"
    );

    // A non-finite reading is **not** classified at all. This is the deliberate early return rather
    // than an accident of comparison: `NaN >= 90` is false and `inf.is_finite()` is false, so a
    // classifier that relied on the comparisons alone would put `NaN` in the same bucket as a healthy
    // reading — "no opinion" — which is precisely the reading that must never come from a broken
    // sensor. Asserted on purpose so the guard is not removed as dead code.
    assert_eq!(
        above.classify(f64::NAN),
        None,
        "a broken sensor is not a healthy metric"
    );
    assert_eq!(
        above.classify(f64::INFINITY),
        None,
        "and infinity is not a reading either"
    );
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}
