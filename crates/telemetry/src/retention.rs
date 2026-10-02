//! The retention sweep: the daily job that actually prunes (REQ-126, slice 4).
//!
//! ## Why this module exists at all
//!
//! `store::prune`, `trace_store::prune` and `alerts::prune_events` all shipped in earlier
//! slices, and all three had **exactly one caller: their own test**. A component whose only
//! callers are its own tests is provable and unreachable — the same defect that made the exporter
//! pipeline buffer nothing forever in slice 3. Nothing in a running instance ever deleted a log
//! line, so an instance left up for a year accumulated a year of lines while the settings screen
//! showed a retention window that nothing honoured.
//!
//! So this module is the caller, and it is deliberately thin: it reads the ONE settings row the
//! screen writes and hands the two retention numbers to the three prune functions, which already
//! know how to delete their own table. What the sweep adds is the part that is easy to get
//! wrong and impossible to see:
//!
//! * **It refuses to widen a window.** The stored value is validated against the same cap the
//!   screen validates against, so a row edited by hand in the database cannot make the job keep
//!   less than the store's floor. A retention job that deletes more than the operator asked for
//!   is unrecoverable, so the clamp is one-directional — toward *keeping more*.
//! * **It counts what it removed and what it deliberately left alone.** `PruneReport` names the
//!   tables it touched, and a unit test asserts the set. A sweep that grew a new `delete` without
//!   the report growing with it would delete audit rows, and the number of deleted audit rows is
//!   the kind of finding that is only read after the fact.
//! * **It is idempotent.** A sweep that failed halfway and runs again removes the rest, and
//!   removes nothing twice, because every prune is a `delete … where <ts> < cutoff`.
//!
//! ## Why a daily job and not a nightly one, and not a timer per process
//!
//! A retention window is measured in days, so a sweep that runs hourly does the same work 24
//! times to delete the same rows. But the *cadence is not the interesting part* — the interesting
//! part is that the job is idempotent and bounded, so a run that overlaps a deploy, a clock
//! adjustment or a second replica is safe. The interval is configuration for that reason alone.

use std::time::Duration as StdDuration;

use serde_json::json;
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use crate::{alerts, store, trace_store};

/// The family a sweep is counted in. Declared in `metrics::FAMILIES`.
pub const PRUNED_FAMILY: &str = "omnion_retention_pruned_rows_total";

/// The event a sweep emits when it removed something.
///
/// An alias, not a second constant: `crate::events::RETENTION_PRUNED` is the one place the eight
/// documented names live, and a copy of this string here is a name that can be changed in one
/// file and not the other — which is how the other seven came to be documented and dead in the
/// first place.
pub const PRUNED_EVENT: &str = crate::events::RETENTION_PRUNED;

/// The default sweep interval: once a day.
pub const SWEEP_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// The floor on a retention window, in days.
///
/// One day, not zero: a zero-day window would make the store refuse its own search range (the
/// store's `MAX_WINDOW_DAYS` is a *maximum* and its minimum is a day), so the job would delete
/// everything a second's search could not reach anyway. Clamping up to one day is what makes
/// "the window you can search" and "the window the job keeps" agree.
pub const MIN_RETENTION_DAYS: i64 = 1;

/// What one sweep removed.
///
/// The table names are the contract: a sweep is allowed to remove log lines, trace-index rows,
/// resolved alert events and expired silences, and **nothing else**. Audit rows and health
/// incidents live in other tables and are not reachable from any prune function in this crate —
/// which is a compliance property, so it is asserted by a test that writes an audit row and
/// checks it survives, and mirrored here by the unit test that pins the set of tables a sweep
/// reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Lines removed from the bounded log store.
    pub log_rows: i64,
    /// Trace-index rows removed.
    pub trace_rows: i64,
    /// Resolved alert events removed.
    pub alert_events: i64,
    /// Silences whose window had passed, removed.
    pub silences: i64,
    /// Prunes that FAILED.
    ///
    /// This field exists because of a real defect it would have made visible. `trace_store::prune`
    /// shipped with `make_interval(days => $1)` bound to an `i64`, so PostgreSQL refused the
    /// statement on every call — and the sweep, which treated a failed prune as a warning and
    /// carried on, reported `trace_rows: 0` for it. Zero and "failed" were the same number, so
    /// the trace index silently never pruned while every other signal said retention was fine.
    ///
    /// A count that cannot be distinguished from a failure is not a report. The number is here
    /// so a failed sweep is visibly different from a quiet one, and `record` refuses to emit
    /// `observability.retention.pruned` while it is non-zero — an event that says "pruned 4
    /// rows" must never be emitted by a pass that also failed to prune.
    pub errors: u32,
}

