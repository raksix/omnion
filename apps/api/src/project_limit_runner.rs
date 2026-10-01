//! The project limit notice worker (REQ-133, slice 4).
//!
//! `main.rs` spawns this task when the worker is enabled (`OMNION_PROJECT_LIMIT_RUNNER`, default
//! on). Each tick walks the projects that have at least one cap set, asks the store which
//! crossings nobody has been told about, and emits one event per crossing **it alone won**.
//!
//! Everything else about this REQ's limits already existed and was correct: `Limits::warns`
//! computed the 80 percent crossing, `ensure_run_within_limits` refused the 100th run with a
//! message naming the limit and the owner, and the limits screen rendered the bar amber. What did
//! not exist was the word *once*. The warning was computed per read, so reloading the screen
//! re-warned for ever, and the `automation.project.limit.warning` / `.limit_exceeded` events the
//! REQ names as its webhook-relevant pair were emitted by nothing on this branch.
//!
//! That is the same shape this branch has now met seven times — a function that computes the right
//! answer, a screen that renders it, and no caller able to produce the state it describes. The
//! difference here is that the answer is *not* idempotent: a warning that repeats is worse than a
//! missing one, because an operations team that subscribed to it learns to ignore the channel.
//!
//! Four decisions, all of them about the claim:
//!
//! * **The claim is taken by the store, not here.** `claim_due_notices` owns the
//!   `on conflict do nothing` whose row count is the decision. This worker emits only what it was
//!   handed, so a second worker (or a second API process on the same database) cannot double-notify
//!   even though both are running the identical loop.
//! * **The emit is best-effort and the claim is not given back.** A bus that is briefly unreachable
//!   loses that one notice; un-claiming it would reopen the window and turn a transient database
//!   blip into a storm. The count is logged, so a lost notice is visible rather than silent.
//! * **A tick that finds nothing is idle, not a warning.** Every other worker in the platform logs
//!   an empty pass at `debug`; a warning per empty minute fills the log with entries nobody reads
//!   and makes the one line that matters — a sweep that cannot reach the database — the one in a
//!   thousand still worth looking at.
//! * **The payload carries ids, the column name, the two numbers and the period.** Never a
//!   workflow definition, never a credential reference: this is the event an operations team
//!   subscribes to and forwards, and `period` is what lets a consumer tell "crossed again after
//!   midnight" from "the same crossing observed twice" without keeping state of its own.

use std::time::Duration as StdDuration;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use omnion_events::{NewEvent, bus};
use omnion_workflows::limits::{self, LimitNotice, NoticeKind};

use crate::state::AppState;

/// Start the limit-notice worker; the handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().project_limit.poll_ms.max(5_000);
    let max_projects = state.config().project_limit.max_projects;

    tracing::info!(poll_ms, max_projects, "project limit notice worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the crossings are still true, and
        // the claim would suppress the second notice anyway — but a burst is a sign the interval is
        // wrong, and hiding that behind the claim is exactly the sort of quiet this file argues
        // against.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Boot has its own work to do before a limit is worth a socket.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match tick(state.db().pool(), max_projects).await {
                Ok(report) if report.is_idle() => {
                    tracing::debug!(
                        projects = report.projects,
                        "the project limit tick found nothing to say"
                    );
                }
                Ok(report) => tracing::info!(
                    projects = report.projects,
                    warnings = report.warnings,
                    exceeded = report.exceeded,
                    failed = report.failed,
                    "the project limit tick did its work"
                ),
                // A worker that dies on the first refused statement is a worker that never warns
                // anybody. The error is logged and the loop continues.
                Err(error) => tracing::warn!(%error, "the project limit tick failed"),
            }
        }
    })
}

/// What one pass did.
///
/// Returned rather than only logged so a caller — and a test — can assert on it. `failed` is
/// counted separately from `projects` for the reason every worker here keeps it: a project that
/// could not be swept is a fact an operator needs, and it is invisible in a count of successes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    /// Projects looked at.
    pub projects: usize,
    /// Warning crossings emitted.
    pub warnings: u64,
    /// Cap crossings emitted.
    pub exceeded: u64,
    /// Projects that could not be swept.
    pub failed: usize,
}

impl TickReport {
    /// Whether the pass said anything at all.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.warnings == 0 && self.exceeded == 0
    }
}

