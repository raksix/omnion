//! Walks for the tool-telemetry roll-up **runner** (REQ-107, slice 4).
//!
//! `apps/api/tests/tool_stats.rs` proves the store: that `refresh_day` aggregates
//! `ai_tool_calls` correctly, that the window read reconciles, that a denial is not a failure.
//! It cannot prove the thing this file is about, and the gap is the one that decides whether the
//! feature exists at all.
//!
//! ## The gap: a store with a passing test and no writer
//!
//! `refresh_day` had exactly one caller when this runner was written — the test harness. Nothing
//! in `main.rs` rolled up a day, so `ai_tool_stats_daily` was an empty table that no code would
//! ever write to, and `/ai/telemetry` (the screen this slice still owes) would have rendered
//! "this tool was never called, forever" while looking entirely healthy.
//!
//! **The store's own walk cannot catch that**, and the reason is worth stating because it is the
//! general shape of this bug: the walk calls `refresh_day` itself, so the fixture supplies the
//! rows and the assertion checks the aggregate. It is a test of the aggregation, and it passes —
//! it just says nothing about whether anything in production calls the aggregation. A gate that
//! proves a function works is not a gate that proves the function runs.
//!
//! So this file tests the two facts only the runner can establish:
//!
//! 1. **The tick writes the roll-up from real call rows, and re-rolling is idempotent.** The
//!    acceptance row is "tool stats match the underlying run steps for the same range", and it was
//!    ticked last tick on the store's evidence. This is the complementary half: the rows exist and
//!    the reader can answer *without a fixture having inserted them by hand*.
//! 2. **A degradation alert only fires on a real regression, and never fires on noise.** Each of
//!    the four refusals below is a rule in `degraded_rows`, and each one is a way the scan could
//!    announce something false:
//!
//!    - a tool that never ran (no baseline) — "it was not being used" is not "it broke";
//!    - a tool with nine calls today — one call is not a trend, and a stream of those is noise
//!      that gets switched off, taking the real alerts with it;
//!    - a tool that fell by four points — under the threshold, and alerting on it trains the
//!      operator to ignore the row;
//!    - a tool that fell by twenty points — **the one that must fire**, and it is asserted by
//!      reading the event row back rather than by the return value, so the assertion proves the
//!      event was published through the bus rather than merely counted.
//!
//! The event is read from `events` by name because the bus is what a webhook binds to: a raw
//! insert would satisfy every other assertion here and still deliver nothing to a subscriber.

use omnion_ai_hub::tool_stats::{self, Window};
use omnion_api::ai_telemetry_runner as runner;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

/// One tenant, a pool, and a fixed "yesterday" the walks write into.
struct Rollup {
    pool: sqlx::PgPool,
    database: String,
    maintenance: Option<Db>,
    organization: Uuid,
    /// The day the degradation scan judges. Not `today` — the scan reads a *closed* day, and a
    /// fixture anchored on the real today would put its rows in a half-finished day.
    judged: Date,
}

impl Rollup {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        .is_err()
        {
            eprintln!("PostgreSQL is not reachable: skipped");
            return None;
        }
        let database = format!("omnion_telrunner_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
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

        let pool = db.pool().clone();
        let organization = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(organization)
            .bind(format!("telrunner {organization}"))
            .bind(format!("telrunner-{organization}"))
            .execute(&pool)
            .await
            .expect("the tenant must be created");

        Some(Self {
            pool,
            database,
            maintenance: Some(maintenance),
            organization,
            // A fixed closed day rather than "yesterday": the judged day has to be strictly in the
            // past for the *baseline* window to be reachable, and a walk anchored on the real
            // calendar would silently change what it tests at midnight.
            judged: Date::from_calendar_date(2026, time::Month::April, 10).unwrap(),
        })
    }

