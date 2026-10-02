//! Walks for the tool-statistics roll-up of REQ-107 (slice 4).
//!
//! `crates/ai-hub/src/tool_stats.rs` proves the arithmetic that needs no database (a success rate
//! is a division). This file proves the thing that only a database can answer, and the acceptance
//! row names it exactly: **"`/ai/telemetry` tool stats match the underlying run steps for the same
//! range."**
//!
//! A roll-up is the one kind of store that can be perfectly self-consistent and completely wrong.
//! Every row it writes comes from its own `insert … select`, so a reader can never catch it
//! disagreeing with itself — and if the aggregation counts the wrong rows, or buckets a denial as
//! a failure, or double-counts a call that produced two steps, the screen shows a plausible,
//! internally consistent, entirely fictional success rate. **The only way to catch that class of
//! bug is to write the underlying rows by hand and then compare the two numbers.** So every walk
//! here inserts `ai_tool_calls` rows directly and asserts the roll-up against a count computed
//! from the same rows by a separate query.
//!
//! What is asserted:
//!
//! - **The roll-up reconciles with `ai_tool_calls`, row for row and bucket for bucket.** The
//!   counts come from the log table; the percentiles come from a `percentile_cont` computed here.
//!   Two independent paths to the same number is the whole point.
//! - **A denial is not a failure.** The walk writes one `denied` call and asserts it lands in the
//!   `denials` column with `failures` unchanged — the split REQ-100's `status = 'denied'` exists
//!   for, and the one an operator's "widen permissions or fix the tool?" question depends on.
//! - **The refresh is idempotent.** Running it twice must produce the same table, not double it.
//!   This is the property that makes it safe to schedule hourly *and* safe to re-run after a
//!   failure, so it is asserted by counting rows, not by eyeballing a value.
//! - **A second organization cannot read the first's numbers.** The window read is scoped by
//!   organization; an unscoped read is a cross-tenant telemetry leak, which is the same
//!   cross-tenant class the other REQ-107 walks already assert for suites and runs.
//! - **Cost arrives from the step the call points at.** `ai_tool_calls` has no cost column, so the
//!   roll-up joins `ai_run_steps`. A fixture with a call whose step is absent proves the join is a
//!   `left` join and not an inner one — an inner join would silently drop the call from `calls`
//!   and make a tool look less used than it was.
//!
//! The fixtures insert rows with an explicit `created_at` on a fixed day, so the window is a
//! closed interval the walk controls and the assertions never depend on when the suite ran.

use std::sync::Mutex;

use omnion_ai_hub::tool_stats::{self, Window};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// One tool call, as the loop would have written it.
#[derive(Debug, Clone)]
struct Call {
    tool: String,
    status: &'static str,
    duration_ms: i32,
    error_code: Option<&'static str>,
    /// Cost on the *step* the call points at. `None` = no step (a pruned or unrecorded step).
    cost_micros: Option<i64>,
}

/// A throwaway database, migrated, plus the tenant and day every walk writes into.
struct Stats {
    pool: sqlx::PgPool,
    database: String,
    maintenance: Option<Db>,
    organization: Uuid,
    day: Date,
    guard: Mutex<()>,
}

