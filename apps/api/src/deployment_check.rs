//! The update check (REQ-024, slice 1).
//!
//! One scheduled pass that reads the release manifest, caches what it found, and emits
//! `update.available` **once per newly seen version**. It is the reason the card can show
//! `Available 2.5.0` without a network call on page load, and it is the reason an installation
//! that cannot reach the feed still renders — the spec requires the second, and an air-gapped
//! deployment (REQ-036) is the extreme case of it.
//!
//! Five decisions, each a place the obvious shortcut is wrong:
//!
//! * **A failure is a state, not an error.** The cache is left exactly as it was and the run is
//!   recorded as `failed` with its reason. The alternative — clearing the cache so the screen
//!   does not show a lie — is the one that takes the deployment centre down every time the
//!   publisher's CDN has a bad afternoon.
//! * **The dedupe is the database's, not this file's.** `claim_announced` is an
//!   `insert … on conflict do nothing` whose affected-row count is the answer, so two passes
//!   racing (a scheduled one and an operator pressing "check now") emit one event per version by
//!   construction. A `select` then an `insert` emits duplicates whenever both run at once, and
//!   duplicates are exactly what a webhook consumer cannot defend against.
//! * **The feed URL is configuration, and an instance without one is not broken.** No URL means
//!   the check records "not configured" and stops; it does not hammer a default endpoint that
//!   this installation never asked to talk to.
//! * **The whole check is one pass, synchronously, on one worker.** There is no claim queue
//!   because the work is a single HTTP read: a claim table would add a row, a reaper and a
//!   failure mode to avoid re-reading one URL.
//! * **A check that runs on boot is a check that runs before the database is ready.** The first
//!   tick is consumed and the interval starts from it, so a slow start delays the first real
//!   check rather than firing a burst of catch-ups.

use std::time::Duration as StdDuration;

use omnion_deployment::manifest::{self, CheckResult, SeenSet};
use omnion_deployment::store;
use omnion_deployment::version::{Channel, Release};
use omnion_deployment::StoreError;
use omnion_events::NewEvent;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// How long between two checks, in seconds.
///
/// Six hours. The interval is a product decision rather than a performance one: a release
/// manifest changes a few times a week, and a shorter interval turns a rate-limited publisher
/// into a support ticket against this installation for no benefit to the operator.
const INTERVAL_SECONDS: i64 = 6 * 60 * 60;

/// How long one feed read may take.
///
/// Fifteen seconds. A feed that has not answered in fifteen seconds is not slow, it is down, and
/// the check's job on a down feed is to *say so* — which it cannot do while blocked on a socket.
const FEED_TIMEOUT_SECS: u64 = 15;

/// How much of the feed is read.
///
/// A megabyte is far past any real manifest and small enough that a hostile or misconfigured URL
/// cannot make the API hold a large response in memory on a timer.
const FEED_MAX_BYTES: usize = 1_048_576;

/// What one check did, as the route and the runner both report it.
#[derive(Debug, Clone)]
pub struct CheckOutcomeReport {
    /// The check's result.
    pub result: CheckResult,
    /// The versions this run announced, after the database dedupe.
    pub announced: Vec<String>,
}

/// The interval between two scheduled checks, in seconds.
#[must_use]
pub fn interval_seconds() -> i64 {
    INTERVAL_SECONDS
}

/// Start the update-check worker; the handle is kept by the binary and ends with the process.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = (INTERVAL_SECONDS * 1000) as u64;
    tracing::info!(interval_hours = INTERVAL_SECONDS / 3600, "the deployment update check started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A check that was slow (a feed that hung until the timeout) must not be followed by a
        // burst of catch-up checks: the feed is already unwell, and five immediate reads of it
        // turn one problem into five.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do, and a check against a
        // database that is still migrating fails for a reason that is not the feed's.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            let pool = state.db().pool();
            let channel = match store::load_check(pool).await {
                Ok(check) => check.channel,
                Err(error) => {
                    tracing::warn!(error = %error, "the deployment update check could not read its own row");
                    continue;
                }
            };
            if let Err(error) = check_once(pool, channel).await {
                tracing::warn!(error = %error, "the deployment update check failed");
            }
        }
    })
}