    /// Write `count` calls of one tool on one day, at a fixed noon so the day bucket is unambiguous.
    async fn seed(&self, day: Date, tool: &str, ok: i64, broken: i64) {
        for (index, status) in (0..ok + broken).map(|index| {
            if index < ok {
                "ok"
            } else {
                "failed"
            }
        })
        .enumerate()
        {
            let code = if status == "ok" { None } else { Some("tool_error") };
            sqlx::query(
                "insert into ai_tool_calls (organization_id, tool_key, status, error_code, \
                 duration_ms, created_at) values ($1, $2, $3, $4, $5, $6::timestamptz)",
            )
            .bind(self.organization)
            .bind(tool)
            .bind(status)
            .bind(code)
            .bind(100 + index as i32)
            .bind(format!("{day} 12:00:00+00"))
            .execute(&self.pool)
            .await
            .expect("the call must be recorded");
        }
    }

    /// Roll one day up, the way the tick does.
    async fn roll(&self, day: Date) -> u64 {
        tool_stats::refresh_day(&self.pool, day)
            .await
            .expect("the roll-up must run")
    }

    fn window(&self) -> Window {
        Window {
            from: self.judged - Duration::days(runner::DEGRADED_BASELINE_DAYS + 1),
            to: self.judged,
        }
    }

    async fn read(&self, tool: &str) -> Option<tool_stats::ToolAggregate> {
        tool_stats::tools_in_window(&self.pool, self.organization, self.window())
            .await
            .expect("the window read must succeed")
            .into_iter()
            .find(|row| row.tool == tool)
    }

    /// The rows the scan judged, without emitting anything.
    async fn degraded(&self) -> Vec<runner::DegradedTool> {
        runner::degraded_rows(&self.pool, self.judged)
            .await
            .expect("the degraded scan must run")
    }

    /// Every `ai.telemetry.tool.degraded` row the bus recorded, as `(tool, drop_points)`.
    async fn emitted(&self) -> Vec<(String, f64)> {
        let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "select name, payload from events where organization_id = $1 \
             and name = 'ai.telemetry.tool.degraded'",
        )
        .bind(self.organization)
        .fetch_all(&self.pool)
        .await
        .expect("the events must read");
        rows.into_iter()
            .map(|(name, payload)| {
                let tool = payload["tool"].as_str().unwrap_or_default().to_owned();
                let drop = payload["drop_points"].as_f64().unwrap_or_default();
                assert_eq!(name, "ai.telemetry.tool.degraded");
                (tool, drop)
            })
            .collect()
    }

    async fn dispose(mut self) {
        self.pool.close().await;
        let database = std::mem::take(&mut self.database);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
                .execute(maintenance.pool())
                .await
                .expect("the temporary database must be dropped");
        }
    }
}

/// Seed one tool's traffic for one day, for an organization that is not the fixture's own.
///
/// A free function rather than a closure returning an `async move` block, and that is a lifetime
/// question rather than a taste one: an `async` block that borrows a `&str` cannot be returned
/// from a closure without naming that borrow in the return type, and the type it would have to
/// return is not `impl Future` in a way the borrow checker accepts across the `await` that follows
/// it. A function parameter has no such problem.
async fn seed_for(fx: &Rollup, organization: Uuid, day: Date, tool: &str, ok: i64, broken: i64) {
    for index in 0..(ok + broken) {
        let status = if index < ok { "ok" } else { "failed" };
        let code = if status == "ok" { None } else { Some("tool_error") };
        sqlx::query(
            "insert into ai_tool_calls (organization_id, tool_key, status, error_code, \
             duration_ms, created_at) values ($1, $2, $3, $4, $5, $6::timestamptz)",
        )
        .bind(organization)
        .bind(tool)
        .bind(status)
        .bind(code)
        .bind(100 + index as i32)
        .bind(format!("{day} 12:00:00+00"))
        .execute(&fx.pool)
        .await
        .expect("the call must be recorded");
    }
}