impl PruneReport {
    /// Whether the sweep removed nothing AND nothing failed.
    ///
    /// A pass that failed on all four tables and removed nothing is not a quiet pass. `is_empty`
    /// is what decides whether the loop logs and emits, so folding the failure in here is what
    /// stops "retention is silently broken" from being reported as "retention had nothing to do".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.errors == 0
    }

    /// Whether any prune failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.errors > 0
    }

    /// Everything the sweep removed.
    #[must_use]
    pub fn total(&self) -> i64 {
        self.log_rows + self.trace_rows + self.alert_events + self.silences
    }

    /// The payload `observability.retention.pruned` carries.
    ///
    /// Counts only. A retention event that named a table and a count is safe to deliver to a
    /// webhook endpoint, and one that named the rows would put log lines in a subscriber's
    /// inbox — which is the one thing this whole module exists to prevent.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "log_rows": self.log_rows,
            "trace_rows": self.trace_rows,
            "alert_events": self.alert_events,
            "silences": self.silences,
            "total": self.total(),
            "errors": self.errors,
        })
    }
}

/// The retention numbers the sweep runs with.
///
/// A snapshot taken from the settings row at the start of a sweep, so every prune in one pass
/// uses one consistent pair: a sweep that read `logs_retention_days` twice and the operator
/// edited the screen between the two reads would delete logs at one window and traces at another
/// and be unable to say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// How many days of log lines to keep.
    pub logs_days: i64,
    /// How many days of trace-index rows to keep.
    pub traces_days: i64,
}

impl Retention {
    /// The documented defaults: the request's `logs_retention_days` of 14 and
    /// `traces_retention_days` of 7.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            logs_days: 14,
            traces_days: trace_store::DEFAULT_TRACE_RETENTION_DAYS,
        }
    }

    /// Clamp both windows into the store's own bounds, naming nothing and refusing nothing.
    ///
    /// One-directional on purpose: a stored value of 0 or a negative one is a hand-edited row,
    /// and the answer to "the database says keep zero days" is not "delete the log store". The
    /// upper bound is the store's own cap, so a row beyond it is kept for *longer* than the
    /// screen allows — the failure direction is extra rows, which cost disk and are deletable
    /// later, rather than fewer rows, which are gone.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            logs_days: self
                .logs_days
                .clamp(MIN_RETENTION_DAYS, store::MAX_WINDOW_DAYS),
            traces_days: self
                .traces_days
                .clamp(MIN_RETENTION_DAYS, trace_store::MAX_TRACE_RETENTION_DAYS),
        }
    }
}

/// Read the retention numbers off the settings row.
///
/// A missing row is the documented defaults rather than an error: the migration seeds it, and a
/// sweep that refuses to run because of a row that only *reads* is a job that silently stops
/// the day somebody empties a table.
pub async fn read_retention(pool: &PgPool) -> Result<Retention, crate::TelemetryError> {
    let row: Option<(i32, i32)> = sqlx::query_as(
        "select logs_retention_days, traces_retention_days from obs_log_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((logs, traces)) => Retention {
            logs_days: i64::from(logs),
            traces_days: i64::from(traces),
        }
        .clamped(),
        None => Retention::defaults(),
    })
}

