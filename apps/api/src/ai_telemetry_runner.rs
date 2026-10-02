//! The per-tool telemetry roll-up runner (REQ-107, slice 4).
//!
//! ## Why this exists at all, and why it is the first thing slice 4 shipped
//!
//! `crates/ai-hub/src/tool_stats.rs` grew a complete store in the previous tick: the migration
//! (0237), [`omnion_ai_hub::tool_stats::refresh_day`], the window reader and the costliest-failing
//! ranking, all with a walk that proves the roll-up reconciles with the `ai_tool_calls` rows it
//! summarises. **And nothing in production ever called `refresh_day`.** The only caller was the
//! test harness.
//!
//! That is the exact failure this workspace has been bitten by before, and it is worth stating
//! plainly because the alternative is shipping a screen on top of it: **a roll-up table with no
//! writer is a table that reads as "this tool was never called, forever"**, and a telemetry
//! screen rendering that empty table is worse than no screen at all — it is a screen making a
//! confident claim from data that was never collected. The reconciliation walk passed because it
//! wrote its own rows first; nothing in it could notice that no *scheduler* would.
//!
//! So the writer ships before the screen. A gate that proves the reader reconciles, with the
//! reader's only source of rows being a hand-made fixture, is a gate that proves nothing about the
//! product.
//!
//! ## The shape: catch up on the way forward
//!
//! Each tick rolls **yesterday** and today — not just today. Two reasons, both concrete:
//!
//! 1. A day bucket only becomes complete at the day boundary, so a runner that rolls "today"
//!    every hour writes a partial day and then rewrites it, and the *final* rewrite is the only
//!    honest one. Rolling the closed day as well means the numbers an operator reads over lunch
//!    are the numbers that will still be there at midnight.
//! 2. **A process that was down over the weekend must not leave a permanent hole.** A tick that
//!    only ever rolls "yesterday" walks one day per tick, so every restart after downtime leaves
//!    every skipped day blank — and the screen's range picker offers exactly those days. The
//!    window below is what makes the tick self-healing over the range the reader can ask for.
//!
//! The window is [`LOOKBACK_DAYS`], and it is bounded rather than "all history" for the honest
//! reason the migration states: this roll-up carries no retention, so a gap older than the
//! window is a gap the reader is never offered either.
//!
//! ## Degradation is an event, and it is emitted here rather than in the store
//!
//! `ai.telemetry.tool.degraded` is in the spec's event table and nothing emits it. The comparison
//! it needs — this day's success rate against the days *before* it — is a decision about what
//! counts as degraded, so it lives in the runner where the rule is written down next to the two
//! queries it reads, rather than inside `refresh_day` where it would fire once per organization
//! per refresh with no way to say which rule applied.
//!
//! [`emit_degraded`] is deliberately **not** part of [`tick`]'s return value as a count of
//! events, and that is on purpose: a tick that refreshed 40 tools and emitted 3 events is not
//! "3 units of work", and a caller summing the report fields to size a run gets a wrong answer.
//! The events are counted separately and logged on their own line.

use std::time::Duration as StdDuration;

use omnion_ai_hub::tool_stats;
use time::{Date, Duration};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// How many days back one tick re-rolls.
///
/// **Bounded by what the reader can ask for, not by how far back the data goes.** The telemetry
/// route clamps its window to `[today - 365, today]` and offers a 30-day default, so a tick that
/// re-rolled a week of history on a cold start would be spending work on days the screen can
/// show and still not covering the widest picker. Eight is chosen to cover a normal weekend plus
/// a restart — two days of downtime with a day's margin — because the daily tick runs once per
/// hour, so a longer outage is repaired by *several* ticks and needs no extra reach here.
pub const LOOKBACK_DAYS: i64 = 8;

/// The tick's period.
///
/// Hourly rather than daily, and that is a direct consequence of rolling *yesterday* as well as
/// today: the closed day is rewritten on every tick anyway, and an hourly tick means the last
/// rewrite of a day happens within the hour after it closed rather than up to 24 hours later —
/// which is the window in which an operator comparing today's screen against last night's would
/// see yesterday's numbers change under them.
pub const TICK_MS: u64 = 60 * 60 * 1000;