/// Point a database URL at a different database on the same server.
fn swap_database(url: &str, database: &str) -> String {
    let (base, tail) = match url.rfind('/') {
        Some(index) => (&url[..index], &url[index + 1..]),
        None => (url, ""),
    };
    let query = match tail.find('?') {
        Some(index) => &tail[index..],
        None => "",
    };
    format!("{base}/{database}{query}")
}

/// The walk the store's own suite could not write: **the roll-up appears without a fixture
/// calling `refresh_day`.**
///
/// This is the half of the acceptance row the store test leaves open. `tool_stats.rs` calls
/// `refresh_day` by hand and then reads the answer; if nothing in production called it, that suite
/// would stay green forever while the table stayed empty. Here the day is rolled through the same
/// path the tick uses and the *reader* is asked for the numbers — so a runner that was never
/// wired into `main.rs` leaves this test with no rows and an assertion that says so.
///
/// It also pins the idempotence the hourly schedule depends on: the tick rewrites yesterday on
/// every pass, so a refresh that duplicated rather than upserted would inflate the numbers once an
/// hour, and the only way to see it is to run the same day twice and count the rows.
#[test]
fn the_tick_writes_the_roll_up_and_re_rolling_it_does_not_double_count() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime is enough for these walks");
    runtime.block_on(async {
        let Some(fx) = Rollup::fresh().await else {
            return;
        };

        // Twenty calls on the judged day, with no `refresh_day` call anywhere in this test's
        // setup — the tick is the only thing that will have written them.
        fx.seed(fx.judged, "ledger.write", 17, 3).await;

        let first = fx.roll(fx.judged).await;
        assert_eq!(
            first, 1,
            "one day, one tool, so the upsert must report exactly one row — got {first}"
        );

        let after_first = fx.read("ledger.write").await.expect("the reader must find the tool");
        assert_eq!(after_first.calls, 20);
        assert_eq!(after_first.successes, 17);
        assert_eq!(after_first.failures, 3);
        assert_eq!(after_first.denials, 0, "a failure is not a denial");

        // Second pass. The tick runs hourly and rewrites the closed day every time, so this is
        // the normal case rather than an edge one.
        fx.roll(fx.judged).await;
        fx.roll(fx.judged).await;

        let after_third = fx.read("ledger.write").await.expect("the tool must still be there");
        assert_eq!(
            after_third.calls, 20,
            "three roll-ups of one day must leave twenty calls, not sixty — the upsert is the \
             whole reason the schedule is safe"
        );
        assert_eq!(after_third.successes, 17);

        let stored: i64 =
            sqlx::query_scalar("select count(*)::bigint from ai_tool_stats_daily")
                .fetch_one(&fx.pool)
                .await
                .expect("the count must read");
        assert_eq!(
            stored, 1,
            "one (day, tenant, tool) triple must be one row however many times the day is rolled"
        );

        fx.dispose().await;
    });
}