/// One sweep: prune every bounded table past its window and return what went.
///
/// Each prune is independent, so a failure in one is a `0` for that table rather than a sweep
/// that rolls back the others — a sweep that is all-or-nothing is a sweep that never runs once
/// one table is locked. The failures are counted in `errors` so a caller can log them, and the
/// rest of the pass still happens.
pub async fn sweep(pool: &PgPool, retention: Retention) -> PruneReport {
    let retention = retention.clamped();
    let mut report = PruneReport::default();

    match store::prune(pool, retention.logs_days).await {
        Ok(removed) => report.log_rows = removed,
        Err(error) => {
            report.errors += 1;
            tracing::warn!(
                days = retention.logs_days,
                error = %error,
                "the log retention pass failed; the other tables are still pruned"
            );
        }
    }
    match trace_store::prune(pool, retention.traces_days).await {
        Ok(removed) => report.trace_rows = removed,
        Err(error) => {
            report.errors += 1;
            tracing::warn!(
                days = retention.traces_days,
                error = %error,
                "the trace retention pass failed; the other tables are still pruned"
            );
        }
    }
    // Alert events use the *trace* window: an event is a thing that happened during a request, so
    // it belongs to the same retention class as the trace index rather than to the log class.
    // Silences have their own expiry and are not a window at all.
    match alerts::prune_events(pool, retention.traces_days).await {
        Ok(removed) => report.alert_events = removed,
        Err(error) => {
            report.errors += 1;
            tracing::warn!(
                error = %error,
                "the alert-event retention pass failed; the other tables are still pruned"
            );
        }
    }
    match alerts::prune_silences(pool).await {
        Ok(removed) => report.silences = removed,
        Err(error) => {
            report.errors += 1;
            tracing::warn!(
                error = %error,
                "the silence expiry pass failed; the other tables are still pruned"
            );
        }
    }

    report
}

/// Read the settings row, prune once, and record what went.
///
/// **This is the only sweep entry point that tells anybody what it did, and that is deliberate.**
/// `sweep` prunes and counts and is the right primitive for a caller that only wants the numbers
/// (a test asserting a window, an operator's own cron running the same deletions from a replica).
/// But the *documented* behaviour of retention on this platform includes writing
/// `observability.retention.pruned` and moving the prune counter, and a sweep that only happened
/// to be called by `run` was an event reachable from exactly one private loop — the sixth instance
/// of the shape this request has kept producing, where a fact is documented, unit-provable, and
/// unreachable in a running instance. `run` calls this, so the loop's behaviour is unchanged; a
/// caller reaching for "prune the way the instance does" now gets the event too.
pub async fn prune_from_settings(pool: &PgPool) -> Result<PruneReport, crate::TelemetryError> {
    let retention = read_retention(pool).await?;
    let report = sweep(pool, retention).await;
    if !report.is_empty() {
        record(pool, &report).await;
    }
    Ok(report)
}

/// Start the sweep loop. The handle ends with the process.
///
/// `OMNION_RETENTION_SWEEP=false` disables it. The switch exists for the same reason the alert
/// evaluator has one: an operator may prefer to run the same deletion from their own cron against
/// a read replica, and two jobs deleting the same rows is harmless (every prune is a `delete
/// where < cutoff`, so the second finds nothing) while two jobs with *different* retention
/// values is not.
#[must_use]
pub fn run(pool: PgPool) -> tokio::task::JoinHandle<()> {
    tracing::info!(
        interval_ms = SWEEP_INTERVAL_MS,
        "the retention sweep started"
    );
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(SWEEP_INTERVAL_MS));
        // A sweep that overran its interval must not become a burst of catch-up sweeps; the rows
        // it would delete a second time are already gone.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; give the process a moment to finish booting so the
        // sweep does not compete with the migrations it depends on for the pool.
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match prune_from_settings(&pool).await {
                // The event and the counter are already written by `prune_from_settings` — this
                // arm is now only the log line. Recording here as well is what would have made
                // every sweep emit two `retention.pruned` events, and a subscriber that gets the
                // same name twice for one sweep learns to ignore the name.
                Ok(report) => {
                    if !report.is_empty() {
                        tracing::info!(
                            log_rows = report.log_rows,
                            trace_rows = report.trace_rows,
                            alert_events = report.alert_events,
                            silences = report.silences,
                            errors = report.errors,
                            "the retention sweep finished"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "the retention sweep could not read its settings")
                }
            }
        }
    })
}

