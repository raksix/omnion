//! The re-wrap runner: the background writer that walks a rotation's versions.
//!
//! `main.rs` spawns this task when the runner is enabled (`OMNION_SECRETS_RUNNER`, default on).
//! Each tick looks for a live re-wrap job and re-seals one small batch of its versions, then
//! sleeps. Everything it needs to resume after a restart is a row: the job's `status`, its
//! `cursor` and its `rewrapped_count`.
//!
//! The knobs are `OMNION_SECRETS_POLL_MS` and `OMNION_SECRETS_REWRAP` (the batch size, which
//! defaults to the crate's [`omnion_secrets::REWRAP_BATCH`]).
//!
//! Two properties this runner is written to keep:
//!
//! * **It never holds a lock a normal read needs.** A batch is a handful of single-row updates,
//!   and between them the pool is free — so a lease redemption during a rotation succeeds on the
//!   old key for every version the walk has not reached. That is the invariant the request makes
//!   about a rotation being an online ceremony.
//! * **A rotation that cannot finish says so instead of skipping.** A version that will not
//!   unseal pauses the job with the reason, and the operator sees it on the screen. The walk
//!   never advances past a version it could not re-seal.

use std::time::Duration as StdDuration;

use omnion_secrets::store::{self, RewrapJob};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    // The floor keeps a misconfigured poll from spinning the runner.
    let poll_ms = state.config().secrets.rewrap_poll_ms.max(250);
    tracing::info!(poll_ms, "secret re-wrap runner started");

    tokio::spawn(async move {
        // A fresh installation may have no key ring at all; the first write creates one. Doing
        // it here means an operator who opens the key screen before saving any secret still
        // sees a real ring rather than an empty state that is really a missing key.
        if let Err(error) = store::ensure_active_key(state.db().pool()).await {
            tracing::info!(error = %error, "no root key was created at boot");
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the versions are still on the
        // old key, and the next tick walks them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match tick(&state).await {
                Ok(true) => {}
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "the re-wrap tick failed");
                }
            }
        }
    })
}

/// One tick: advance the live job by one batch. Returns whether a batch was applied.
async fn tick(state: &AppState) -> Result<bool, omnion_secrets::SecretsError> {
    let pool = state.db().pool();
    let Some(job) = store::live_rewrap_job(pool).await? else {
        return Ok(false);
    };
    if job.status == "paused" {
        // A paused job is an operator's decision; the runner leaves it alone.
        return Ok(false);
    }
    let report = store::rewrap_batch(pool, job.id).await?;
    if !report.is_idle() {
        tracing::info!(
            job = %job.id,
            rewrapped = report.rewrapped,
            complete = report.complete,
            "re-wrap batch applied"
        );
    }
    Ok(true)
}

/// The finished jobs the screen's history strip reads, newest first.
pub async fn recent_jobs(
    state: &AppState,
    limit: i64,
) -> Result<Vec<RewrapJob>, omnion_secrets::SecretsError> {
    let rows = sqlx::query_as::<_, RewrapJob>(
        "select id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                pause_reason, last_error, started_at, completed_at \
         from secret_rewrap_jobs where status in ('completed', 'failed') \
         order by started_at desc limit $1",
    )
    .bind(limit)
    .fetch_all(state.db().pool())
    .await?;
    Ok(rows)
}
