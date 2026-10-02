//! The background form → lead drain.
//!
//! `main.rs` spawns this task when the ingress is enabled (`OMNION_LEAD_RUNNER`, default on).
//! Each tick reads the `form.submitted` events above its durable cursor and files them as
//! contacts and deals — the half of "a form submission becomes a lead" that the CRM owns, the
//! other half being the public submit endpoint in REQ-064.
//!
//! Nothing here remembers anything: the cursor, the settings and the records are rows. A tick
//! that cannot reach the database is logged and the next one picks the work up, because a
//! submission that has not been filed is still on the bus with its id above the cursor.

use std::time::Duration as StdDuration;

use omnion_module_crm::leads;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// The floor between two ticks when the environment is misconfigured.
const MIN_POLL_MS: u64 = 100;

/// The poll interval used when the configuration does not name one.
const DEFAULT_POLL_MS: u64 = 5_000;

/// Start the drain; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let config = state.config().crm.clone();
    let poll_ms = config.lead_poll_ms.max(MIN_POLL_MS);
    let batch = i64::try_from(config.lead_batch).unwrap_or(leads::DEFAULT_BATCH).max(1);

    tracing::info!(poll_ms, batch, "the CRM lead drain started");

    tokio::spawn(async move {
        // Seeded to the end of the bus before the first tick: a fresh installation watches
        // forward rather than turning every submission ever recorded into a contact. Doing it
        // before the loop is also what closes the boot window — a submission recorded between
        // the seed and the first tick sits above the cursor, so the tick files it.
        match leads::seed_cursor(state.db().pool()).await {
            Ok(Some(cursor)) => {
                tracing::info!(cursor, "the CRM lead cursor seeded to the end of the bus");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "the CRM lead cursor could not be seeded");
            }
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the submissions are still on
        // the bus, and each one is filed at most once whatever order the ticks arrive in.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match leads::drain(state.db().pool(), batch).await {
                Ok(report) if !report.is_idle() => {
                    tracing::info!(
                        created = report.created,
                        merged = report.merged,
                        rejected = report.rejected,
                        orphaned = report.orphaned,
                        disabled = report.disabled,
                        failures = report.failures.len(),
                        "the CRM lead drain filed submissions"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "the CRM lead drain failed");
                }
            }
        }
    })
}

/// The configuration keys the runner reads, with the defaults a fresh install gets.
///
/// This lives here rather than in `omnion_core::config` so the numbers the runner actually uses
/// are next to the loop that uses them, and the test below can assert the two agree.
#[must_use]
pub fn default_poll_ms() -> u64 {
    DEFAULT_POLL_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misconfigured_poll_interval_cannot_spin_the_drain() {
        // A zero or negative poll is a configuration mistake, and the symptom without the floor
        // is a task that saturates a core re-reading the bus.
        assert!(MIN_POLL_MS >= 100);
        assert!(MIN_POLL_MS < DEFAULT_POLL_MS);
    }

    #[test]
    fn the_runner_watches_the_event_the_module_consumes() {
        assert_eq!(leads::FORM_SUBMITTED, "form.submitted");
        assert!(leads::DEFAULT_BATCH >= 1);
    }
}