impl Stats {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let base = DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        };
        if Db::connect(&base).await.is_err() {
            eprintln!("PostgreSQL is not reachable at {}: skipped", config.database.url);
            return None;
        }
        let database = format!("omnion_toolstats_{}", Uuid::new_v4().simple());
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
            .bind(format!("toolstats {organization}"))
            .bind(format!("toolstats-{organization}"))
            .execute(&pool)
            .await
            .expect("the tenant must be created");

        Some(Self {
            pool,
            database,
            maintenance: Some(maintenance),
            organization,
            day: Date::from_calendar_date(2026, time::Month::March, 3).unwrap(),
            guard: Mutex::new(()),
        })
    }

    /// A window wide enough to hold the fixed day on both sides.
    fn window(&self) -> Window {
        Window {
            from: Date::from_calendar_date(2026, time::Month::March, 1).unwrap(),
            to: Date::from_calendar_date(2026, time::Month::March, 5).unwrap(),
        }
    }

    /// Write a run, and `calls.len()` tool calls on it, all timestamped on the fixed day.
    ///
    /// **One run per tool set, so `calls` and `successes` are comparable without a join.**
    /// `run_id` is required (`ai_tool_calls.run_id` is not null) but nothing in the roll-up reads
    /// it — the aggregation is per `(organization, tool)` across runs, which is the point: a tool
    /// used by five agents is one row, not five.
    async fn seed(&self, agent_key: &str, calls: &[Call]) {
        let agent = Uuid::new_v4();
        sqlx::query("insert into ai_agents (id, organization_id, name, key) values ($1, $2, $3, $4)")
            .bind(agent)
            .bind(self.organization)
            .bind(format!("agent {agent_key}"))
            .bind(format!("{agent_key}-{agent}"))
            .execute(&self.pool)
            .await
            .expect("the agent must be created");

        let run = Uuid::new_v4();
        sqlx::query(
            // A finished run must name why, and `stop_reason` is a **closed vocabulary**
            // (`final_answer`, `max_steps`, `deadline`, `token_budget`, `cancelled`,
            // `loop_detected`, `error`) — a free-text reason is refused by the schema, which is
            // what makes the run list's "why did this end" column sortable and groupable. So the
            // fixture says `final_answer`, the reason a run that did its job actually carries.
            // `finished_at` is set for the same reason: a stamp is what distinguishes finished
            // from going, and leaving it null would write a row the runtime never produces.
            "insert into ai_runs (id, organization_id, agent_id, status, stop_reason, goal, finished_at) \
             values ($1, $2, $3, 'completed', 'final_answer', $4, $5::timestamptz)",
        )
        .bind(run)
        .bind(self.organization)
        .bind(agent)
        // `goal` is not null and nothing in the roll-up reads it; it is written because the
        // schema requires a value, and the value says plainly that this row exists to give the
        // tool calls something to hang from.
        .bind("tool telemetry fixture")
        // Finished before the calls it carries, so the steps and calls are strictly inside the
        // run's lifetime — the same ordering the runtime writes.
        .bind(format!("{} 11:59:00+00", self.day))
        .execute(&self.pool)
        .await
        .expect("the run must be created");

        // `ai_run_steps` is unique on `(run_id, step_no)`, so the fixture numbers its steps —
        // a constant 1 would let only the first call of a run carry a step, and the walk about a
        // *missing* step would be measuring the fixture instead of the roll-up's join.
        for (index, call) in calls.iter().enumerate() {
            // The step is written first because the call points at it, and `cost_micros` lives on
            // the step — the price was snapshotted there at call time (REQ-098 slice 5).
            let step = Uuid::new_v4();
            let has_step = call.cost_micros.is_some();
            if has_step {
                sqlx::query(
                    "insert into ai_run_steps (id, run_id, step_no, kind, tool, status, cost_micros, duration_ms) \
                     values ($1, $2, $3, 'tool_call', $4, 'completed', $5, $6)",
                )
                .bind(step)
                .bind(run)
                .bind(index as i32 + 1)
                .bind(&call.tool)
                .bind(call.cost_micros.unwrap_or_default())
                .bind(call.duration_ms)
                .execute(&self.pool)
                .await
                .expect("the step must be created");
            }

            // `step_id` is a foreign key with `on delete set null`, so a call whose step is gone
            // carries a **NULL** step_id rather than a sentinel uuid — the column's `null` is the
            // only value the schema accepts for "no step", and `Uuid::nil()` would be rejected by
            // the foreign key. This is the shape a pruned step leaves behind, so the roll-up's
            // left join is exercised against the real thing.
            let step_id: Option<Uuid> = has_step.then_some(step);
            sqlx::query(
                // `ai_tool_calls.id` is a `bigserial`, so it takes its own default — naming it
                // would be a uuid into a bigint column. Nothing in the roll-up reads it.
                "insert into ai_tool_calls (run_id, agent_id, organization_id, step_id, tool_key, status, error_code, duration_ms, created_at) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz)",
            )
            .bind(run)
            .bind(agent)
            .bind(self.organization)
            .bind(step_id)
            .bind(&call.tool)
            .bind(call.status)
            .bind(call.error_code)
            .bind(call.duration_ms)
            .bind(format!("{} 12:00:00+00", self.day))
            .execute(&self.pool)
            .await
            .expect("the call must be recorded");
        }
    }

    async fn refresh(&self) -> u64 {
        tool_stats::refresh_day(&self.pool, self.day)
            .await
            .expect("the roll-up must run")
    }

    async fn read(&self, tool: &str) -> Option<tool_stats::ToolAggregate> {
        tool_stats::tools_in_window(&self.pool, self.organization, self.window())
            .await
            .expect("the window read must succeed")
            .into_iter()
            .find(|row| row.tool == tool)
    }

    /// The counts, counted straight off `ai_tool_calls` — the independent path.
    async fn truth(&self, tool: &str) -> (i64, i64, i64, i64) {
        let row: (i64, i64, i64, i64) = sqlx::query_as(
            "select count(*)::bigint, \
                    count(*) filter (where status = 'ok')::bigint, \
                    count(*) filter (where status not in ('ok','denied'))::bigint, \
                    count(*) filter (where status = 'denied')::bigint \
             from ai_tool_calls where tool_key = $1",
        )
        .bind(tool)
        .fetch_one(&self.pool)
        .await
        .expect("the truth query must succeed");
        row
    }

    /// One settled agent run on the fixed day: a status, a stop reason and a cost.
    ///
    /// A run is written by hand rather than by the runtime because the runtime needs a model, a
    /// provider and a step loop to get to the state — and a walk whose fixture has to stand up
    /// an inference stack to prove a `count(*) filter` is a walk that breaks the week someone
    /// moves that column. Every column written here is one the schema requires, not one a
    /// summary reads: `goal` is the run's own text, `stop_reason` is the closed vocabulary the
    /// run list renders, and the cost is the run's own total.
    ///
    /// `stop_reason` is `Option` and `finished_at` is derived from the status, because the schema
    /// will not accept the alternative: `ai_runs_finished_has_stamp` refuses a stamp on an
    /// unsettled run and `ai_runs_reason_implies_finished` refuses a reason on one either. A
    /// helper that wrote both unconditionally would be writing rows the runtime never produces —
    /// and it would do it *silently*, because the failure shows up as a constraint violation in
    /// whatever walk happens to seed an in-flight run first.
    async fn seed_run(
        &self,
        status: &str,
        stop_reason: Option<&str>,
        cost_micros: i64,
        steps: i32,
    ) {
        let agent = Uuid::new_v4();
        sqlx::query("insert into ai_agents (id, organization_id, name, key) values ($1, $2, $3, $4)")
            .bind(agent)
            .bind(self.organization)
            .bind(format!("run agent {agent}"))
            .bind(format!("run-agent-{agent}"))
            .execute(&self.pool)
            .await
            .expect("the run's agent must be created");

        // A run is settled or it is not, and the two shapes differ in the columns the schema
        // demands: an unsettled run has no finish stamp and no reason, a settled one has both.
        let settled = matches!(status, "completed" | "failed" | "cancelled");
        let finished_at = settled.then(|| format!("{} 09:30:00+00", self.day));
        let stop_reason = settled.then_some(stop_reason).flatten();

        let run = Uuid::new_v4();
        sqlx::query(
            "insert into ai_runs (id, organization_id, agent_id, status, stop_reason, goal, \
             cost_micros, started_at, finished_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9::timestamptz)",
        )
        .bind(run)
        .bind(self.organization)
        .bind(agent)
        .bind(status)
        .bind(stop_reason)
        .bind("telemetry run fixture")
        .bind(cost_micros)
        .bind(format!("{} 09:00:00+00", self.day))
        .bind(finished_at)
        .execute(&self.pool)
        .await
        .expect("the run must be created");

        for index in 0..steps {
            sqlx::query(
                "insert into ai_run_steps (run_id, step_no, kind, tool, status) \
                 values ($1, $2, 'tool_call', $3, 'completed')",
            )
            .bind(run)
            .bind(index + 1)
            .bind(format!("step_tool_{index}"))
            .execute(&self.pool)
            .await
            .expect("the run's step must be created");
        }
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

macro_rules! stats {
    () => {
        match Stats::fresh().await {
            Some(fixture) => fixture,
            None => panic!(
                "PostgreSQL is required for the REQ-107 telemetry walks; a skipped walk proves nothing"
            ),
        }
    };
}

/// Swap the database name in a URL, keeping the credentials the harness was given.
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

/// **The roll-up reconciles with `ai_tool_calls`, and the percentiles are real percentiles.**
///
/// This is the acceptance row. The counts are compared against a count taken from the log table by
/// a separate query, and the latency percentiles against a `percentile_cont` computed here — so
/// the assertion does not reuse the production expression, which would be a tautology.
///
/// The fixture is deliberately lopsided: four `ok`, two `failed`, one `denied`, one `timeout`,
/// with durations 10/20/30/40/50/60/900. A runner that bucketed `denied` into `failures` reports
/// three instead of two; a runner that double-counted a call reports fourteen instead of seven; and
/// a runner that averaged the daily percentiles instead of taking a quantile gets a p99 near 700
/// where the true one is 780.
#[tokio::test]
async fn the_rollup_reconciles_with_the_calls_it_summarises() {
    let fx = stats!();
    fx.seed(
        "lopsided",
        &[
            Call { tool: "search".into(), status: "ok", duration_ms: 10, error_code: None, cost_micros: Some(100) },
            Call { tool: "search".into(), status: "ok", duration_ms: 20, error_code: None, cost_micros: Some(100) },
            Call { tool: "search".into(), status: "ok", duration_ms: 30, error_code: None, cost_micros: Some(100) },
            Call { tool: "search".into(), status: "ok", duration_ms: 40, error_code: None, cost_micros: Some(100) },
            Call {
                tool: "search".into(),
                status: "failed",
                duration_ms: 50,
                error_code: Some("tool_timeout"),
                cost_micros: Some(250),
            },
            Call {
                tool: "search".into(),
                status: "denied",
                duration_ms: 60,
                error_code: Some("permission_denied"),
                cost_micros: Some(0),
            },
            Call {
                tool: "search".into(),
                status: "timeout",
                duration_ms: 900,
                error_code: Some("tool_timeout"),
                cost_micros: Some(900),
            },
        ],
    )
    .await;
    fx.refresh().await;

    let (calls, successes, failures, denials) = fx.truth("search").await;
    let row = fx
        .read("search")
        .await
        .expect("the roll-up must carry the tool the calls recorded");

    assert_eq!(row.calls, calls, "the roll-up counted a different number of calls");
    assert_eq!(row.successes, successes, "the ok bucket disagrees with the log");
    // The point of the split: `denied` is NOT a failure.
    assert_eq!(
        row.failures, failures,
        "denials must not land in failures (the log has {} failures, the roll-up claims {})",
        failures, row.failures
    );
    assert_eq!(row.denials, denials, "the denial bucket disagrees with the log");
    // 4×100 (the four `ok` rows) + 250 (the `failed` one) + 0 (the denied one — it never ran, so
    // its step costs nothing) + 900 (the `timeout` one) = 1550. The constant is written as the
    // sum rather than as a number because the first draft of this assertion said 1350 and was
    // wrong: the timeout row's cost was dropped from the arithmetic. A hand-copied total is a
    // second source of truth that disagrees with the first, and the walk then "proves" whatever
    // the author believed rather than what the code does.
    assert_eq!(row.cost_micros, 1550, "cost must sum the steps' own cost_micros");
    assert_eq!(row.days_seen, 1);

    // Percentiles over [10, 20, 30, 40, 50, 60, 900]: the interpolated quantiles, computed here
    // rather than by the production expression.
    let durations: Vec<i32> = vec![10, 20, 30, 40, 50, 60, 900];
    let p = |q: f64| {
        let mut sorted = durations.clone();
        sorted.sort_unstable();
        let pos = q * (sorted.len() as f64 - 1.0);
        let low = pos.floor() as usize;
        let high = pos.ceil() as usize;
        (sorted[low] as f64 + (sorted[high] as f64 - sorted[low] as f64) * (pos - low as f64))
            .round() as i32
    };
    assert_eq!(row.p50_ms, Some(p(0.50)), "p50 must be the median of the call durations");
    assert_eq!(row.p95_ms, Some(p(0.95)), "p95 must be interpolated, not a sample");
    assert_eq!(row.p99_ms, Some(p(0.99)), "p99 must interpolate the tail, not clamp to max");

    // The failure histogram: `tool_timeout` twice (the `failed` and the `timeout` row), and the
    // denial's `permission_denied` must NOT be in it — it was not a failure.
    assert_eq!(
        row.error_codes.get("tool_timeout"),
        Some(&2),
        "the two timeout-shaped failures must be counted together, got {:?}",
        row.error_codes
    );
    assert!(
        !row.error_codes.contains_key("permission_denied"),
        "a denial is not a failure and must not appear in the failure histogram"
    );

    fx.dispose().await;
}

/// **The refresh is idempotent** — twice is the same table, not twice the table.
#[tokio::test]
async fn refreshing_a_day_twice_is_the_same_table() {
    let fx = stats!();
    fx.seed(
        "twice",
        &[
            Call { tool: "mail".into(), status: "ok", duration_ms: 5, error_code: None, cost_micros: Some(10) },
            Call { tool: "mail".into(), status: "ok", duration_ms: 7, error_code: None, cost_micros: Some(10) },
        ],
    )
    .await;

    fx.refresh().await;
    let first = fx.read("mail").await.expect("the first refresh must write a row");
    fx.refresh().await;
    let second = fx.read("mail").await.expect("the second refresh must still write the row");

    // One row, not two: the primary key is `(day, organization_id, tool)` and the refresh is an
    // upsert, so the second run rewrites the numbers. A refresh that inserted would double the
    // counts, and a screen with no way to tell would report a day's traffic as growing.
    assert_eq!(first.calls, second.calls, "a re-run must not double the day's calls");
    assert_eq!(first.calls, 2);
    assert_eq!(first.cost_micros, second.cost_micros);

    let rows: i64 = sqlx::query_scalar(
        "select count(*)::bigint from ai_tool_stats_daily where day = $1",
    )
    .bind(fx.day)
    .fetch_one(&fx.pool)
    .await
    .expect("the roll-up table must be countable");
    assert_eq!(rows, 1, "the second refresh must update the row, not add one");

    fx.dispose().await;
}

/// **A call whose step is gone still counts as a call** — and contributes no cost.
///
/// `ai_tool_calls` has no cost column; the price lives on the step (REQ-098 slice 5). So the roll-up
/// joins, and this walk proves it is a **left** join: an inner join would drop this call from
/// `calls` entirely and make a tool look less used than it was, which no count-based assertion
/// elsewhere would catch because the other fixtures all have steps.
#[tokio::test]
async fn a_call_without_a_step_still_counts_and_costs_nothing() {
    let fx = stats!();
    fx.seed(
        "nostep",
        &[
            Call { tool: "pruned".into(), status: "ok", duration_ms: 11, error_code: None, cost_micros: None },
            Call { tool: "pruned".into(), status: "ok", duration_ms: 13, error_code: None, cost_micros: Some(70) },
        ],
    )
    .await;
    fx.refresh().await;

    let row = fx.read("pruned").await.expect("the roll-up must carry the tool");
    assert_eq!(row.calls, 2, "a pruned step must not drop the call from the day's count");
    assert_eq!(row.successes, 2);
    assert_eq!(
        row.cost_micros, 70,
        "only the call whose step survives contributes cost; the rest contributes zero, not the whole row"
    );

    fx.dispose().await;
}

/// **One organization's telemetry is nobody else's.** The same tool, two tenants, one busy and
/// one silent: the silent tenant must read zero rows rather than the busy tenant's numbers.
#[tokio::test]
async fn a_tenant_reads_only_its_own_tool_numbers() {
    let fx = stats!();

    let other = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other)
        .bind("other tenant")
        .bind(format!("other-{other}"))
        .execute(&fx.pool)
        .await
        .expect("the second tenant must be created");

    fx.seed(
        "shared",
        &[Call { tool: "shared_tool".into(), status: "ok", duration_ms: 3, error_code: None, cost_micros: Some(5) }],
    )
    .await;
    fx.refresh().await;

    let theirs: i64 = sqlx::query_scalar(
        "select count(*)::bigint from ai_tool_stats_daily where organization_id = $1",
    )
    .bind(other)
    .fetch_one(&fx.pool)
    .await
    .expect("the roll-up must be countable");
    assert_eq!(theirs, 0, "the silent tenant must have no rows of its own");

    // And the read is scoped even if rows existed for the other tenant.
    let visible = tool_stats::tools_in_window(&fx.pool, other, fx.window())
        .await
        .expect("the window read must succeed");
    assert!(
        visible.is_empty(),
        "tenant B read tenant A's telemetry: {visible:?} — the window read is not organization-scoped"
    );

    fx.dispose().await;
}