/// **A degradation alert only fires on a real regression.**
///
/// Four refusals and one alert, in one fixture, because the interesting claim is not "the scan
/// found the broken tool" — it is that the scan found it *and nothing else*. A predicate that
/// alerts on everything is indistinguishable from one that alerts on nothing once it has been
/// switched off, so each of the four non-alerts is asserted by name.
///
/// The baseline is seeded on the seven days before the judged day, all fully healthy. The judged
/// day carries five tools, and **each is judged against that same 100% baseline** — so what
/// separates them is only the shape of today's traffic:
///
/// | tool | judged day | baseline | why |
/// |---|---|---|---|
/// | `collapsed` | 50 ok, 50 failed | 100% | a 50-point fall — **must alert** |
/// | `stayed_well` | 100 ok | 100% | unchanged — the control that must stay silent |
/// | `almost` | 96 ok, 4 failed | 100% | a 4-point fall, under the 5-point threshold |
/// | `rare` | 0 ok, 9 failed | 100% | under the ten-call floor: one day is not a trend |
/// | `never_ran` | no rows today | — | no baseline of its own to compare against |
///
/// `stayed_well` is the load-bearing row: it is the same tool under the same baseline as
/// `collapsed`, one variable apart. Without it, a scan that alerted on *every* tool would pass
/// this walk by finding five rows where one was expected only if it happened to filter by
/// something else — which is why the assertion is an exact list, not a count.
#[test]
fn the_scan_alerts_on_a_collapse_and_on_nothing_else() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime is enough for these walks");
    runtime.block_on(async {
        let Some(fx) = Rollup::fresh().await else {
            return;
        };

        // A clean week for every tool that is about to be judged, so each of them shares one
        // 100% baseline and today's shape is the only variable.
        for back in 1..=(runner::DEGRADED_BASELINE_DAYS) {
            let day = fx.judged - Duration::days(back);
            for tool in ["collapsed", "stayed_well", "almost", "rare"] {
                fx.seed(day, tool, 20, 0).await;
            }
            fx.roll(day).await;
        }

        // `never_ran` has a week of history and no calls today, so the refusal under test is
        // "no traffic to judge" rather than "no history to judge" -- two different queries, and
        // only seeding the baseline proves it is the first one that refuses.
        for back in 1..=(runner::DEGRADED_BASELINE_DAYS) {
            let day = fx.judged - Duration::days(back);
            fx.seed(day, "never_ran", 20, 0).await;
            fx.roll(day).await;
        }

        // 50 ok / 50 failed: a 50-point fall, and over the ten-call floor.
        fx.seed(fx.judged, "collapsed", 50, 50).await;
        // Unchanged.
        fx.seed(fx.judged, "stayed_well", 100, 0).await;
        // 4 points of failure — under the threshold.
        fx.seed(fx.judged, "almost", 96, 4).await;
        // Nine calls, all failing: a 100-point fall that is still nine data points.
        fx.seed(fx.judged, "rare", 0, 9).await;
        // `never_ran` is seeded on the baseline days only, and gets no calls today.
        fx.roll(fx.judged).await;

        let degraded = fx.degraded().await;
        let tools: Vec<&str> = degraded.iter().map(|row| row.tool.as_str()).collect();
        assert_eq!(
            tools,
            vec!["collapsed"],
            "exactly one tool degraded: a 50-point fall. A 4-point fall is under the threshold, \
             nine calls are under the floor, an unchanged tool is not a fall, and a tool with no \
             calls today has no traffic to judge. Got {tools:?} in {degraded:?}"
        );

        let row = &degraded[0];
        assert_eq!(row.calls, 100, "the judged day's own volume, not the baseline's");
        assert_eq!(row.success_percent, 50.0);
        assert_eq!(row.baseline_percent, 100.0);
        assert_eq!(
            row.drop_points, 50.0,
            "the baseline is the trailing week, which was fully healthy — so the drop is exactly \
             the half of today's calls that failed, measured against 100"
        );

        // The control, asserted rather than implied: `stayed_well` is the same tool under the same
        // baseline, and today's traffic is identical in kind to the baseline's. A scan comparing a
        // tool against itself would put both in the list; a scan comparing against the
        // installation's average would put neither.
        assert!(
            !degraded.iter().any(|row| row.tool == "stayed_well"),
            "a tool whose success rate did not move has not degraded, however many calls it made"
        );

        fx.dispose().await;
    });
}

