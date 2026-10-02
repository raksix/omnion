//! The staging clone worker (REQ-017, slice 2).
//!
//! `main.rs` spawns this task when the worker is enabled. Each tick it claims the oldest pending
//! clone job and runs it. It is the only thing in the platform that writes rows into a staging
//! environment.
//!
//! Three decisions shape this file:
//!
//! * **A tick is a single job, not a batch.** A clone of a large site is minutes of database
//!   work; draining a queue of them in one tick means the API process is busy for as long as the
//!   slowest site and every other request queues behind it. One job per tick bounds the worst
//!   case to one clone.
//!
//! * **A failing job is logged and the loop continues.** A clone that fails on one organization's
//!   content must not stop the platform from cloning the next tenant's. The job row already
//!   carries the failure, so the error has a home; this file's only job is to make sure it got
//!   there.
//!
//! * **The claim is a single `update … for update skip locked` statement.** Re-reading the
//!   claimed id afterwards races the update that just claimed it, and two API processes would
//!   drain the same job and write the same rows twice.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// How often the worker looks for work.
///
/// Short, because a clone's progress bar is watched by the operator who just asked for it. A
/// slow tick does not delay the clone much, but it delays the *first visible progress*, which is
/// what makes a submitted wizard look like it did nothing.
const TICK: StdDuration = StdDuration::from_secs(2);

/// Spawn the worker.
///
/// Returns the task handle, or `None` when the pool is already closed — the same contract the
/// other runners use, so `main.rs` logs one uniform warning instead of one per subsystem.
#[must_use]
pub fn spawn(state: AppState) -> Option<JoinHandle<()>> {
    let pool = state.db().pool().clone();
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick of a `tokio::time::interval` fires immediately, and the first thing that
        // should happen after a restart is draining whatever the last process left pending.
        loop {
            ticker.tick().await;
            match tick(&pool).await {
                Ok(Some(job)) => tracing::info!(%job, "staging clone finished"),
                Ok(None) => {}
                Err(err) => tracing::warn!(error = %err, "a staging clone did not finish"),
            }
        }
    }))
}

/// Run at most one claimed job. Returns the job id it finished.
async fn tick(pool: &sqlx::PgPool) -> Result<Option<Uuid>, String> {
    let Some(job) = omnion_environment::store::claim_next_job(pool)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(None);
    };
    let job_id = job.id;
    let outcome = omnion_environment::runner::run_job(pool, &job)
        .await
        .map_err(|err| err.to_string())?;
    tracing::info!(
        %job_id,
        status = outcome.status.as_str(),
        areas = outcome.areas.len(),
        "staging clone job closed"
    );
    Ok(Some(job_id))
}