/// **A tool with no failures is not the "costliest failing tool".**
///
/// The panel under the scatter ranks the most expensive *failing* tool per day. A healthy
/// expensive tool must not appear there: including it would put a working tool above a broken one
/// and answer the wrong question. This walk seeds a cheap failing tool and an expensive healthy
/// one and asserts only the failing tool is returned.
#[tokio::test]
async fn the_costliest_failing_tool_ignores_a_tool_that_never_failed() {
    let fx = stats!();

    fx.seed(
        "mixed",
        &[
            // The broken one, and cheap.
            Call {
                tool: "flaky".into(),
                status: "failed",
                duration_ms: 9,
                error_code: Some("tool_error"),
                cost_micros: Some(10),
            },
        ],
    )
    .await;
    fx.seed(
        "healthy",
        &[Call {
            tool: "solid".into(),
            status: "ok",
            duration_ms: 900,
            error_code: None,
            // Twenty times the failing tool's cost.
            cost_micros: Some(200),
        }],
    )
    .await;
    fx.refresh().await;

    let rows = tool_stats::costliest_failing_per_day(&fx.pool, fx.organization, fx.window())
        .await
        .expect("the ranking must read");
    assert_eq!(
        rows.len(),
        1,
        "exactly one failing tool qualifies for the day, got {rows:?}"
    );
    let (day, tool, _failures, cost) = &rows[0];
    assert_eq!(*day, fx.day);
    assert_eq!(tool, "flaky", "the expensive but healthy tool must not be ranked");
    assert_eq!(*cost, 10);

    fx.dispose().await;
}