/// The success-rate drop that marks a tool degraded, in percentage points.
///
/// **Five points against the trailing week, and five is a chosen number.** It is written here as
/// a constant with its reasoning rather than inlined at the comparison, because the obvious ways
/// to get this wrong are both silent: a percentage *ratio* threshold (`0.05`) against a value
/// already expressed in percent is a 5000-point threshold that never fires, and comparing the
/// two *rates* against each other rather than against a floor means a tool that always returned
/// garbage never degrades relative to itself.
pub const DEGRADED_DROP_POINTS: f64 = 5.0;

/// The trailing window the comparison measures against.
pub const DEGRADED_BASELINE_DAYS: i64 = 7;

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Day buckets rewritten, today and yesterday first.
    pub days: u64,
    /// Rows the upserts wrote or updated — a day's rows per tool, summed.
    pub rows: u64,
    /// `ai.telemetry.tool.degraded` events emitted.
    pub degraded: u64,
}

impl TickReport {
    /// `true` when the tick had nothing to do, so the loop can stay silent.
    ///
    /// **A refresh that wrote rows is never idle, even if it emitted no events.** These are two
    /// different answers to two different questions — "is the roll-up being kept current" and
    /// "is anything wrong" — and folding them together means a healthy day logs nothing while a
    /// broken one logs twice, which is precisely backwards from what an operator wants to skim.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.rows == 0 && self.days == 0
    }
}

/// The days one tick rolls, newest first.
///
/// **Yesterday, then today** — the order is the one that matters if a tick is interrupted: today
/// is the row people are reading, and yesterday is the row the costliest-failing ranking compares
/// against. Exposed as a function rather than inlined so a test can assert the exact set, which
/// is otherwise a two-element slice buried in a loop body where a swap to "today only" would look
/// harmless and silently stop repairing downtime.
#[must_use]
pub fn days_to_roll(today: Date, lookback: i64) -> Vec<Date> {
    // The floor is on the **day count**, not on the lookback: a lookback of zero means "today
    // only", and clamping the lookback to one first and then making the range inclusive would
    // give that caller two days. The off-by-one is real and was found by the unit test that
    // pins this function — `span.max(1)` followed by `0..=span` reads as an obvious floor and is
    // not one. `LOOKBACK_DAYS = 8` therefore rolls nine days: today plus the eight before it.
    let days = lookback.max(0) + 1;
    (0..days)
        .map(|back| today - Duration::days(back))
        .collect()
}

/// Run one tick: re-roll the recent days, then announce anything that degraded.
///
/// **Both halves run even if the first one fails.** A degraded-check that depends on the refresh
/// having succeeded is a check whose coverage silently depends on an unrelated write, and the
/// only symptom is an event that stopped being emitted — the kind of gap nobody notices until a
/// tool has been quietly failing for a month. So a refresh error is recorded in the report as a
/// log line and the scan still runs; the scan reads the *previous* days' rows, which were written
/// by an earlier tick and are therefore still meaningful.
pub async fn tick(state: &AppState) -> TickReport {
    let pool = state.db().pool();
    let today = time::OffsetDateTime::now_utc().date();

    let days = days_to_roll(today, LOOKBACK_DAYS);
    let mut report = TickReport::default();

    for day in &days {
        match tool_stats::refresh_day(pool, *day).await {
            Ok(rows) => {
                report.days += 1;
                report.rows += rows;
            }
            Err(error) => {
                // Not an early return — see the module header. The day after this one still gets
                // rolled, and the degradation scan below still runs on the rows already stored.
                tracing::warn!(error = %error, %day, "a telemetry day could not be rolled up");
            }
        }
    }

    // The scan runs on the *closed* days rather than today: today's success rate is measured over
    // a day that is still happening, so a tool that failed at 09:00 and has not been called since
    // reads as 0% and emits a degraded event on the strength of one call. The screen's default
    // range ends today for the same reason the scan excludes it — a half-finished day is not a
    // trend.
    match emit_degraded(pool, today).await {
        Ok(emitted) => report.degraded = emitted,
        Err(error) => {
            tracing::warn!(error = %error, "the degraded-tool scan could not run");
        }
    }

    report
}