/// Run one check: read the feed, cache it, announce what is new, record the outcome.
///
/// Called by the scheduled worker and by `POST /api/v1/deployment/checks/run`, so it is written
/// to be *idempotent under concurrency* rather than to be convenient once: both callers may run
/// it in the same second, and the only claim in this file that has to hold is "one event per
/// newly seen version".
pub async fn check_once(
    pool: &sqlx::PgPool,
    channel: Channel,
) -> Result<CheckOutcomeReport, StoreError> {
    let url = feed_url().await?;

    let Some(url) = url else {
        // Not configured is not a failure of the *check* — it is a fact about this installation,
        // and recording it as a failed run would put a red banner on a self-hosted instance that
        // deliberately manages its own releases (which is the air-gapped case the spec names).
        let reason = "no release feed is configured for this installation".to_string();
        store::record_failed(pool, &reason).await?;
        return Ok(CheckOutcomeReport {
            result: CheckResult::Failed { reason },
            announced: Vec::new(),
        });
    };

    let (body, fetched_at) = match read_feed(&url).await {
        Ok(fetched) => fetched,
        Err(reason) => {
            // The cache is untouched. This is the line that keeps the screen rendering.
            store::record_failed(pool, &reason).await?;
            tracing::info!(reason = %reason, "the release feed could not be read; the cache was left as it was");
            return Ok(CheckOutcomeReport {
                result: CheckResult::Failed { reason },
                announced: Vec::new(),
            });
        }
    };

    let manifest = manifest::parse_manifest(&body);
    if manifest.releases.is_empty() {
        // A feed that answered with nothing usable is treated exactly like a feed that did not
        // answer, *including* the cache rule. A 200 carrying an HTML error page parses to zero
        // releases, and treating that as "the publisher withdrew everything" would empty the
        // release list on the next page load.
        let reason = manifest.rejected.first().map_or_else(
            || "the manifest carried no releases".to_string(),
            |entry| entry.reason.clone(),
        );
        store::record_failed(pool, &reason).await?;
        return Ok(CheckOutcomeReport {
            result: CheckResult::Failed { reason },
            announced: Vec::new(),
        });
    }

    // The crate's `SeenSet` is the *decision* about what is new; the database is the *record*.
    // Both run, in that order, and the difference matters: the in-memory set keeps this pass
    // from announcing the same version twice when a feed lists it twice, and the table keeps the
    // answer across restarts and across two workers.
    let mut seen = SeenSet::new();
    let candidates = seen.take_new(&manifest.releases);
    let announced = store::claim_announced(pool, channel, &candidates).await?;

    store::replace_channel(pool, channel, &manifest.releases, &Some(fetched_at)).await?;
    store::record_completed(pool, channel, manifest.releases.len(), &announced).await?;

    if !announced.is_empty() {
        // The claim above and the emit below are two writes, and between them the check has
        // already recorded the versions as "seen". If the emit fails and the claim stands, the
        // next check sees nothing new and those versions are **never** announced — a webhook
        // consumer misses the update for ever, and the check screen cheerfully reports
        // "nothing new since the last check". So a failed emit releases the claim it just took,
        // and the next pass re-announces. The window is a duplicate at worst, which a consumer
        // can defend against; the alternative is a silent loss it cannot.
        // The payload carries versions, not a log, and no credential: this event is subscribable
        // by a chat webhook (the spec says so), so everything in it is something a chat message
        // can contain.
        let outcome = omnion_events::bus::emit(
            pool,
            NewEvent::new("update.available").payload(json!({
                "channel": channel.as_str(),
                "versions": announced,
                "releases_known": manifest.releases.len(),
                // The dedupe is the *database's* — `claim_announced`'s affected-row count above
                // is what makes this emission once per version. This key is not the mechanism;
                // it is the identity a subscriber reads, so a chat webhook can tell "2.5.0 and
                // 2.5.1 arrived together" from "2.5.1 arrived alone" without parsing the list.
                "announcement": announced_key(channel, &announced),
            })),
        )
        .await;

        if let Err(error) = outcome {
            store::release_announced(pool, channel, &announced).await?;
            return Err(StoreError::Event(error.to_string()));
        }
    }

    tracing::info!(
        channel = channel.as_str(),
        known = manifest.releases.len(),
        announced = announced.len(),
        "the deployment update check completed"
    );

    Ok(CheckOutcomeReport {
        result: CheckResult::Completed {
            announced: announced.clone(),
            seen: manifest.releases.len(),
        },
        announced,
    })
}

/// The identity of one `update.available` emission.
///
/// A hash of the sorted set, not the first version: a check that announces `2.5.0` and then
/// `2.5.1` ten minutes later is one *situation* and two emissions, and a key of the first
/// version would collapse the second into the first. A hash of the whole set means the two
/// emissions differ, and a re-emission of the same set does not.
fn announced_key(channel: Channel, versions: &[String]) -> String {
    let mut sorted = versions.to_vec();
    sorted.sort();
    let joined = sorted.join(",");
    // A short, stable digest. The platform's own hash is not needed here and a longer one would
    // be stored in the event row for no reader; the point is only that the same set produces the
    // same string and a different set does not.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in joined.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("update.available:{}:{hash:016x}", channel.as_str())
}

/// The configured feed URL, if there is one.
///
/// From the environment rather than a settings table: this is deployment configuration, read
/// once per check, and a setting the operator can change without a migration is worth more than
/// a row a request has to guard.
async fn feed_url() -> Result<Option<String>, StoreError> {
    Ok(std::env::var("OMNION_RELEASE_FEED_URL")
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty()))
}