/// **The step histogram counts settled runs, files a stepless one under zero, and ignores the
/// ones still in flight.**
///
/// Three claims, each of which the chart gets wrong in a way no count elsewhere would notice:
///
/// 1. **A run with no steps is a bar, not a gap.** `inner join` would drop it, and "the agent
///    answered without calling a single tool" is the row that explains a flat histogram.
/// 2. **A run still in flight is not counted.** Its step count is a prefix, so including it makes
///    the shape of the chart depend on when the page was loaded — the one property a
///    distribution must not have.
/// 3. **A queued run is not counted either**, and not merely for symmetry: `started_at` is null
///    for it, so there is no day to file it under. A reader that bucketed it anyway would be
///    inventing a timestamp.
#[tokio::test]
async fn the_step_histogram_files_a_stepless_run_and_ignores_one_still_going() {
    let fx = stats!();

    // 0 steps — answered from nothing.
    fx.seed_run("completed", Some("final_answer"), 100, 0).await;
    // 1 step.
    fx.seed_run("completed", Some("final_answer"), 200, 1).await;
    // 3 steps, twice — the bar has to accumulate rather than overwrite.
    fx.seed_run("completed", Some("final_answer"), 300, 3).await;
    fx.seed_run("completed", Some("max_steps"), 400, 3).await;

    // Two runs the histogram must not see. `running` has a step count that is still growing, and
    // `queued` has no `started_at` at all — the schema's own "never began" marker.
    fx.seed_run("running", None, 900, 5).await;
    let queued_started_at: Option<OffsetDateTime> =
        sqlx::query_scalar("insert into ai_runs (organization_id, status, goal, cost_micros) \
         values ($1, 'queued', 'queued fixture', 999) returning started_at")
            .bind(fx.organization)
            .fetch_one(&fx.pool)
            .await
            .expect("the queued run must be created");
    // The reason the reader may exclude it rather than having to guess: a queued run has no day
    // at all. Asserted here so the walk states the fact it depends on rather than trusting it.
    assert_eq!(
        queued_started_at, None,
        "a queued run has no started_at — the reader must not be inventing one for it"
    );

    let buckets = tool_stats::step_histogram(&fx.pool, fx.organization, fx.window())
        .await
        .expect("the histogram must read");
    let seen: std::collections::BTreeMap<i32, i64> = buckets
        .iter()
        .map(|bucket| (bucket.steps, bucket.runs))
        .collect();

    assert_eq!(seen.get(&0), Some(&1), "a run with no steps is a bar of one, not a gap");
    assert_eq!(seen.get(&1), Some(&1));
    assert_eq!(seen.get(&3), Some(&2), "two three-step runs are one bar of two");
    assert!(
        !seen.contains_key(&5),
        "a running run has a step count that is a prefix; the chart must not include it ({seen:?})"
    );
    assert_eq!(
        seen.values().sum::<i64>(),
        4,
        "exactly the four settled runs count: {seen:?}"
    );

    fx.dispose().await;
}