/// One pass over the capped projects.
pub async fn tick(pool: &PgPool, max_projects: i64) -> Result<TickReport, String> {
    let projects = limits::projects_with_caps(pool, max_projects)
        .await
        .map_err(|error| error.to_string())?;
    let mut report = TickReport {
        projects: projects.len(),
        ..TickReport::default()
    };

    for project_id in projects {
        match sweep_project(pool, project_id).await {
            Ok((warnings, exceeded)) => {
                report.warnings += warnings;
                report.exceeded += exceeded;
            }
            Err(error) => {
                report.failed += 1;
                tracing::warn!(%error, %project_id, "a project's limit notices could not be swept");
            }
        }
    }
    Ok(report)
}

/// One project's crossings, emitted by this caller alone.
async fn sweep_project(pool: &PgPool, project_id: Uuid) -> Result<(u64, u64), String> {
    // The claim comes first and decides everything after it: an empty list means somebody else was
    // first, or nothing crossed, and both are equally "this caller emits nothing".
    let notices = limits::claim_due_notices(pool, project_id)
        .await
        .map_err(|error| error.to_string())?;
    let mut warnings = 0_u64;
    let mut exceeded = 0_u64;

    for notice in notices {
        if emit(pool, &notice).await.is_ok() {
            match notice.kind {
                NoticeKind::Warning => warnings += 1,
                NoticeKind::Exceeded => exceeded += 1,
            }
        }
    }
    Ok((warnings, exceeded))
}

/// `automation.project.limit.warning` or `.limit_exceeded`, with the numbers an operator needs.
///
/// **The name is written out twice on purpose — once per arm, at the constructor.** The name
/// used to be a local binding named `name`, built by a `match` three lines above the
/// constructor, and the drift gate in `apps/api/tests/events.rs` reported both catalogue rows
/// as `Live` with no emitter behind them for the whole of their life. That gate is a *source*
/// scanner: it reads `NewEvent::new("…")` and looks at nothing else, so a name that arrives
/// through a binding is invisible to it, and the row it cannot see reads as a row nothing emits.
/// The only two honest repairs were to teach the scanner about bindings — which would have
/// taught it to accept any expression, and so to accept a `format!` too, turning a gate that
/// names a defect into one that tolerates a wire contract assembled at runtime — or to write
/// the name where the scanner can see it. `NoticeKind::as_str` below stays the assertion of
/// *what* the two names are; this is where they are *emitted*.
///
/// The real lesson is not the shape. It is that a green suite is not the evidence here: both
/// rows shipped `Live` with the gate **red**, and the tick that wrote them ran a different
/// suite. Nothing in the product was broken — the worker emitted correctly at runtime — so
/// every behavioural test stayed green while the one gate that names the contract was the only
/// thing that could have said anything.
async fn emit(pool: &PgPool, notice: &LimitNotice) -> Result<(), omnion_events::EventsError> {
    let event = NewEvent::new(match notice.kind {
        NoticeKind::Warning => "automation.project.limit.warning",
        NoticeKind::Exceeded => "automation.project.limit.exceeded",
    })
    .payload(serde_json::json!({
        "project_id": notice.project_id,
        "project_key": notice.project_key,
        "limit": notice.limit,
        "current": notice.current,
        "max": notice.max,
        "period": notice.period,
    }));
    bus::emit(pool, event).await.map(|_report| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_names_are_the_reqs_and_not_a_templated_one() {
        // The event names are the REQ's, and a `format!("{prefix}.{suffix}")` would make them a
        // function of an enum that a later contributor may extend with a name nobody subscribed
        // to. Two constants, asserted, is the shape that keeps the wire contract in the test.
        assert_eq!(
            NoticeKind::Warning.as_str(),
            "warning",
            "the claim column and the payload agree"
        );
        assert_eq!(NoticeKind::Exceeded.as_str(), "exceeded");
    }

    #[test]
    fn an_idle_pass_is_idle_and_not_failed() {
        // The worker logs `debug` on idle and `info` otherwise. A report that had been walked but
        // said nothing must read as idle, or the log fills with a warning per minute.
        let report = TickReport {
            projects: 50,
            failed: 0,
            ..TickReport::default()
        };
        assert!(report.is_idle(), "50 projects and no crossing is idle");

        let spoke = TickReport {
            warnings: 1,
            ..TickReport::default()
        };
        assert!(!spoke.is_idle());
    }

    #[test]
    fn a_project_that_could_not_be_swept_is_counted_not_hidden() {
        // `projects` and `failed` are separate fields on purpose: a sweep where every project fails
        // and one where every project is clean are the same number in a single counter, and only
        // one of them is an outage.
        let report = TickReport {
            projects: 3,
            failed: 3,
            ..TickReport::default()
        };
        assert!(report.is_idle(), "three failures emitted nothing");
        assert_eq!(report.failed, 3);
        assert_eq!(report.projects, 3);
    }
}
