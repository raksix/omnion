//! The scheduled publishing worker (REQ-064, slice 1).
//!
//! `main.rs` spawns this task when the worker is enabled. Each tick claims the queue entries that
//! are due and runs them.
//!
//! Two decisions shape this file, and both are about what happens when there is more than one
//! worker, or when a run goes wrong:
//!
//! * **The claim is a write, and it happens before the page is touched.** `claim_due` takes the
//!   rows with `for update skip locked` and stamps `claimed_at` in the same transaction that
//!   hands them back. Two workers sweeping the same due row is the ordinary case on a scaled
//!   deployment, not an edge case — and the second one steps over what the first is holding
//!   rather than publishing the same revision again. A runner that read first and wrote after
//!   would be correct on one worker and silently wrong on two, which is the worst shape a
//!   scheduler can have.
//! * **A failed publish is a row with a reason, not a gap.** The entry flips to `failed`, keeps
//!   its error text, and the queue screen's `Retry` puts it back. The alternative — a sweep that
//!   logs and moves on — produces a site where a post simply did not appear, and the only
//!   evidence is a log line nobody reads at the hour it happened.

use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// How often the queue is swept.
///
/// Thirty seconds is the acceptance criterion's own word: a scheduled publish fires "within a
/// minute of its time". A one-minute tick could satisfy that only by never being late, so the
/// tick is half the budget and the sweep has room to be early.
const POLL: StdDuration = StdDuration::from_secs(30);

/// Most entries one tick will claim.
///
/// Bounded so a queue that accumulated a thousand entries while the worker was down does not
/// turn into a thousand publishes inside one transaction-heavy tick. The rest go on the next.
const BATCH: i64 = 50;

/// Start the publishing worker; the returned handle ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tracing::info!(
        poll_secs = POLL.as_secs(),
        "publishing queue worker started"
    );

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(POLL);
        // A slow tick must not become a burst: the rows are still pending, so the next tick
        // claims the same set and the queue records two passes rather than one enormous one.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Boot has its own work; the first tick waits its turn.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state).await {
                tracing::warn!(error = %error, "the publishing tick failed");
            }
        }
    })
}

/// One sweep: claim what is due, run it, record what happened.
pub async fn tick(state: &AppState) -> Result<usize, omnion_content::ContentError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let due = omnion_content::claim_due(pool, now, BATCH).await?;
    if due.is_empty() {
        return Ok(0);
    }
    tracing::info!(count = due.len(), "publishing queue: running due entries");

    let mut done = 0usize;
    for entry in &due {
        match omnion_content::run_entry(pool, entry).await {
            Ok(result) => {
                tracing::info!(entry = %entry.id, page = %entry.page_slug, result, "published");
                done += 1;
            }
            // The failure is already on the row (`run_entry` wrote it). A page that has no draft
            // to publish — the common cause — is a content problem the queue screen shows, not
            // a worker problem, so it is logged at info and does not stop the sweep.
            Err(error) => {
                tracing::info!(
                    entry = %entry.id,
                    page = %entry.page_slug,
                    error = %error,
                    "a scheduled publish did not run"
                );
            }
        }
    }
    Ok(done)
}

/// Run one entry right now, outside the loop — the `Publish now` button and the tests use it.
///
/// It is the same claim → run → record path the loop takes, so a row's `result` is written by
/// the same code whether it fired on time or because somebody pressed a button.
pub async fn run_now(
    state: &AppState,
    entry_id: uuid::Uuid,
) -> Result<String, omnion_content::ContentError> {
    let pool = state.db().pool();
    let claimed = omnion_content::claim_entry(pool, entry_id).await?;
    match claimed {
        Some(entry) => omnion_content::run_entry(pool, &entry).await,
        None => Err(omnion_content::ContentError::PublishingEntryNotFound),
    }
}