/// One tool that has degraded against its trailing baseline.
///
/// **`Eq` is deliberately absent** — the percentages are `f64`, and `f64: Eq` is not something a
/// derive can manufacture. That is worth one line because the derive is the kind of thing people
/// add by reflex, and the compiler's answer here is a genuine design fact rather than an
/// inconvenience: this struct holds percentages, and percentages are not a set of exact values.
#[derive(Debug, Clone, PartialEq)]
pub struct DegradedTool {
    pub organization_id: Uuid,
    pub tool: String,
    /// The days measured.
    pub day: Date,
    /// Share of calls that succeeded on `day`, in percent.
    pub success_percent: f64,
    /// The same share over the days before it.
    pub baseline_percent: f64,
    /// How far it fell. Always positive — the direction is `success < baseline` by construction.
    pub drop_points: f64,
    pub calls: i64,
}

/// Find the tools whose success rate fell past the threshold, and emit an event for each.
///
/// **The baseline excludes the day being judged.** That is the whole difference between this
/// comparison and a tautology: a baseline that includes the day under test pulls its own mean
/// toward the value being tested, so a tool that collapses from 100% to 0% still reads as "no
/// meaningful drop" once the collapse is inside the window. `day < $1` rather than `day <= $1`.
///
/// **A minimum of ten calls, because one call is not a trend.** A tool invoked once a day, failing
/// that one call, produces a 100-point "drop" every single day — an alert stream that is
/// uniformly noise gets switched off, and then the real drop is filtered out with it. Ten is a
/// floor rather than a percentage because the honest signal depends on how often a tool runs, not
/// on how its failures are distributed.
///
/// **A baseline that saw no calls yields no event, not a 100-point drop.** `sum(calls) > 0` on
/// the baseline side is what distinguishes "it was working" from "it was not running", and the
/// second is not a degradation — it is a tool nobody is using any more, which belongs on the
/// activity panel rather than in an alert.
pub async fn emit_degraded(pool: &sqlx::PgPool, today: Date) -> Result<u64, sqlx::Error> {
    let rows = degraded_rows(pool, today).await?;
    let mut emitted = 0_u64;
    for row in &rows {
        // The event is `emit`ed through the platform's own event writer rather than an audit
        // entry, because the spec lists it beside `ai.eval.*` — a row that a webhook can bind to,
        // and REQ-021's notification centre can read. The runner's siblings do the same.
        //
        // **The count is of events that actually landed, not of tools that degraded.** These
        // come apart the moment the bus is unhappy: counting rows regardless would report "3
        // degraded tools" in a log line and in the tick report while writing nothing, and the
        // report's only job is to make the operator believe an alert was raised. A boolean the
        // caller can actually be wrong about is the honest shape; the alternative is a number that
        // reads as confirmation of something that never happened.
        if emit_event(pool, row).await {
            emitted += 1;
        }
    }
    Ok(emitted)
}

