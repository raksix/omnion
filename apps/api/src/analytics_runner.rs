//! The background analytics rollup worker.
//!
//! `main.rs` spawns this task when the worker is enabled (`OMNION_ANALYTICS_RUNNER`, default
//! on). Each tick rebuilds the recent hourly and daily buckets from the raw rows — a pageview
//! recorded a second ago shows up in a report after the next tick — and prunes hourly buckets
//! that fell out of the 48-hour window (docs/requests/REQ-007).
//!
//! Nothing here remembers anything: a bucket run is idempotent (it deletes its own rows and
//! writes what it computes), so a tick that cannot reach the database is logged and the next
//! one recomputes the same buckets from the same rows. The knobs are `OMNION_ANALYTICS_POLL_MS`.

use std::collections::HashMap;
use std::time::Duration as StdDuration;

use omnion_events::{NewEvent, bus};
use omnion_module_analytics::{privacy, rollup};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// Start the rollup worker; the returned handle is kept by the binary (and ends with the
/// process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().analytics.poll_ms.max(1_000);

    tracing::info!(poll_ms, "analytics rollup worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the raw rows are still
        // there, so the next tick rolls the same buckets up again.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        // The last completed hour each site was watched for. In memory on purpose: it only
        // saves queries — whether an hour has already been announced is asked of the events
        // themselves, so a restart cannot announce one twice.
        let mut watched: HashMap<Uuid, OffsetDateTime> = HashMap::new();

        loop {
            ticks.tick().await;
            match rollup::tick(state.db().pool(), OffsetDateTime::now_utc()).await {
                Ok(report) if !report.is_idle() => {
                    tracing::debug!(?report, "analytics rollup tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "analytics rollup tick failed");
                }
            }

            // The spike watch (REQ-007, slice 4): the hour that just closed is compared against
            // the trailing week, and an hour that ran away is announced once.
            watch_spikes(&state, &mut watched).await;
        }
    })
}

/// Announce every hour that ran more than [`privacy::SPIKE_FACTOR`] times its trailing median.
///
/// A site is only asked once per hour, and the answer rides on the platform bus as
/// `analytics.traffic_spike` — the fact the system-health notifications subscribe to. A failure
/// here is a warning and never stops the worker: the rollups are the point of the tick.
async fn watch_spikes(state: &AppState, watched: &mut HashMap<Uuid, OffsetDateTime>) {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let completed = privacy::hour_start(now) - time::Duration::hours(1);

    let sites = match rollup::tracked_sites(pool).await {
        Ok(sites) => sites,
        Err(error) => {
            tracing::warn!(error = %error, "the spike watch could not list the sites");
            return;
        }
    };

    for site_id in sites {
        if watched.get(&site_id) == Some(&completed) {
            continue;
        }
        watched.insert(site_id, completed);

        let finding = match privacy::detect_spike(pool, site_id, now).await {
            Ok(Some(finding)) => finding,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(site_id = %site_id, error = %error, "the spike watch failed");
                continue;
            }
        };

        match privacy::spike_recorded(pool, site_id, finding.hour).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(site_id = %site_id, error = %error, "the spike guard failed");
                continue;
            }
        }

        let organization: Option<Uuid> =
            match sqlx::query_scalar("select organization_id from sites where id = $1")
                .bind(site_id)
                .fetch_optional(pool)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(site_id = %site_id, error = %error, "the spike site could not be read");
                    continue;
                }
            };

        let emission = bus::emit(
            pool,
            NewEvent::new("analytics.traffic_spike")
                .organization(organization)
                .site(site_id)
                .payload(serde_json::json!({
                    "hour": privacy::hour_label(finding.hour),
                    "visitors": finding.visitors,
                    "median": finding.median,
                    "factor": finding.factor,
                })),
        )
        .await;

        if let Err(error) = emission {
            tracing::warn!(site_id = %site_id, error = %error, "the spike event could not be recorded");
        }
    }
}

#[cfg(test)]
mod tests {
    use omnion_core::config::AnalyticsConfig;

    #[test]
    fn the_worker_polls_fast_enough_for_a_fresh_pageview_to_show_up() {
        let defaults = AnalyticsConfig::default();
        assert!(
            defaults.poll_ms <= 30_000,
            "a rollup should not lag a minute behind"
        );
        assert!(
            defaults.collect_per_minute >= 60,
            "a page needs room for its own beacon"
        );
    }
}