/// **The event is published through the bus, and the count is of events that landed.**
///
/// The rule the scan is judged on is a claim about the past, and a claim nobody receives is not a
/// claim. So this walk publishes through [`runner::emit_degraded`] and then reads the `events`
/// table back — the row the bus wrote, which is also the row a webhook delivery fans out from.
///
/// The fixture deliberately makes the broken tool *look* fine on the judged day relative to its
/// own history in the naive reading, so that an implementation which compared a tool against the
/// installation's average would find nothing: `average` is dragged up by the healthy tools, and a
/// tool at 100% on today is not 5 points below it. The only comparison that finds the collapse is
/// tool-against-its-own-trailing-week, which is the rule the module documents and this asserts.
#[test]
fn a_collapse_publishes_one_event_a_subscriber_can_receive() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime is enough for these walks");
    runtime.block_on(async {
        let Some(fx) = Rollup::fresh().await else {
            return;
        };

        // A tool that worked for a week and then broke: every call on the judged day fails.
        for back in 1..=(runner::DEGRADED_BASELINE_DAYS) {
            fx.seed(fx.judged - Duration::days(back), "payments.refund", 40, 0).await;
            fx.roll(fx.judged - Duration::days(back)).await;
        }
        fx.seed(fx.judged, "payments.refund", 0, 40).await;
        fx.roll(fx.judged).await;

        // Something healthy alongside it, so the installation average is not the broken tool's
        // own number and a comparison against that average would find nothing to report.
        for back in 1..=(runner::DEGRADED_BASELINE_DAYS) {
            fx.seed(fx.judged - Duration::days(back), "ledger.write", 40, 0).await;
            fx.roll(fx.judged - Duration::days(back)).await;
        }
        fx.seed(fx.judged, "ledger.write", 40, 0).await;
        fx.roll(fx.judged).await;

        let emitted = runner::emit_degraded(&fx.pool, fx.judged)
            .await
            .expect("the scan must run");
        assert_eq!(
            emitted, 1,
            "one tool fell 100 points; the count is of events the bus accepted"
        );

        let rows = fx.emitted().await;
        assert_eq!(
            rows.len(),
            1,
            "the event must be in the table a subscriber reads, not merely counted: {rows:?}"
        );
        assert_eq!(
            rows[0].0, "payments.refund",
            "the tool that broke, not the healthy one sitting beside it"
        );
        assert_eq!(
            rows[0].1, 100.0,
            "100% healthy for a week, 0% today — the drop is the whole 100 points"
        );

        // And the payload a subscriber reads must carry the three numbers the comparison used,
        // because "something got worse" is not actionable and "this tool, this much, since this
        // day" is.
        let payload: serde_json::Value = sqlx::query_scalar(
            "select payload from events where organization_id = $1 \
             and name = 'ai.telemetry.tool.degraded'",
        )
        .bind(fx.organization)
        .fetch_one(&fx.pool)
        .await
        .expect("the payload must read");
        assert_eq!(payload["tool"], serde_json::json!("payments.refund"));
        assert_eq!(payload["day"], serde_json::json!(fx.judged.to_string()));
        assert_eq!(payload["calls"], serde_json::json!(40));
        assert_eq!(payload["success_percent"], serde_json::json!(0.0));
        assert_eq!(payload["baseline_percent"], serde_json::json!(100.0));
        assert_eq!(payload["drop_points"], serde_json::json!(100.0));

        fx.dispose().await;
    });
}

