//! The per-organization **audit** retention sweep (docs/requests/REQ-005, slice 4).
//!
//! Named for what it sweeps, and that is the point: there are two retention workers in
//! this binary and they are not interchangeable. This one removes *audit rows* past each
//! tenant's own window; `crate::retention_runner` removes *files and versions* past a
//! site's policy (REQ-010). They were both called `retention_runner` on two branches,
//! and a merge silently wired one of the two `spawn` calls to the other's module.
//!
//! `main.rs` spawns this task when the sweep is enabled (`OMNION_AUDIT_RETENTION_SWEEP`, default
//! on). Each tick it reads every organization's own stored `audit_retention_days`, works out the
//! instant that tenant's window closed, and removes the rows that fell out of it — then files a
//! system audit row and announces `organization.retention.swept` with the count and the cutoff.
//!
//! Why a runner at all, when the analytics module already has a retention purge: the two are
//! the same idea at different scopes. `privacy::purge` is a *site* about a site's visitors, an
//! operator action with a button and a result. This is the *organization's own* window, applied
//! by the platform, unattended — the thing that makes a stored number mean something. A setting
//! that is validated, saved and rendered but never read is a promise the product does not keep.
//!
//! Three decisions are worth naming, because each of them is easy to get quietly wrong:
//!
//! * **Each tenant keeps its own window.** There is no global retention: the number in
//!   `organization_settings` belongs to that organization, and a sweep that used one platform
//!   default would silently delete a tenant's history that the tenant paid to keep.
//! * **A sweep that removed nothing files nothing.** The audit row and the event are about a
//!   deletion that happened. A nightly `organization.retention.swept` with
//!   `rows_removed: 0` on a platform with two hundred tenants would be two hundred rows a day
//!   in the trail, and a trail full of its own housekeeping is a trail nobody reads. So the
//!   tick skips a tenant with nothing to purge, entirely.
//! * **The audit row is written after the delete, not inside the same transaction.** A trail
//!   that cannot record its own housekeeping would have no way to explain the gap a purge left;
//!   and the row is deliberately *newer* than the cutoff, so the next sweep cannot remove the
//!   receipt for the previous one.

use std::time::Duration as StdDuration;

use omnion_audit::{NewAuditEntry, RetentionSweep};
use omnion_events::{NewEvent, bus};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// Start the sweep worker; the returned handle is kept by the binary (and ends with the
/// process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let retention = state.config().audit_retention.clone();
    let sweep_seconds = retention.sweep_seconds.max(1);

    tracing::info!(
        sweep_seconds,
        batch = retention.sweep_batch,
        "audit retention sweep started"
    );

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_secs(sweep_seconds));
        // A slow tick must not turn into a burst of catch-up sweeps. The work is idempotent,
        // so skipping is free, while a catch-up burst would re-read every tenant back to back.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately: a process that is restarted more often than the
        // cadence still sweeps, instead of a tenant on a daily schedule losing a day every
        // time the box rebooted.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = sweep_once(&state).await {
                tracing::warn!(error = %error, "the audit retention sweep tick failed");
            }
        }
    })
}

/// Run one sweep across the tenants that have something to purge.
///
/// Returns how many rows went. A tenant whose `organization_settings` row is missing falls back
/// to the schema default rather than being skipped: the settings table is backfilled, so a
/// missing row means a tenant created between the backfill and now, and *not* sweeping it would
/// keep rows forever — the opposite of what the setting promises.
pub async fn sweep_once(state: &AppState) -> Result<i64, omnion_audit::AuditError> {
    let now = OffsetDateTime::now_utc();
    let mut removed_total = 0_i64;

    for window in due_windows(state, now).await? {
        let removed = match omnion_audit::purge_before(
            state.db().pool(),
            window.organization_id,
            window.cutoff,
        )
        .await
        {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(
                    organization_id = %window.organization_id,
                    error = %error,
                    "the retention sweep could not purge one organization"
                );
                continue;
            }
        };

        if removed == 0 {
            // The oldest-row probe and the delete disagree only under a concurrent write, and
            // a delete that removed nothing has no receipt to file.
            continue;
        }

        removed_total += removed;
        announce(state, &window, removed).await;
        tracing::info!(
            organization_id = %window.organization_id,
            retention_days = window.retention_days,
            rows_removed = removed,
            "audit retention swept one organization"
        );
    }

    Ok(removed_total)
}

/// One organization's window and the instant it closed.
struct Window {
    /// Tenant the window belongs to.
    organization_id: Uuid,
    /// The stored window, in days.
    retention_days: i32,
    /// Rows older than this are outside it.
    cutoff: OffsetDateTime,
}