/// **Cost per solved task divides by the successes, and a run that hit `max_steps` is not one.**
///
/// The fixture is three runs on one day: one that answered, one that ran out of steps, one that
/// failed. The ratio is asserted against the arithmetic a reader would do by hand, and the trap
/// is explicit — `completed` **alone** is not "solved". A runaway that burned its whole budget is
/// recorded `completed`, so a reader that counted statuses instead of stop reasons would halve
/// this denominator and double the reported price of every success while calling a loop "a win".
#[tokio::test]
async fn cost_per_solved_counts_only_runs_that_reached_an_answer() {
    let fx = stats!();

    fx.seed_run("completed", Some("final_answer"), 200_000, 2).await;
    fx.seed_run("completed", Some("max_steps"), 600_000, 9).await;
    fx.seed_run("failed", Some("error"), 200_000, 4).await;

    let days = tool_stats::cost_per_solved(&fx.pool, fx.organization, fx.window())
        .await
        .expect("the scatter's source must read");
    assert_eq!(days.len(), 1, "one day of runs is one point: {days:?}");

    let day = &days[0];
    assert_eq!(day.day, fx.day);
    assert_eq!(
        day.runs, 3,
        "every settled run is counted — the failures cost money too"
    );
    assert_eq!(
        day.solved, 1,
        "only `completed` with `final_answer` answered; `max_steps` and `error` did not"
    );
    assert_eq!(day.cost_micros, 1_000_000);
    assert_eq!(
        day.cost_per_solved(),
        Some(1_000_000),
        "the whole day's cost over the one run that answered"
    );

    fx.dispose().await;
}