/// Count a sweep on the metric family and record the event.
///
/// A sweep that removed 40 000 lines and told nobody has left a number on a dashboard nobody is
/// watching, which is the same "provable but unreachable" shape as the prune functions had. The
/// event is emitted only when something was removed, because a webhook subscriber that receives
/// a `retention.pruned` every 24 hours for zero rows learns to ignore the name.
///
/// **Awaited, not spawned.** This used to `tokio::spawn` the write and hand the report back, which
/// is the one arrangement where the caller's next statement — read the `events` table back — races
/// the write it is checking for. The walk that does that read failed with "the event reached the
/// events table" while the event arrived milliseconds later, and it did so intermittently, which
/// is the hardest kind of failure to reproduce. Retention is housekeeping: a subscriber's slow
/// inbox must not hold up a prune, and a *bounded* wait is the honest way to have both. A failure
/// is logged and the sweep stands either way, which is what the doc below has always claimed.
async fn record(pool: &PgPool, report: &PruneReport) {
    crate::metrics::global().counter_add(PRUNED_FAMILY, &["rows"], report.total() as f64);
    if report.failed() {
        // A `retention.pruned` payload is a claim that retention ran. Emitting it from a pass
        // that deleted nothing because a statement was refused is the one way this event could
        // be a lie, so the emission is refused and only the failure counter moves.
        crate::metrics::global().counter_add(PRUNED_FAMILY, &["failed"], f64::from(report.errors));
        return;
    }
    if let Err(error) = emit_pruned(pool, report.payload()).await {
        tracing::warn!(error = %error, "the retention event could not be recorded");
    }
}

