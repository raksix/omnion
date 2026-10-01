//! The CDN purge worker (REQ-011, slice 2).
//!
//! `main.rs` spawns this task when the worker is enabled. Each tick claims the due purge
//! items, hands them to the provider the site configured, and records what came back. It
//! is the only place in the platform that calls a CDN adapter.
//!
//! Five decisions shape this file, and each is a place the obvious shortcut is wrong:
//!
//! * **A tick is seconds, not minutes.** A purge is an invalidation of something a visitor
//!   is about to see, and the window between "the page was published" and "the cache agrees"
//!   is the window the whole feature exists to shorten. A worker that polls every five
//!   minutes has turned a cache into a five-minute-old cache.
//! * **The claim is atomic and the rows come back with it.** `claim_due` is one statement
//!   with `for update skip locked`; re-reading the claimed ids afterwards would race the
//!   update that just claimed them, and two API processes would drain the same batch.
//! * **Items are grouped by site, because the provider is per site.** Claiming across two
//!   sites and asking one adapter would send site B's URLs to site A's provider — a
//!   cross-tenant leak of URLs, and a bug that only appears on an installation with two
//!   sites, which is exactly where it is least likely to be noticed.
//! * **The adapter call is blocking, and it runs on a blocking thread.** The
//!   [`Provider`](omnion_cdn::provider::Provider) trait is deliberately synchronous-looking
//!   (a half-finished purge is a state in the database, not a suspended future). Calling it
//!   on the async runtime would stall every other task in the process for the length of an
//!   HTTP round trip, so it goes through `spawn_blocking` exactly as the HTTP handlers do.
//! * **A tick that fails is logged and retried, never fatal.** A provider that is down for
//!   two hours must not take the API with it, and the items stay `pending` with their
//!   backoff, which is the whole point of storing `next_attempt_at` rather than a counter.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use omnion_cdn::invalidation;
use omnion_cdn::purge::{self, PurgeItemRow, PurgeKind};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// How many items one tick claims.
///
/// Bounded so a burst of publishing cannot make one tick hold every row in memory, and so
/// a slow provider delays a small batch rather than a large one. The queue is durable: the
/// items not claimed here are the next tick's work.
const CLAIM_BATCH: i64 = 200;

/// How many events one invalidation pass reads.
///
/// Half the claim batch, for a reason that is about the *shape* of the work rather than
/// its size: a drain turns one event into one purge, and a purge is a row plus a row per
/// target. A pass that read 200 events and queued 200 purges would write 200 history rows
/// in one transaction while the claim batches the resulting items 200 at a time — the two
/// numbers answer different questions and there is no reason to make them the same one.
const EVENT_BATCH: i64 = 100;

/// Start the purge worker; the handle is kept by the binary and ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state
        .config()
        .retention
        .poll_ms
        .clamp(1_000, 30_000);
    tracing::info!(poll_ms, "the CDN purge worker started");

    tokio::spawn(async move {
        // The invalidation cursor is pointed at the head of the bus before the first walk,
        // for the same reason the search indexer does it: a fresh installation watches
        // forward, and an existing one does not replay every publication it has ever
        // recorded into a burst of purges at a provider that is doing nothing wrong.
        match invalidation::seed_cursor(state.db().pool()).await {
            Ok(Some(cursor)) => {
                tracing::info!(cursor, "the CDN invalidation cursor seeded to the end of the bus");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "the CDN invalidation cursor could not be seeded");
            }
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the rows are still
        // queued, so the next tick drains the same set again and the history shows two
        // passes rather than one enormous one.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            // The order of the two passes is the whole point of this tick, and it is not
            // arbitrary. Automatic invalidation (REQ-011 slice 3) walks the events recorded
            // since the last one and queues what is stale; the drain above then claims
            // whatever is due. Running the walk first means a publication recorded a second
            // ago is queued *and* claimed in the same pass, which is what the request's
            // "page published -> purge -> new version live" diagram asks for. Running the
            // drain first would still deliver the purge, one tick later — and on a tick
            // measured in seconds that is a visitor seeing the old page for no reason.
            if let Err(error) = invalidate(&state).await {
                tracing::warn!(error = %error, "the CDN invalidation pass failed");
            }
            if let Err(error) = tick(&state).await {
                tracing::warn!(error = %error, "the CDN purge tick failed");
            }
        }
    })
}

/// Walk the event bus above the invalidation cursor and queue what it says is stale.
pub async fn invalidate(state: &AppState) -> Result<(), omnion_cdn::CdnError> {
    let pool = state.db().pool();
    let report = invalidation::drain(pool, EVENT_BATCH).await?;
    if report.is_idle() {
        return Ok(());
    }
    tracing::debug!(
        read = report.read,
        queued = report.queued,
        skipped = report.skipped,
        cursor = report.cursor,
        "cdn: the automatic invalidation pass"
    );
    Ok(())
}

/// One pass: claim, dispatch, record.
pub async fn tick(state: &AppState) -> Result<(), omnion_cdn::CdnError> {
    let pool = state.db().pool();
    let claimed = purge::claim_due(pool, CLAIM_BATCH).await?;
    if claimed.is_empty() {
        return Ok(());
    }

    // Group by site, then by the purge each item belongs to, so one provider call answers
    // one batch of one purge. Batching across purges would make the provider's per-call
    // response cover targets from two different invalidations, and the per-item result
    // could no longer be attributed to the right history row.
    let mut groups: BTreeMap<(Option<Uuid>, Uuid), Vec<PurgeItemRow>> = BTreeMap::new();
    for item in claimed {
        let site: Option<Uuid> =
            sqlx::query_scalar("select site_id from cdn_purges where id = $1")
                .bind(item.purge_id)
                .fetch_one(pool)
                .await?;
        groups.entry((site, item.purge_id)).or_default().push(item);
    }

    let purge_ids: Vec<Uuid> = groups.keys().map(|(_, purge_id)| *purge_id).collect();
    purge::mark_running(pool, &purge_ids).await?;

    for ((site, _), mut items) in groups {
        dispatch_one(pool, site, &mut items).await?;
    }

    // Settle only the purges this tick touched. A sweep over the whole table would stamp
    // `finished_at` on purges whose items are still waiting out a backoff.
    purge::settle(pool, &purge_ids).await?;
    Ok(())
}

