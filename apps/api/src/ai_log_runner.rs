//! The background route-decision pruner.
//!
//! The health probe runner (`ai_health_runner`) already prunes its own samples and usage rows in
//! the tick that writes them, so this follows the same shape rather than inventing a second
//! timer: one spawn call, one tick, and the same "the tick is a plain function so it can be
//! tested without a clock" property.
//!
//! # Why this does not prune the usage counters
//!
//! The decision log and the cost counters answer different questions and have different lives. A
//! decision is a *diagnostic*: within a few weeks its walk describes a routing map that no longer
//! exists, and keeping it costs a row on the hot path of every AI request. A cost counter is an
//! *accounting* number: the month it belongs to is still being reported on long after its
//! decision is gone, and a counter that vanished with its explanation is a chart that quietly
//! went flat. The spec is explicit ("the counters the panel shows for older windows come from
//! `ai_usage`, which this request never prunes"), and the test asserts the split by pruning and
//! then counting the usage rows that were never touched.

use std::time::Duration as StdDuration;

use omnion_ai_hub::decision_store::RETENTION_DAYS;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Decision rows dropped.
    pub pruned: u64,
}

impl TickReport {
    /// `true` when the tick had nothing to do, so the loop can stay silent.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.pruned == 0
    }
}

/// Run one tick: drop the decisions that fell out of the retention window.
///
/// Exposed separately from [`spawn`] so the tick can be exercised against a real database
/// without starting a timer — the same split the health runner uses, for the same reason: a
/// pruner that can only be tested by waiting 90 days is a pruner that ships untested.
pub async fn tick(state: &AppState) -> TickReport {
    // The pruner calls the **store**, not the route module. A background task that goes through
    // an `ApiError` return type has already lost: `ApiError` deliberately does not implement
    // `Display` (its message is an HTTP body, not a log line), so a runner reporting it has
    // nothing to print and the failure ends up as a compile error or a silent `Debug`. The
    // store returns the crate's own error, which is what a log line wants.
    let pool = state.db().pool();
    let pruned = match omnion_ai_hub::decision_store::prune(pool, RETENTION_DAYS).await {
        Ok(pruned) => pruned,
        Err(error) => {
            tracing::warn!(error = %error, "the decision pruner could not run");
            return TickReport::default();
        }
    };

    // A background delete with no record is indistinguishable from a bug that is eating rows.
    if pruned > 0 {
        let entry = omnion_audit::NewAuditEntry::system("ai.route.decisions_pruned")
            .metadata(serde_json::json!({ "pruned": pruned, "retention_days": RETENTION_DAYS }));
        if let Err(error) = omnion_audit::record(pool, entry).await {
            tracing::warn!(error = %error, pruned, "a decision prune could not be recorded");
        }
    }

    TickReport { pruned }
}

/// Start the pruner; the handle is kept by the binary and ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    // Once a day. The retention is 90 days, so a daily sweep removes at most a day of rows and
    // the table never shrinks in a step an operator would notice in a query plan.
    let period = StdDuration::from_secs(24 * 60 * 60);

    tracing::info!(retention_days = RETENTION_DAYS, "AI decision pruner started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(period);
        // A restart storm must not turn into a burst of catch-up sweeps: each one is a bulk
        // delete, and a machine that has been down for a week does not need seven of them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            let report = tick(&state).await;
            if !report.is_idle() {
                tracing::debug!(?report, "AI decision prune tick");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_that_pruned_nothing_is_idle() {
        // The fresh-install case: an installation that has served no AI request must not log a
        // line every day. A row count of zero is not an event.
        assert!(TickReport::default().is_idle());
        assert!(!TickReport { pruned: 1 }.is_idle());
    }

    #[test]
    fn the_retention_is_ninety_days() {
        // The number is the contract with the spec, and a pruner that keeps 30 days would drop a
        // decision an operator is still asking about. Asserting it costs one line and catches
        // the "reused the health runner's constant" mistake, which is the obvious way to get
        // this wrong: `retention_days` there is also 30 and lives one module away.
        assert_eq!(RETENTION_DAYS, 90);
    }
}
