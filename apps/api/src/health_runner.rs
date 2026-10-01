//! The health runner: this process's heartbeat, and the scheduled probe run (REQ-014, slice 4).
//!
//! Two jobs, and both of them exist because a table had a reader and no writer.
//!
//! **The heartbeat.** `worker_heartbeats` shipped in slice 1 with a probe that counts rows and
//! names the stale ones. Nothing in the platform ever wrote a row, so in production the worker
//! card would have read "no worker has registered a heartbeat" for ever, and the acceptance
//! criterion that stopping a worker takes `4/4` to `3/4` would have been proven by the walk's
//! own fixture rather than by the platform. This module is the writer: one upsert on boot and
//! one per interval, keyed on `kind@host#pid` so a restart reclaims the row instead of adding
//! another (`omnion_health::workers::heartbeat_id`).
//!
//! **The scheduled checks.** `run_and_record` was reached from four route handlers and nowhere
//! else, so samples existed only when somebody pressed "Run all checks". The request's
//! `check_interval_seconds` (5–600, default 60) was stored, shown, and validated by
//! `Threshold`-shaped form — and nothing read it to schedule anything. A health panel whose
//! history fills in only while an operator watches it is not a health panel; the default 24 h
//! trend would have been an empty chart eight hours after the last visit.
//!
//! Three decisions are worth stating, because each one has an obvious wrong answer:
//!
//! * **The interval is read from `health_settings` every tick, not cached at boot.** An operator
//!   who saves 15 s means 15 s from the next tick, not after the next deploy — and the settings
//!   screen's own sentence says "saving refreshes the next run immediately".
//! * **A failed run is logged and the loop continues.** A probe run that cannot reach the
//!   database has not told anybody anything about the platform, and a runner that exits on the
//!   first error stops publishing the heartbeats that would have reported *that*.
//! * **Missed ticks are skipped, never caught up.** After a long GC pause a catch-up burst
//!   would write a run's worth of samples in one instant, and `health_samples` would carry
//!   several rows with the same `sampled_at` for one honest reading.
//!
//! The `health.*` events the request names are emitted here rather than in the crate: the crate
//! is infrastructure and knows nothing about the bus, and the route that already ran the same
//! registry does not emit them either — which is why slice 4 has this file rather than three
//! more call sites in a library that has no bus handle.

use std::time::Duration as StdDuration;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// The kind this process registers itself under.
///
/// `api` rather than `web`: the binary that serves HTTP is `omnion-api`, and the panel calls
/// the *other* thing web. The row's `kind` is what an operator reads in the `n/m` card, so the
/// name has to be the one they call the process when they are looking for it in `ps`.
const WORKER_KIND: &str = "api";

/// How often the heartbeat row is refreshed, independent of the probe interval.
///
/// Half the *staleness* limit rather than the staleness limit itself: a heartbeat that lands
/// exactly when the panel would start calling it stale is a fleet that reads healthy only
/// because both clocks happen to agree. `health_settings.worker_stale_seconds` defaults to 120
/// and this fires every 30 s, so a worker gets four chances to be seen before it is called out.
const HEARTBEAT_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// The floor between two probe runs.
///
/// One second. The settings form allows 5 s and up, so this can only ever be hit by a
/// hand-edited row, and the floor turns "someone wrote 0" into a spin at a readable rate rather
/// than a tight loop that starves the probes themselves.
const MIN_CHECK_INTERVAL: StdDuration = StdDuration::from_secs(1);

/// The upper bound between two probe runs, when the settings row cannot be read.
///
/// Ten minutes, and it is an upper bound rather than the 600 s the form allows because this is
/// the value used **when the policy could not be read at all** — an unreadable settings row is
/// not a reason to probe every second, and it is not a reason to never probe again either.
const MAX_CHECK_INTERVAL: StdDuration = StdDuration::from_secs(600);