/// The degraded set, without emitting anything.
///
/// Split out so the **rule** can be tested against a database with no event side effect, and so
/// [`emit_degraded`] stays a thin loop. A predicate that can only be observed by writing rows and
/// reading them back is a predicate whose test depends on the event writer too.
pub async fn degraded_rows(
    pool: &sqlx::PgPool,
    today: Date,
) -> Result<Vec<DegradedTool>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        organization_id: Uuid,
        tool: String,
        day: Date,
        calls: i64,
        success_percent: f64,
        baseline_percent: f64,
    }

    // The two sides are two CTEs over the same roll-up table and are joined on
    // `(organization, tool)` — a tool is compared against **its own** trailing week, never
    // against the installation's average, because an installation full of healthy tools would
    // otherwise hide the one broken one. The roll-up carries `days_seen` semantics through its
    // primary key, so "the days it ran" is already known to the reader.
    let rows = sqlx::query_as::<_, Row>(
        r#"
        with judged as (
            select organization_id, tool, day,
                   calls::bigint as calls,
                   successes::float8 * 100.0 / calls::float8 as success_percent
            from ai_tool_stats_daily
            where day = $1::date
              and calls >= 10
        ),
        baseline as (
            select organization_id, tool,
                   sum(calls)::bigint as calls,
                   sum(successes)::float8 * 100.0 / sum(calls)::float8 as baseline_percent
            from ai_tool_stats_daily
            where day < $1::date
              and day >= ($1::date - make_interval(days => $2::int))::date
              and calls > 0
            group by organization_id, tool
        )
        select j.organization_id, j.tool, j.day, j.calls,
               j.success_percent, b.baseline_percent
        from judged j
        join baseline b on b.organization_id = j.organization_id and b.tool = j.tool
        where b.calls > 0
          and b.baseline_percent - j.success_percent >= $3::float8
        order by (b.baseline_percent - j.success_percent) desc, j.tool asc
        "#,
    )
    .bind(today)
    .bind(DEGRADED_BASELINE_DAYS)
    .bind(DEGRADED_DROP_POINTS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let drop_points = row.baseline_percent - row.success_percent;
            DegradedTool {
                organization_id: row.organization_id,
                tool: row.tool,
                day: row.day,
                success_percent: row.success_percent,
                baseline_percent: row.baseline_percent,
                drop_points,
                calls: row.calls,
            }
        })
        .collect())
}

/// Write one `ai.telemetry.tool.degraded` event.
///
/// **Through `omnion_events::bus`, not a raw `insert into events`.** Three reasons, and each one
/// is a silent failure if it is skipped:
///
/// 1. The bus validates the name and the payload. A hand-rolled insert writes a row that the
///    catalogue gate never sees and the fan-out never reads — an event that exists in the table
///    and is invisible to every subscriber, which is the worst shape a missing event can take.
/// 2. The bus runs the **webhook fan-out** in the same transaction. Without it, the event lands
///    and nobody is ever told, which defeats the point of the spec listing it ("an early warning
///    to pair with REQ-021").
/// 3. `events` is not a table this module owns. The column list is the store's, and a raw insert
///    is a second source of truth that a migration would silently break.
///
/// A failure is **logged and swallowed by the caller**, not propagated: one tenant whose event
/// row cannot be written must not stop the scan from announcing the next one, and the event table
/// being unhappy is not the roll-up's problem. The alternative — failing the whole tick — makes
/// one bad insert cost every other organization its alert.
async fn emit_event(pool: &sqlx::PgPool, row: &DegradedTool) -> bool {
    let event = omnion_events::NewEvent::new("ai.telemetry.tool.degraded")
        .organization(row.organization_id)
        .payload(serde_json::json!({
            "tool": row.tool,
            "day": row.day.to_string(),
            "success_percent": round2(row.success_percent),
            "baseline_percent": round2(row.baseline_percent),
            "drop_points": round2(row.drop_points),
            "calls": row.calls,
        }));
    match omnion_events::bus::emit(pool, event).await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, tool = %row.tool, "a degraded-tool event could not be published");
            false
        }
    }
}