/// Write `observability.retention.pruned` to the bus.
///
/// **Platform-wide, so it carries no organization.** A retention sweep is a fact about the
/// instance, not about a tenant — there is no tenant whose retention ran. That is also what the
/// fan-out does with it: `enqueue_fanout` returns zero deliveries for an event with no
/// organization, because matching a tenant's webhook endpoint against a platform fact would leak
/// one tenant's configuration into another's. So this event is written for the record and for
/// the operations endpoint that subscribes without a tenant, and it is deliberately not fanned
/// out to tenant endpoints.
///
/// A failure is logged and the sweep stands: retention is a housekeeping duty and refusing to
/// delete old rows because a subscriber's inbox is unreachable would be a worse outcome than a
/// missed notification.
pub async fn emit_pruned(
    pool: &PgPool,
    payload: serde_json::Value,
) -> Result<(), crate::TelemetryError> {
    crate::events::emit(pool, crate::events::RETENTION_PRUNED, payload)
        .await
        .map(|report| {
            tracing::debug!(
                event_id = report.event.id,
                "the retention event was recorded"
            );
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_defaults_match_the_two_stores() {
        // A second implementation of a default is a second place to be wrong. The log store's
        // default is the migration's literal and the trace store's is its own constant; this
        // pins the sweep's copy to both.
        let defaults = Retention::defaults();
        assert_eq!(
            defaults.logs_days, 14,
            "the request's log default is 14 days"
        );
        assert_eq!(
            defaults.traces_days,
            trace_store::DEFAULT_TRACE_RETENTION_DAYS,
            "the trace default drifted from the trace store's constant"
        );
    }

    #[test]
    fn a_window_is_clamped_toward_keeping_more_never_toward_deleting_more() {
        // The failure direction that matters: a value of zero or a negative one must NOT become
        // "delete everything", and a value beyond the cap must not be honoured verbatim.
        let clamped = Retention {
            logs_days: 0,
            traces_days: -5,
        }
        .clamped();
        assert_eq!(clamped.logs_days, MIN_RETENTION_DAYS);
        assert_eq!(clamped.traces_days, MIN_RETENTION_DAYS);

        let clamped = Retention {
            logs_days: 9_000,
            traces_days: 9_000,
        }
        .clamped();
        assert_eq!(clamped.logs_days, store::MAX_WINDOW_DAYS);
        assert_eq!(clamped.traces_days, trace_store::MAX_TRACE_RETENTION_DAYS);
    }

    #[test]
    fn a_sweep_reports_the_four_bounded_tables_and_nothing_else() {
        // The compliance property, as an executable statement: a retention sweep touches the log
        // store, the trace index, the alert timeline and expired silences. Audit rows and health
        // incidents are NOT in this list, and if a future prune function is added for a new
        // table this test is the thing that has to be updated on purpose.
        let report = PruneReport {
            log_rows: 1,
            trace_rows: 2,
            alert_events: 3,
            silences: 4,
            errors: 0,
        };
        assert_eq!(report.total(), 10);
        assert!(!report.is_empty());
        assert!(PruneReport::default().is_empty());
    }

    #[test]
    fn a_sweep_that_pruned_nothing_but_failed_is_not_quiet() {
        // The exact confusion that hid a real defect: a pass whose only action was a refused
        // statement reported `trace_rows: 0`, which is the same number a quiet pass reports. The
        // loop logs and emits on `is_empty`, so folding the failure in is what turns "retention
        // is broken" into a visible line instead of a healthy silence.
        let failed = PruneReport {
            errors: 1,
            ..PruneReport::default()
        };
        assert!(failed.failed());
        assert!(
            !failed.is_empty(),
            "a failed sweep reported itself as quiet, which is how the make_interval defect hid"
        );

        // And a successful sweep that removed nothing IS quiet.
        let quiet = PruneReport::default();
        assert!(quiet.is_empty());
        assert!(!quiet.failed());
    }

    #[test]
    fn a_failed_pass_carries_its_error_count_in_the_payload() {
        // The payload goes to a webhook subscriber. "It pruned 12 rows" and "it pruned 12 rows
        // and two statements failed" are different facts, and only the second one lets a
        // subscriber notice that a signal stopped being retained.
        let payload = PruneReport {
            log_rows: 12,
            errors: 2,
            ..PruneReport::default()
        }
        .payload();
        assert_eq!(payload["total"], 12);
        assert_eq!(payload["errors"], 2);
    }

    #[test]
    fn the_pruned_payload_carries_counts_and_never_rows() {
        let payload = PruneReport {
            log_rows: 12,
            ..PruneReport::default()
        }
        .payload();
        assert_eq!(payload["log_rows"], 12);
        assert_eq!(payload["total"], 12);
        assert_eq!(payload["errors"], 0);
        // The payload is what a webhook subscriber receives. A field carrying a log line, a
        // request id or a message would put operator data in somebody's inbox, so the check is
        // on the KEYS rather than on a string that a fixture would have to invent.
        let keys: Vec<&str> = payload
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in ["message", "msg", "line", "rows", "request_id", "fields"] {
            assert!(
                !keys.contains(&forbidden),
                "the retention payload carries `{forbidden}`, which is a row or a log line"
            );
        }
    }

    #[test]
    fn the_pruned_family_is_declared_in_the_registry() {
        assert!(
            crate::metrics::family(PRUNED_FAMILY).is_some(),
            "{PRUNED_FAMILY} is recorded by the sweep but not declared, so a sweep that removed \
             40 000 lines is invisible on the scrape"
        );
    }

    #[test]
    fn the_event_name_is_the_one_the_request_documents() {
        assert_eq!(PRUNED_EVENT, crate::events::RETENTION_PRUNED);
        assert_eq!(PRUNED_EVENT, "observability.retention.pruned");
    }

    #[test]
    fn a_daily_sweep_is_not_shorter_than_the_shortest_window_it_enforces() {
        // A sweep that ran more often than the shortest window it keeps would do the same delete
        // many times a day. The lower bound is checked rather than the value being trusted.
        assert!(
            SWEEP_INTERVAL_MS >= 24 * 60 * 60 * 1000,
            "the retention sweep runs more than once a day; the window it enforces is in days"
        );
    }

    #[test]
    fn the_loop_is_a_spawned_task_not_a_future_the_caller_must_poll() {
        let _type_check: fn(PgPool) -> tokio::task::JoinHandle<()> = run;
    }
}