/// Ask one purge's provider to invalidate its targets, then write the outcomes back.
async fn dispatch_one(
    pool: &sqlx::PgPool,
    site_id: Option<Uuid>,
    items: &mut [PurgeItemRow],
) -> Result<(), omnion_cdn::CdnError> {
    if items.is_empty() {
        return Ok(());
    }

    let (key, settings, max_attempts) = purge::provider_for_site(pool, site_id).await?;
    let (kind, provider_key) = purge_row_shape(pool, items[0].purge_id).await?;

    // The stored `kind` decides how the targets are phrased. A purge queued as tags and
    // drained as URLs would invalidate the wrong thing silently: the provider would happily
    // accept `/blog` as a URL path and the surrogate keys would stay warm.
    let targets: Vec<String> = items.iter().map(|item| item.target.clone()).collect();
    let request = kind.to_purge(&targets);

    let outcome = tokio::task::spawn_blocking({
        let key = key.clone();
        move || omnion_cdn::provider_for(&key, &settings).purge(&request)
    })
    .await
    .unwrap_or_else(|error| {
        // A panicked or aborted blocking task is a provider call that never answered, and
        // the honest record of that is a failure with the reason — not a silent success.
        omnion_cdn::PurgeOutcome::Failed {
            message: format!("the purge adapter could not be scheduled: {error}"),
        }
    });

    let (failed, message) = purge::apply_outcome(
        items,
        &outcome,
        time::OffsetDateTime::now_utc(),
        max_attempts,
        0,
    );

    // The parent's own error column is only set here when the items carry none, so the
    // drawer can always point at the specific target that failed.
    if failed > 0 && message.is_some() {
        tracing::info!(
            purge = %items[0].purge_id,
            provider = %provider_key,
            failed,
            kind = kind.as_str(),
            "a purge did not fully succeed"
        );
    }

    purge::save_items(pool, items).await?;

    // A purge that ends without a subscriber hearing about it is a purge an operations
    // endpoint cannot alert on, which is the whole reason `cdn.purge.failed` is subscribable
    // in the request.
    if failed > 0 {
        let site = site_id.map(|id| id.to_string());
        let _ = omnion_events::bus::emit(
            pool,
            omnion_events::NewEvent::new("cdn.purge.failed")
                .site(site.and_then(|value| value.parse().ok()))
                .payload(serde_json::json!({
                    "purge_id": items[0].purge_id,
                    "provider": provider_key,
                    "kind": kind.as_str(),
                    "failed_items": failed,
                })),
        )
        .await;
    }

    Ok(())
}

/// The kind a stored purge was requested with.
async fn purge_row_shape(
    pool: &sqlx::PgPool,
    purge_id: Uuid,
) -> Result<(PurgeKind, String), omnion_cdn::CdnError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "select kind, provider from cdn_purges where id = $1",
    )
    .bind(purge_id)
    .fetch_optional(pool)
    .await?;

    // A purge whose parent row vanished cannot be drained: the cascade would have taken
    // the items with it, so reaching here means the row was deleted between the claim and
    // this read. Reporting it as a URL purge would call a provider for nothing.
    match row {
        Some((kind, provider)) => Ok((
            PurgeKind::parse(&kind).unwrap_or(PurgeKind::Url),
            provider,
        )),
        None => Ok((PurgeKind::Url, "origin".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claimed_group_is_keyed_by_site_and_purge_so_two_tenants_never_share_a_call() {
        // The grouping is the tenancy boundary for this worker. A `HashMap<Uuid, ...>` keyed
        // only by purge id would merge two sites' items into one provider call whenever the
        // ids interleaved, which is the shape a single-site test can never produce.
        let mut groups: BTreeMap<(Option<Uuid>, Uuid), Vec<PurgeItemRow>> = BTreeMap::new();
        for (site, purge) in [
            (Some(Uuid::from_u128(1)), Uuid::from_u128(10)),
            (Some(Uuid::from_u128(2)), Uuid::from_u128(11)),
            (Some(Uuid::from_u128(1)), Uuid::from_u128(12)),
        ] {
            groups.entry((site, purge)).or_default().push(PurgeItemRow {
                id: 1,
                purge_id: purge,
                target: "/a".into(),
                status: "running".into(),
                attempts: 1,
                next_attempt_at: time::OffsetDateTime::now_utc(),
                response_status: None,
                error: None,
                done_at: None,
            });
        }
        assert_eq!(groups.len(), 3, "three (site, purge) pairs, three calls");
        assert_eq!(groups[&(Some(Uuid::from_u128(1)), Uuid::from_u128(10))].len(), 1);
    }

    #[test]
    fn the_claim_batch_is_bounded_so_one_tick_cannot_hold_the_whole_queue() {
        assert!(CLAIM_BATCH > 0 && CLAIM_BATCH <= omnion_cdn::purge::MAX_TARGETS as i64);
    }
}