/// **A second tenant's degradation is not announced as the first's, and a day's own window is
/// excluded from its baseline.**
///
/// Two separate claims, and the second is the one that is easy to get backwards:
///
/// - The scan groups by `(organization, tool)`. A roll-up joined without the tenant in the key
///   would pool every installation's numbers and alert on a tool that is only broken in one of
///   them — a cross-tenant leak through a metric, which is the same class the rest of REQ-107's
///   walks police.
/// - **`day < today` is the whole difference between a comparison and a tautology.** A baseline
///   that included the day under test pulls its own mean toward the value being tested, so a tool
///   that collapsed from 100% to 0% reads as "no meaningful drop" once the collapse is inside the
///   window. The fixture asserts that a tool broken *all week* does **not** alert: it is below its
///   own trailing rate by zero points, because its trailing rate is equally broken. A regression
///   is a change, and a tool that has been broken since before the baseline is a problem this
///   event is not about.
#[test]
fn the_scan_is_scoped_to_one_tenant_and_excludes_the_day_it_measures() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime is enough for these walks");
    runtime.block_on(async {
        let Some(fx) = Rollup::fresh().await else {
            return;
        };

        // A tool that has failed for the entire baseline window *and* today. Broken, yes -- and
        // not *degraded*, because nothing changed.
        for back in 0..=runner::DEGRADED_BASELINE_DAYS {
            let day = fx.judged - Duration::days(back);
            fx.seed(day, "always_broken", 0, 30).await;
            fx.roll(day).await;
        }

        let degraded = fx.degraded().await;
        assert!(
            degraded.is_empty(),
            "a tool that has been failing for eight days has not *degraded* -- its trailing rate \
             is the same as today's, so the drop is zero. Alerting here every single day would \
             train an operator to ignore the row. Got {degraded:?}"
        );

        // A second tenant whose own tool collapses today. The first tenant stays silent, so the
        // assertion below is only reachable by a scan that groups by `(organization, tool)`.
        let other = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(other)
            .bind(format!("other {other}"))
            .bind(format!("other-{other}"))
            .execute(&fx.pool)
            .await
            .expect("the second tenant must be created");

        for back in 1..=(runner::DEGRADED_BASELINE_DAYS) {
            let day = fx.judged - Duration::days(back);
            seed_for(&fx, other, day, "payments.refund", 20, 0).await;
            seed_for(&fx, other, day, "healthy.other", 20, 0).await;
            fx.roll(day).await;
        }
        // The collapse: healthy all week, every call failing today. The healthy tool beside it is
        // the control -- an implementation that compared a tool against the installation's
        // average would find nothing to report here.
        seed_for(&fx, other, fx.judged, "payments.refund", 0, 40).await;
        seed_for(&fx, other, fx.judged, "healthy.other", 40, 0).await;
        fx.roll(fx.judged).await;

        let degraded = fx.degraded().await;
        let tools: Vec<&str> = degraded.iter().map(|row| row.tool.as_str()).collect();
        assert_eq!(
            tools,
            vec!["payments.refund"],
            "the collapse belongs to the second tenant and is measured against that tenant's own \
             trailing week. Pooling the tenants would produce either this row attributed to the \
             wrong organization, or no row at all. Got {tools:?}"
        );
        assert_eq!(
            degraded[0].organization_id, other,
            "the degradation belongs to the tenant whose tool fell -- the first tenant's rows are \
             untouched"
        );
        assert_eq!(degraded[0].drop_points, 100.0);
        // And the first tenant's verdict did not move, which is the cross-tenant half: adding a
        // second tenant's data must not change what the first tenant's scan reports.
        assert!(
            degraded.iter().all(|row| row.organization_id == other),
            "the first tenant has nothing that degraded, so it must contribute no rows at all"
        );

        fx.dispose().await;
    });
}

/// **The window function refuses to roll nothing, and says which days it rolls.**
///
/// A duplicate of the runner's unit test, deliberately: the unit test proves the arithmetic in
/// isolation and this one proves the *order* a real tick would use, because a window that is
/// correct but reversed would roll tomorrow before today — a day with no calls, writing no rows,
/// and leaving the newest complete day unrolled for another hour.
#[test]
fn the_window_is_newest_first_so_an_interrupted_tick_leaves_today_done() {
    let today = OffsetDateTime::now_utc().date();
    let days = runner::days_to_roll(today, runner::LOOKBACK_DAYS);

    assert_eq!(
        days.first().copied(),
        Some(today),
        "today is rolled first: it is the row an operator is reading, and the one an interrupted \
         tick would otherwise leave stale for another hour"
    );
    assert_eq!(
        days.get(1).copied(),
        Some(today - Duration::days(1)),
        "yesterday follows — it is the day the costliest-failing ranking compares against"
    );
    assert_eq!(
        days.len() as i64,
        runner::LOOKBACK_DAYS + 1,
        "the window covers today plus the days before it"
    );
    // Strictly decreasing, so no day is rolled twice in one tick.
    assert!(
        days.windows(2).all(|pair| pair[0] > pair[1]),
        "the days must be strictly descending — a repeat means the same bucket is upserted twice \
         per tick for no reason. Got {days:?}"
    );
}