/// **A day where every run failed has no cost per solved — and does not read as a free day.**
///
/// The `None` matters more than the number: `Some(0)` would put the worst day on this screen at
/// the bottom of the y-axis as the cheapest day, which is the one inversion a cost chart can make
/// that actively inverts its own meaning.
#[tokio::test]
async fn a_day_that_solved_nothing_reports_no_cost_and_keeps_the_spend_it_made() {
    let fx = stats!();

    fx.seed_run("failed", Some("error"), 400_000, 3).await;
    fx.seed_run("failed", Some("error"), 350_000, 2).await;

    let days = tool_stats::cost_per_solved(&fx.pool, fx.organization, fx.window())
        .await
        .expect("the scatter's source must read");
    let day = &days[0];
    assert_eq!(day.runs, 2);
    assert_eq!(day.solved, 0);
    assert_eq!(day.cost_micros, 750_000, "the spend is real even though nothing was solved");
    assert_eq!(
        day.cost_per_solved(),
        None,
        "a day that bought nothing has no cost per success, not a cost of zero"
    );

    fx.dispose().await;
}

/// **The run-shaped panels are scoped to a tenant, like the tool table.**
///
/// Without this, a silent tenant's `/ai/telemetry` would show another tenant's run shapes — the
/// cross-tenant leak the tool read above already guards, applied to the two panels the screen
/// renders next to it.
#[tokio::test]
async fn the_run_panels_never_show_another_tenants_runs() {
    let fx = stats!();
    fx.seed_run("completed", Some("final_answer"), 100_000, 2).await;

    let other = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(other)
        .bind("other tenant runs")
        .bind(format!("other-runs-{other}"))
        .execute(&fx.pool)
        .await
        .expect("the second tenant must be created");

    let histogram = tool_stats::step_histogram(&fx.pool, other, fx.window())
        .await
        .expect("the histogram must read");
    assert!(
        histogram.is_empty(),
        "tenant B saw tenant A's run shapes: {histogram:?}"
    );

    let scatter = tool_stats::cost_per_solved(&fx.pool, other, fx.window())
        .await
        .expect("the scatter must read");
    assert!(
        scatter.is_empty(),
        "tenant B saw tenant A's cost per solved: {scatter:?}"
    );

    fx.dispose().await;
}

