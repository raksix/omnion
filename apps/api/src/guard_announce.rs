//! Announcing exemption lapses — the `ai.guard.exemption.expired` event (REQ-105).
//!
//! # Why this is a module and not a line in the checkpoint
//!
//! Two different questions are involved and they run on different clocks. "Is this exemption
//! still in force?" is read *per request*, because the answer has to be true for the request
//! being answered right now. "Has this lapse been announced?" is read *once per lapse*, because
//! a lapse is a historical fact that stays true forever. Putting the announcement inside the
//! per-request read would announce the same event on every request that followed the expiry —
//! and, worse, would make the number of announcements a function of how much traffic the tenant
//! sends rather than of how many exemptions actually expired.
//!
//! # Why it runs off the checkpoint and not off a scheduler
//!
//! There is no scheduler here on purpose. This project has a runner for agents and one for
//! workflow deliveries, and a third idea — "a cron job that expires things" — would be the
//! obvious home for a lapse sweep. It would also be a second place that has to be configured,
//! deployed and monitored before a single expiry is ever announced, and a control whose
//! announcement depends on a background process being alive is a control that silently stops
//! reporting when that process dies. A sweep driven by traffic has the same coverage for any
//! installation that is actually using the guard (an installation with no traffic has no lapses
//! worth announcing either) and needs nothing deployed.
//!
//! The cost of that choice is honest and worth stating: the announcement is late by up to one
//! request. A lapse takes effect at `expires_at` (read fresh, below), and the *announcement* of
//! it lands when the tenant next calls a provider. Nothing about the request's outcome depends on
//! the announcement, so being late cannot make the guard permissive.
//!
//! # The watermark
//!
//! `since` is a lookback, not a stored cursor: the sweep announces every lapse in the window and
//! the marker makes each announcement happen once, so re-announcing a window that was already
//! covered costs a read and nothing else. A stored cursor would need a place to live per
//! organization and would be wrong the first time a clock moved backwards; a fixed lookback
//! cannot be wrong at all, at the price of re-reading a bounded window. The window is 24 hours,
//! which is far longer than any plausible gap between requests and far shorter than any exemption
//! an operator sets.
use std::time::Duration;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_ai_hub::guard_store;

/// How far back one sweep looks for lapses it may not have announced yet.
const LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);

/// Announce every exemption lapse this tenant has not announced yet.
///
/// Returns how many events were emitted. Called from the chat checkpoint's path, so it runs on
/// traffic — see the module docs for why that is the right trigger and what it costs.
///
/// Errors are swallowed by the caller's contract rather than here: a failure to *announce* is
/// not a failure of the request, and the checkpoint must not turn a bookkeeping problem into a
/// refused chat. What a failure costs is bounded and small — the next sweep retries the same
/// window, because the marker was never written.
pub async fn announce_lapsed_exemptions(pool: &PgPool, organization_id: Uuid) -> u64 {
    if organization_id.is_nil() {
        // An account with no organization reads the platform rules only and can never hold a
        // tenant exemption, so there is nothing to announce. Returning early also keeps the nil
        // id out of the store's `organization_id = $1` read, which is what a nil row would
        // otherwise have to be distinguished from.
        return 0;
    }

    let since = OffsetDateTime::now_utc() - LOOKBACK;
    let rows = match guard_store::lapsed_exemptions(pool, organization_id, since).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                %error,
                %organization_id,
                "the data guard could not read lapsed exemptions; the announcement is deferred"
            );
            return 0;
        }
    };

    let mut announced = 0_u64;
    for row in rows {
        // The claim is the de-duplication. A caller that loses it says nothing, so the event is
        // emitted by exactly one of the requests that raced for it.
        match guard_store::claim_exemption_announcement(pool, organization_id, &row).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                // One unreadable row must not abandon the rest: the loop continues and this one
                // is retried by the next sweep, which is the same cost as a read failure.
                tracing::warn!(
                    %error,
                    exemption_id = %row.id,
                    "a lapsed exemption could not be claimed for announcement"
                );
                continue;
            }
        }

        let event = omnion_events::NewEvent::new("ai.guard.exemption.expired")
            .organization(organization_id)
            .payload(serde_json::json!({
                "exemption_id": row.id,
                "label": row.label,
                "providers": provider_list(&row.providers),
                "features": feature_list(&row.features),
                "reason": row.reason,
                "created_by": row.created_by,
                "expires_at": row.expires_at.map(|at| at.unix_timestamp()),
            }));
        if let Err(error) = omnion_events::bus::emit(pool, event).await {
            // The marker is already written, so this lapse will NOT be retried — the event is
            // lost. That is a deliberate trade rather than an oversight: the alternative is to
            // roll the marker back and let every request since the expiry try again, turning one
            // failing webhook fan-out into a permanent retry storm on a chat endpoint. A lost
            // announcement is visible in the log; a storm is not. The tradeoff is named here so
            // the next writer does not read the marker and assume exactly-once delivery.
            tracing::warn!(
                %error,
                exemption_id = %row.id,
                "an exemption-lapse event was claimed but could not be published; \
                 it will not be retried"
            );
        } else {
            announced += 1;
        }
    }
    announced
}

/// The provider scope an exemption applied to, for the event payload.
fn provider_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The feature scope an exemption applied to, for the event payload.
fn feature_list(value: &serde_json::Value) -> Vec<String> {
    provider_list(value)
}