/// Start the health runner. The handle is kept by the binary and ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tracing::info!(
        kind = WORKER_KIND,
        heartbeat_secs = HEARTBEAT_INTERVAL.as_secs(),
        "the health runner started"
    );

    tokio::spawn(async move {
        // Beat once at boot rather than waiting out the first interval: a process that boots
        // and takes thirty seconds to register looks identical to one that died at boot, and
        // the panel is exactly where somebody looks to tell those apart.
        beat_once(&state).await;

        let mut heartbeats = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeats.set_missed_tick_behavior(MissedTickBehavior::Skip);
        heartbeats.tick().await; // the immediate tick already happened above

        loop {
            // The heartbeat lives on its own clock so a slow probe run cannot silence it: the
            // whole point of the row is to say "this process is still here" while something
            // else is stuck, and a heartbeat written by the probe loop after a 40-second
            // timeout is a heartbeat that reports the timeout instead of the liveness.
            tokio::select! {
                _ = heartbeats.tick() => beat_once(&state).await,
                _ = tokio::time::sleep(check_interval(&state).await) => run_once(&state).await,
            }
        }
    })
}

/// Mark this process's heartbeat row as stopped, on a graceful shutdown.
///
/// Without it a clean stop is indistinguishable from a crash until the staleness window
/// closes: the panel says "stale", which is true and useless, because "we stopped it on
/// purpose" and "it died" are the two answers an operator needs.
pub async fn stopped(state: &AppState) {
    if let Err(error) = omnion_health::mark_stopped(state.db().pool(), &self_heartbeat_id()).await {
        tracing::warn!(error = %error, "this worker's heartbeat could not be marked stopped");
    }
}

/// The id this process's heartbeat is stored under.
fn self_heartbeat_id() -> String {
    omnion_health::heartbeat_id(
        WORKER_KIND,
        &omnion_health::hostname(),
        std::process::id() as i32,
    )
}

/// Write this process's heartbeat row, upserting on conflict.
async fn beat_once(state: &AppState) {
    let build = state.build();
    let beat = omnion_health::self_heartbeat(
        WORKER_KIND,
        &omnion_health::hostname(),
        build.version,
        serde_json::json!({
            "service": build.service,
            "environment": state.config().env.as_str(),
            "probing": true,
        }),
    );
    if let Err(error) = omnion_health::beat(state.db().pool(), &beat).await {
        tracing::warn!(error = %error, "the heartbeat could not be written");
    }
}

/// How long to wait before the next probe run.
///
/// Read from the settings row on **every** call rather than cached: the screen says saving
/// "refreshes the next run immediately", and a cached interval makes that sentence a lie for
/// up to one deploy. A row that cannot be read falls back to the form's own default, which is
/// also the migration's default, so the two cannot drift apart silently.
async fn check_interval(state: &AppState) -> StdDuration {
    let seconds = match omnion_health::load_settings(state.db().pool()).await {
        Ok(settings) => settings.check_interval_seconds,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "the check interval could not be read; falling back to the default"
            );
            omnion_health::DEFAULT_CHECK_INTERVAL_SECONDS
        }
    };
    // `check_interval_seconds` is an `i32` on the settings row, so the bounds are `i32` too.
    // Clamping against `i64` bounds would have widened the type at the call site and this is
    // the kind of cast pair that silently disagrees with the migration's own check the first
    // time one of the two numbers is edited.
    let seconds = seconds.clamp(
        MIN_CHECK_INTERVAL.as_secs() as i32,
        MAX_CHECK_INTERVAL.as_secs() as i32,
    );
    StdDuration::from_secs(u64::try_from(seconds.max(1)).unwrap_or(1))
}

/// Run every probe once, record the samples, apply the policy.
///
/// Same function the "Run all checks" button reaches, deliberately: a scheduler with its own
/// copy of the registry is a scheduler that can drift from the screen that says what it will do,
/// and the operator who reads the screen is the one who is misled.
async fn run_once(state: &AppState) {
    let ctx = crate::routes::health_panel::probe_context(state).await;
    match omnion_health::run_and_record(state.db().pool(), &ctx).await {
        Ok((overview, policy)) => {
            // The changes first, then the run itself. Order is not cosmetic: a receiver that
            // opens on `health.checks.completed` and then receives the degraded event for the
            // same tick has to reconcile the two, and delivering the news before the "still
            // working on it" is the order that reads correctly.
            let announced = crate::health_events::announce_changes(state.db().pool(), &policy)
                .await
                + crate::health_events::announce_run(state.db().pool(), &overview, None).await;
            let worst = overview
                .services
                .iter()
                .map(|report| report.service.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::info!(
                services = overview.services.len(),
                state = overview.banner.state,
                worst,
                transitions = policy.transitions.len(),
                announced,
                "the scheduled health run completed"
            );
        }
        Err(error) => {
            tracing::warn!(error = %error, "the scheduled health run failed");
        }
    }
}