/// **A window really excludes the runs from outside it.**
///
/// The histogram and the scatter both read live `ai_runs` rather than a roll-up, so the window
/// filter is the only thing keeping last month's traffic out of a 7-day chart. The window is
/// narrowed to the fixture's own day, so a reader that ignored `$2`/`$3` would return the same
/// rows and the assertion would pass — the fixture has to be able to *fail* the reader, and the
/// only way to arrange that is to place a run on a day the window excludes.
#[tokio::test]
async fn the_run_panels_exclude_runs_outside_the_window() {
    let fx = stats!();

    // One run on the fixture's day…
    fx.seed_run("completed", Some("final_answer"), 100_000, 2).await;
    // …and one two days later, which the narrow window must not see.
    let narrow = Window {
        from: fx.day,
        to: fx.day,
    };
    let agent = Uuid::new_v4();
    sqlx::query("insert into ai_agents (id, organization_id, name, key) values ($1, $2, $3, $4)")
        .bind(agent)
        .bind(fx.organization)
        .bind(format!("later agent {agent}"))
        .bind(format!("later-{agent}"))
        .execute(&fx.pool)
        .await
        .expect("the later agent must be created");
    sqlx::query(
        "insert into ai_runs (organization_id, agent_id, status, stop_reason, goal, cost_micros, \
         started_at, finished_at) values ($1, $2, 'completed', 'final_answer', $3, 900_000, $4::timestamptz, $4::timestamptz)",
    )
    .bind(fx.organization)
    .bind(agent)
    .bind("a later run")
    .bind(format!("{} 09:00:00+00", fx.day + time::Duration::days(2)))
    .execute(&fx.pool)
    .await
    .expect("the later run must be created");

    let scatter = tool_stats::cost_per_solved(&fx.pool, fx.organization, narrow)
        .await
        .expect("the scatter must read");
    assert_eq!(scatter.len(), 1, "only the in-window day belongs in the chart");
    assert_eq!(scatter[0].cost_micros, 100_000);

    let histogram = tool_stats::step_histogram(&fx.pool, fx.organization, narrow)
        .await
        .expect("the histogram must read");
    assert_eq!(
        histogram.iter().map(|bucket| bucket.runs).sum::<i64>(),
        1,
        "the two-step in-window run and nothing else"
    );

    fx.dispose().await;
}