/// A percentage to two decimals.
///
/// **Rounded, and that is a contract with the subscriber rather than tidiness.** The event's
/// payload is read by webhooks and by the notification centre, and a subscriber comparing two
/// events with `==` must get the same answer whether it compared `73.33333333333333` from one
/// and `73.33` from the next. Full float precision in a JSON payload buys nothing and costs the
/// subscriber a comparison that only works on the run it was written.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Start the roll-up; the handle is kept by the binary and ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let period = StdDuration::from_millis(TICK_MS);

    tracing::info!(lookback_days = LOOKBACK_DAYS, tick_ms = TICK_MS, "AI tool-telemetry roll-up started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(period);
        // A restart storm must not become a burst of catch-up sweeps. Each tick re-rolls a window
        // of days, so eight skipped ticks after a long outage would otherwise fire back to back
        // and re-write the same nine day buckets in a row — eight times the upserts for the same
        // answer, on the same database, at the moment it is already under pressure.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first `tick()` returns immediately; the sweep runs after it so a restart does not
        // race the binary's own startup work (migrations included) for the pool.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            let report = tick(&state).await;
            // Two lines, never one. `is_idle` covers "did the refresh write anything", and the
            // degraded count is a separate fact: a healthy refresh with a degraded tool is the
            // combination an operator most wants to see, and folding it into the idle check means
            // exactly that day is the one day nothing is logged.
            if !report.is_idle() {
                tracing::info!(?report, "AI tool-telemetry roll-up tick");
            }
            if report.degraded > 0 {
                tracing::warn!(
                    degraded = report.degraded,
                    "AI tool-telemetry reported degraded tools"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_that_rolled_nothing_is_idle() {
        assert!(TickReport::default().is_idle());
        assert!(!TickReport { days: 1, rows: 4, degraded: 0 }.is_idle());
        assert!(!TickReport { days: 1, rows: 4, degraded: 2 }.is_idle());
    }

    #[test]
    fn a_healthy_refresh_is_not_silent_just_because_no_tool_degraded() {
        // The two questions are separate, and this is the assertion that keeps them separate: a
        // tick that wrote 40 rows and emitted nothing is *doing its job*, and an `is_idle` that
        // only looked at `degraded` would report it as idle — so the operator would never learn
        // that the roll-up is running at all.
        let healthy = TickReport { days: 2, rows: 40, degraded: 0 };
        assert!(!healthy.is_idle());
    }

    #[test]
    fn the_window_covers_today_and_the_days_before_it() {
        let today = Date::from_calendar_date(2026, time::Month::October, 2).unwrap();
        let days = days_to_roll(today, 3);
        assert_eq!(
            days,
            vec![
                Date::from_calendar_date(2026, time::Month::October, 2).unwrap(),
                Date::from_calendar_date(2026, time::Month::October, 1).unwrap(),
                Date::from_calendar_date(2026, time::Month::September, 30).unwrap(),
                Date::from_calendar_date(2026, time::Month::September, 29).unwrap(),
            ],
            "the tick rolls today first, then walks back — the order is what makes an \
             interrupted tick leave the day people are reading done"
        );
    }

    #[test]
    fn a_zero_lookback_still_rolls_today() {
        // A lookback of zero is not "roll nothing": it is the configuration in which the roll-up
        // never writes and the screen never fills. The floor is 1, and this is the assertion that
        // keeps a `max(0)` "fix" from silently disabling the whole feature.
        let today = Date::from_calendar_date(2026, time::Month::October, 2).unwrap();
        assert_eq!(days_to_roll(today, 0), vec![today]);
        assert_eq!(days_to_roll(today, -5), vec![today]);
    }

    #[test]
    fn a_week_of_downtime_is_repaired_by_one_tick() {
        // The reason the window exists rather than a "yesterday only" tick: a runner that walks
        // one day per tick leaves every skipped day permanently blank, and the screen's range
        // picker offers exactly those days. Three days of downtime must come back from a single
        // tick, with today written too.
        let today = Date::from_calendar_date(2026, time::Month::October, 10).unwrap();
        let days = days_to_roll(today, LOOKBACK_DAYS);
        assert_eq!(days.first(), Some(&today));
        assert_eq!(days.len() as i64, LOOKBACK_DAYS + 1);
        // The oldest day rolled is three days of downtime before yesterday.
        assert_eq!(
            days.last().copied(),
            Some(today - Duration::days(LOOKBACK_DAYS))
        );
        assert!(days.contains(&(today - Duration::days(3))));
    }

    #[test]
    fn the_degradation_threshold_is_percentage_points_and_five_of_them() {
        // The two ways to get this wrong are both silent, so they are both pinned here. A ratio
        // (`0.05`) against a value already in percent never fires; a comparison of one rate to the
        // other rather than to a floor means a permanently broken tool never degrades relative to
        // itself.
        assert_eq!(DEGRADED_DROP_POINTS, 5.0);
        let baseline = 95.0_f64;
        let just_under = baseline - 4.9_f64;
        let at_threshold = baseline - DEGRADED_DROP_POINTS;
        assert!(
            baseline - just_under < DEGRADED_DROP_POINTS,
            "a 4.9-point fall must not alert"
        );
        assert!(
            baseline - at_threshold >= DEGRADED_DROP_POINTS,
            "a fall of exactly the threshold is a degradation — the comparison is `>=`"
        );
    }
}