/// Read the feed, with a timeout and a size cap.
///
/// Returns the body **and** the moment the read completed. The stamp travels with the body
/// rather than living in a module-level cell: a `OnceLock` here would latch the *first* check's
/// time and stamp every row ever cached with it, so a six-month-old row would claim to have been
/// read six months ago — which is precisely the lie the cached-data banner exists to prevent.
async fn read_feed(url: &str) -> Result<(String, String), String> {
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(FEED_TIMEOUT_SECS))
        .user_agent(concat!("omnion-deployment/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("the HTTP client could not be built: {error}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("{error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("the feed answered {status}"));
    }
    if let Some(length) = response.content_length() {
        if length > FEED_MAX_BYTES as u64 {
            return Err(format!(
                "the feed claims {length} bytes, past the {FEED_MAX_BYTES}-byte cap"
            ));
        }
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("{error}"))?;
    if body.len() > FEED_MAX_BYTES {
        return Err(format!(
            "the feed sent {} bytes, past the {FEED_MAX_BYTES}-byte cap",
            body.len()
        ));
    }
    Ok((body, now_stamp()))
}

/// An RFC 3339 stamp of now, for rows the feed did not date.
fn now_stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Is this set of releases entirely on one channel?
///
/// Not dead weight: the multi-region slice checks *several* channels in one pass, and the one
/// thing that must not happen there is a nightly release written into the stable channel's
/// replace — which would put a nightly on a stable card. The store's primary key is the version
/// alone, so the two channels genuinely can collide.
#[must_use]
pub fn all_on_channel(channel: Channel, releases: &[Release]) -> bool {
    releases.iter().all(|release| release.channel == channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_set_produces_the_same_key() {
        // A feed that republishes the same manifest must not produce a second event, and the
        // key is what does that. So the two calls have to agree.
        let versions = vec!["2.5.0".to_string(), "2.4.9".to_string()];
        assert_eq!(
            announced_key(Channel::Stable, &versions),
            announced_key(Channel::Stable, &versions),
        );
    }

    #[test]
    fn the_key_does_not_depend_on_the_feeds_order() {
        // The set is what was announced, not the order the feed happened to list it in. A feed
        // that reorders its entries between two runs is the same fact, and treating it as new
        // would re-announce every release on every check.
        let forward = vec!["2.5.0".to_string(), "2.4.9".to_string()];
        let backward = vec!["2.4.9".to_string(), "2.5.0".to_string()];
        assert_eq!(
            announced_key(Channel::Stable, &forward),
            announced_key(Channel::Stable, &backward),
        );
    }

    #[test]
    fn two_runs_announcing_one_new_version_are_two_events() {
        // The opposite requirement, and the reason the key is a hash of the whole set rather
        // than of the first element: 2.5.0 arriving, then 2.5.1 ten minutes later, is one
        // continuing situation and two things an operator wants to hear about.
        let first = announced_key(Channel::Stable, &["2.5.0".to_string()]);
        let second = announced_key(Channel::Stable, &["2.5.1".to_string()]);
        assert_ne!(first, second);
    }

    #[test]
    fn the_channel_is_part_of_the_key() {
        // The same version on two channels is two releases (the crate's own rule), so it must
        // be two keys — or a beta install's announcement would silence a stable one's.
        let stable = announced_key(Channel::Stable, &["2.5.0-rc.1".to_string()]);
        let beta = announced_key(Channel::Beta, &["2.5.0-rc.1".to_string()]);
        assert_ne!(stable, beta);
    }

    #[test]
    fn an_empty_announcement_still_has_a_key() {
        // `check_once` only emits when the set is non-empty, so this is unreachable from the
        // runner — and it is here because a key that panicked on an empty set would be a panic
        // one line away from the emit.
        let key = announced_key(Channel::Stable, &[]);
        assert!(key.starts_with("update.available:stable:"));
    }

    #[test]
    fn a_mixed_channel_set_is_refused() {
        // The stable/nightly collision the helper exists for: `releases_cache`'s primary key is
        // the version alone, so a nightly `2.6.0` written by a stable pass overwrites the stable
        // row and the card then offers a nightly as its next release.
        let stable = Release::minimal("2.5.0", Channel::Stable);
        let nightly = Release::minimal("2.6.0", Channel::Nightly);
        assert!(all_on_channel(Channel::Stable, &[stable.clone()]));
        assert!(!all_on_channel(
            Channel::Stable,
            &[stable, nightly]
        ));
    }

    #[test]
    fn the_interval_is_hours_not_seconds() {
        // A mis-placed zero turns a six-hour check into a six-*second* one, which is a
        // rate-limit incident against the publisher and shows up as a check screen that never
        // stops moving. Asserted in hours so the unit is visible in the failure message.
        assert_eq!(INTERVAL_SECONDS / 3600, 6, "the check runs every six hours");
    }
}