/// The tenants whose window has closed on at least one row, oldest first.
///
/// Two statements rather than a join over every row: the candidate list is small (tenants with
/// something to purge, capped at the batch), and the per-tenant probe is what makes the
/// per-tenant window authoritative. Ordering by the oldest expired row means a tick that hits
/// its batch cap works on the tenants that have the most history waiting, and the rest follow
/// on the next tick rather than being starved forever.
async fn due_windows(
    state: &AppState,
    now: OffsetDateTime,
) -> Result<Vec<Window>, omnion_audit::AuditError> {
    let batch = state.config().audit_retention.sweep_batch as i64;

    // A tenant with no settings row still gets the default window, and the schema default is
    // the same 365 days `load_settings` falls back to. The `left join` is what keeps such a
    // tenant from being invisible to the sweep.
    let candidates: Vec<(Uuid, Option<i32>, Option<OffsetDateTime>)> = sqlx::query_as(
        "select o.id, s.audit_retention_days, \
                (select min(a.created_at) from audit_log a where a.organization_id = o.id) as oldest \
         from organizations o \
         left join organization_settings s on s.organization_id = o.id \
         order by oldest asc nulls last, o.id asc \
         limit $1",
    )
    .bind(batch)
    .fetch_all(state.db().pool())
    .await?;

    let mut windows = Vec::new();
    for (organization_id, stored_days, oldest) in candidates {
        let retention_days = stored_days.unwrap_or(DEFAULT_RETENTION_DAYS);
        let Some(oldest) = oldest else {
            // A tenant with no audit rows has nothing to purge, which is not a failure.
            continue;
        };

        let cutoff = now - time::Duration::days(i64::from(retention_days));
        if oldest >= cutoff {
            continue;
        }

        windows.push(Window {
            organization_id,
            retention_days,
            cutoff,
        });
    }

    Ok(windows)
}

/// The window a tenant without a settings row is held to.
///
/// Kept next to the sweep rather than imported from the identity crate's migration so the
/// fallback is one line to read next to the `unwrap_or` that uses it — and so the sweep does
/// not depend on a *load* path to know what an absent row means.
const DEFAULT_RETENTION_DAYS: i32 = 365;

/// File the system audit row and announce the sweep on the bus.
///
/// The audit row goes first: the event bus is a convenience for live consumers, while the trail
/// is the durable record. A failure of the bus is a warning, not a failed sweep — the rows are
/// already gone, and re-running the delete would not bring back a receipt we could not write.
async fn announce(state: &AppState, window: &Window, removed: i64) {
    let sweep = RetentionSweep {
        organization_id: window.organization_id,
        retention_days: window.retention_days,
        cutoff: window.cutoff,
        rows_removed: removed,
    };

    if let Err(error) = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::system("organization.retention.swept")
            .target("organization", sweep.organization_id.to_string())
            .organization(sweep.organization_id)
            .metadata(serde_json::json!({
                "retention_days": sweep.retention_days,
                "rows_removed": sweep.rows_removed,
                "cutoff": sweep.cutoff,
            })),
    )
    .await
    {
        tracing::warn!(
            organization_id = %sweep.organization_id,
            error = %error,
            "the retention sweep could not file its audit row"
        );
        return;
    }

    let emission = bus::emit(
        state.db().pool(),
        NewEvent::new("organization.retention.swept")
            .organization(sweep.organization_id)
            .payload(serde_json::json!({
                "retention_days": sweep.retention_days,
                "rows_removed": sweep.rows_removed,
                "cutoff": sweep.cutoff,
            })),
    )
    .await;

    if let Err(error) = emission {
        tracing::warn!(
            organization_id = %sweep.organization_id,
            error = %error,
            "the retention sweep event could not be recorded"
        );
    }
}

#[cfg(test)]
mod tests {
    use omnion_core::config::AuditRetentionConfig;

    #[test]
    fn the_sweep_is_on_by_default() {
        // Retention that only runs when somebody remembers is not retention. A default of
        // `false` would make the whole feature a lie on a stock installation.
        let defaults = AuditRetentionConfig::default();
        assert!(
            defaults.sweep_enabled,
            "the sweep must be on without configuration"
        );
    }

    #[test]
    fn the_default_cadence_is_longer_than_the_shortest_window_a_tenant_can_ask_for() {
        let defaults = AuditRetentionConfig::default();
        assert!(
            defaults.sweep_seconds >= 60 * 60,
            "a minute-cadence sweep would re-read every tenant 1440 times a day to remove \
             rows that are at least 30 days old"
        );
        assert_eq!(defaults.sweep_batch, 200);
    }
}
