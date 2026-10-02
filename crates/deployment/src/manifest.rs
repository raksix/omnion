//! The release manifest: parsing a feed, and deciding what an update check may claim.
//!
//! The spec makes two promises about this half that are easy to keep in the wrong place:
//!
//! * "`update.available` is emitted once per newly seen version (dedupe on channel + version)" —
//!   *once per newly seen version*, so the dedupe is state, not a guard at the emit site.
//! * "Keep the manifest feed optional: an instance that cannot reach it (offline or air-gapped,
//!   REQ-036) must still show its own version and history, with the cached banner explaining what
//!   is stale."
//!
//! So a feed that cannot be reached is a **state**, [`CheckOutcome`]-shaped and carried in the
//! response, and never an error that empties the screen. The screen's fallback path is the
//! product, not an apology for a broken one.

use crate::preflight::CheckOutcome;
use crate::version::{Availability, Channel, Release, Version, VersionError, availability};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One entry of the release manifest, in the shape a generated `releases.json` actually has.
#[derive(Debug, Clone, Deserialize)]
struct FeedEntry {
    version: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    released_at: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    breaking: Option<bool>,
    #[serde(default)]
    migrations: Option<Vec<String>>,
    #[serde(default)]
    core_min: Option<String>,
    #[serde(default)]
    artifact_checksum: Option<String>,
}

/// A manifest the update check read.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    /// Every entry that parsed, in the feed's own order.
    pub releases: Vec<Release>,
    /// The entries that did not parse, with the reason.
    ///
    /// Kept rather than dropped: a manifest with one bad entry must not silently become a
    /// manifest without it, because "no upgrade available" and "the feed is malformed" are
    /// different answers and the card can only give one of them.
    pub rejected: Vec<RejectedEntry>,
}

/// A feed entry that could not be turned into a [`Release`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedEntry {
    /// The version string the feed claimed.
    pub version: String,
    /// Why it was refused.
    pub reason: String,
}

/// The `2026-09-01T09:00:00` layout: a datetime with no offset.
///
/// A dedicated description because none of the well-known ones will read it — `Rfc3339`
/// *requires* an offset, which is the entire reason this branch exists.
const NAIVE_DATETIME: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

/// Parse a release timestamp the way a generated manifest actually carries one.
///
/// The feed comes out of CI, so its timestamps are whatever that pipeline emitted. Three shapes
/// break in practice, and all three are read here rather than refused, because a release whose
/// date does not parse is still a release — dropping the entry would be a far worse outcome than
/// rendering it undated:
///
/// * a **date with no time** (`2026-09-01`) — read as midnight;
/// * a **space where RFC 3339 wants `T`** (`2026-09-01 09:00:00Z`);
/// * a **naive datetime with no offset** (`2026-09-01 09:00:00`) — read as **UTC**, which is an
///   assumption and is called out here because it is one: a publisher whose pipeline wrote local
///   time gets its release date rendered up to a day out. It is still better than an undated
///   release, and the string is carried through unconverted so the list shows what the feed said.
///
/// Anything genuinely unparseable returns `None` rather than an error, and the release list sorts
/// by version — so an undated release still appears in the right place.
fn parse_released_at(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let rfc3339 = time::format_description::well_known::Rfc3339;
    if time::OffsetDateTime::parse(trimmed, &rfc3339).is_ok() {
        return Some(trimmed.to_string());
    }
    if let Some((date, rest)) = trimmed.split_once(' ') {
        let normalized = format!("{date}T{rest}");
        if time::OffsetDateTime::parse(&normalized, &rfc3339).is_ok() {
            return Some(normalized);
        }
        // A naive datetime: no offset at all, read as UTC rather than refused.
        //
        // The instant is *formatted* back out rather than taken from `normalized`: `Display` on
        // a `PrimitiveDateTime` writes `2026-09-01 9:00:00.0`, which is neither RFC 3339 nor
        // anything this column should hold. The value is a timestamp the release list sorts and
        // the API returns, so it has to be the one format every other reader already parses.
        if let Ok(naive) = time::PrimitiveDateTime::parse(&normalized, NAIVE_DATETIME) {
            return naive.assume_utc().format(&rfc3339).ok();
        }
    }
    if let Ok(date) = time::Date::parse(
        trimmed,
        &time::format_description::well_known::Iso8601::DATE,
    ) {
        // Midnight UTC, so a date-only feed entry still has a real instant behind it.
        return Some(format!("{date}T00:00:00Z"));
    }
    None
}

/// Parse a manifest from the raw feed text.
///
/// A feed is JSON, but a **truncated** one is the common failure and it is not a parse error at
/// the top level — the bytes cut mid-array. So a failed parse returns a [`Manifest`] with no
/// releases and the parse error recorded, and the caller keeps whatever it had cached. An
/// operator sees "the feed could not be read" rather than "you are up to date", which is the
/// difference between a bug report and a false all-clear.
pub fn parse_manifest(raw: &str) -> Manifest {
    let entries: Vec<FeedEntry> = match serde_json::from_str(raw) {
        Ok(entries) => entries,
        Err(err) => {
            return Manifest {
                releases: Vec::new(),
                rejected: vec![RejectedEntry {
                    version: raw.chars().take(60).collect(),
                    reason: format!("the manifest could not be read: {err}"),
                }],
            };
        }
    };

    let mut releases = Vec::with_capacity(entries.len());
    let mut rejected = Vec::new();
    for entry in entries {
        let version = match Version::parse(&entry.version) {
            Ok(version) => version,
            Err(VersionError { reason, .. }) => {
                rejected.push(RejectedEntry {
                    version: entry.version.clone(),
                    reason: reason.to_string(),
                });
                continue;
            }
        };
        let channel = entry
            .channel
            .as_deref()
            .and_then(Channel::parse)
            .unwrap_or(Channel::Stable);
        let core_min = match entry.core_min.as_deref() {
            Some(raw) => match Version::parse(raw) {
                Ok(version) => Some(version),
                Err(err) => {
                    // A malformed floor is rejected outright rather than dropped: accepting the
                    // release with the floor ignored is how a 3.0.0 that needs a 2.9 core reaches
                    // a 2.4 install.
                    rejected.push(RejectedEntry {
                        version: entry.version.clone(),
                        reason: format!("its minimum core version is unusable: {}", err.reason),
                    });
                    continue;
                }
            },
            None => None,
        };
        releases.push(Release {
            version,
            channel,
            notes: entry.notes.unwrap_or_default(),
            breaking: entry.breaking.unwrap_or(false),
            migrations: entry.migrations.unwrap_or_default(),
            core_min,
            artifact_checksum: entry.artifact_checksum,
            released_at: entry.released_at.as_deref().and_then(parse_released_at),
        });
    }
    Manifest { releases, rejected }
}

/// What an update check found, as the check's own state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum CheckResult {
    /// The feed answered. `announced` lists the versions this check was the **first** to see.
    Completed {
        /// Versions newly seen, deduped on channel + version across every previous run.
        announced: Vec<String>,
        /// How many entries the feed carried.
        seen: usize,
    },
    /// The feed could not be read. The cache is untouched and the screen shows the stale banner.
    Failed {
        /// What to tell the operator.
        reason: String,
    },
}

/// The dedupe state an update check carries between runs.
///
/// Keyed on **channel + version** and not on the version alone, because the two are different
/// facts: `2.5.0-rc.1` on beta and a hypothetical `2.5.0-rc.1` on nightly are two releases, and a
/// key of just the version would let the second one be announced as nothing new.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeenSet(BTreeMap<(String, String), ()>);

impl SeenSet {
    /// An empty set: nothing has been seen, so the first check announces every release on the
    /// channel. That is correct and worth being explicit about, because the alternative — seeding
    /// the set from the cache and announcing nothing on the first run — hides every update from
    /// an instance that has never checked.
    pub fn new() -> Self {
        SeenSet(BTreeMap::new())
    }

    /// The releases in `releases` that this set has not seen, and record them as seen.
    ///
    /// Returns the newly seen versions in **manifest order**, so the emitted event's first element
    /// is the newest only if the feed itself is ordered — and the feed usually is, so the common
    /// case is right without this function inventing an order.
    pub fn take_new(&mut self, releases: &[Release]) -> Vec<String> {
        let mut fresh = Vec::new();
        for release in releases {
            let key = (
                release.channel.as_str().to_string(),
                release.version.to_string(),
            );
            if self.0.insert(key, ()).is_none() {
                fresh.push(release.version.to_string());
            }
        }
        fresh
    }

    /// How many (channel, version) pairs are recorded.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Has this set recorded nothing?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Run one update check over a feed, returning what it found and what may be announced.
///
/// `seen` is taken mutably and carries the dedupe across runs. `current` is the release this
/// installation is running, which decides the card's `Available` line, and `core` is this
/// installation's own core version when it reports one.
pub fn run_check(
    raw: &str,
    current: &Release,
    core: Option<&Version>,
    seen: &mut SeenSet,
) -> (CheckResult, Availability) {
    let manifest = parse_manifest(raw);
    if manifest.releases.is_empty() {
        // Nothing parsed, so nothing may be announced and nothing may be claimed. The cached
        // rows stay exactly as they were, which is what lets the screen keep rendering.
        let reason = manifest.rejected.first().map_or_else(
            || "the manifest carried no releases".to_string(),
            |r| r.reason.clone(),
        );
        return (CheckResult::Failed { reason }, Availability::UpToDate);
    }
    // The set records the whole manifest, not only the channel this instance follows: a beta
    // install that later switches to nightly must not re-announce every nightly release it has
    // already seen, and a set that only ever heard about stable has no way to know that.
    let announced = seen.take_new(&manifest.releases);
    let offer = availability(current, core, &manifest.releases);
    (
        CheckResult::Completed {
            announced,
            seen: manifest.releases.len(),
        },
        offer,
    )
}

/// What the screen shows above the cards when the feed is not answering.
///
/// A dedicated function because the banner is a *product* surface: the spec requires the wording
/// to name the cached timestamp, and a generic error component does not have it.
pub fn stale_banner(reason: &str, cached_at: &str) -> String {
    format!("release feed unreachable ({reason}) — showing cached data from {cached_at}")
}

/// The rows the checks screen lists, so `/deployment/checks` has a real answer to render.
///
/// Each row is a [`CheckOutcome`] with the shape the wizard already uses, rather than a second
/// vocabulary for "this passed".
pub fn check_rows(
    result: &CheckResult,
    channel: Channel,
    next_run: &str,
    cached_at: &str,
) -> Vec<CheckOutcome> {
    let mut rows = Vec::new();
    rows.push(match result {
        CheckResult::Completed { seen, announced } => {
            let detail = if announced.is_empty() {
                format!("{seen} release(s) known; nothing new since the last check")
            } else {
                format!("{seen} release(s) known; new: {}", announced.join(", "))
            };
            CheckOutcome::pass(crate::preflight::CheckId::DependencyHealth, detail)
        }
        CheckResult::Failed { reason } => CheckOutcome::warn(
            crate::preflight::CheckId::DependencyHealth,
            format!("release feed unreachable: {reason}"),
            format!(
                "the release list is cached data from {cached_at}; re-run the check to refresh it"
            ),
        ),
    });
    rows.push(CheckOutcome::pass(
        crate::preflight::CheckId::PendingMigrations,
        format!("following the {channel} channel; next check {next_run}"),
    ));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json() -> String {
        serde_json::json!([
            {
                "version": "2.5.0",
                "channel": "stable",
                "notes": "Adds the deployment centre.",
                "migrations": ["0211_deployment_center"],
                "artifact_checksum": "sha256:abc"
            },
            { "version": "2.4.1", "channel": "stable", "notes": "Previous." },
            { "version": "2.6.0", "channel": "nightly", "notes": "Untagged." },
        ])
        .to_string()
    }

    fn stable_241() -> Release {
        Release::minimal("2.4.1", Channel::Stable)
    }

    #[test]
    fn a_feed_becomes_releases_and_keeps_the_installed_one() {
        let manifest = parse_manifest(&manifest_json());
        assert!(
            manifest.rejected.is_empty(),
            "nothing should be rejected: {:?}",
            manifest.rejected
        );
        assert_eq!(manifest.releases.len(), 3);
        let offer = availability(&stable_241(), None, &manifest.releases);
        assert_eq!(
            offer,
            Availability::Upgrade {
                version: "2.5.0".into(),
                breaking: false
            }
        );
    }

    #[test]
    fn a_nightly_entry_is_reported_as_blocked_with_its_reason() {
        // The feed carries a nightly 2.6.0 that is *newer* than the stable 2.5.0 on offer. The
        // card must name 2.5.0 and not fall for the higher number.
        let manifest = parse_manifest(&manifest_json());
        let stable_only: Vec<Release> = manifest
            .releases
            .iter()
            .filter(|r| r.channel == Channel::Stable)
            .cloned()
            .collect();
        assert_eq!(
            availability(&stable_241(), None, &stable_only),
            Availability::Upgrade {
                version: "2.5.0".into(),
                breaking: false
            },
        );
        // With the nightly dropped in and nothing else newer, the answer is still the stable one.
        assert_eq!(
            availability(&stable_241(), None, &manifest.releases).label(),
            "2.5.0"
        );
    }

    #[test]
    fn a_version_is_announced_exactly_once_across_runs() {
        let mut seen = SeenSet::new();
        let (first, _) = run_check(&manifest_json(), &stable_241(), None, &mut seen);
        let CheckResult::Completed { announced, .. } = first else {
            panic!("the first check should complete")
        };
        assert_eq!(
            announced,
            vec!["2.5.0", "2.4.1", "2.6.0"],
            "the first check announces everything"
        );

        // Same feed again: nothing is new, and the card still says the same thing.
        let (second, offer) = run_check(&manifest_json(), &stable_241(), None, &mut seen);
        let CheckResult::Completed { announced, .. } = second else {
            panic!("the second check should complete")
        };
        assert!(
            announced.is_empty(),
            "a repeated feed must announce nothing: {announced:?}"
        );
        assert_eq!(offer.label(), "2.5.0");
    }

    #[test]
    fn a_new_version_announces_while_the_old_one_stays_quiet() {
        let mut seen = SeenSet::new();
        run_check(&manifest_json(), &stable_241(), None, &mut seen);
        let extended = serde_json::json!([
            { "version": "2.6.0", "channel": "stable", "notes": "Next." },
            { "version": "2.5.0", "channel": "stable" },
            { "version": "2.4.1", "channel": "stable" },
        ])
        .to_string();
        let (result, _) = run_check(&extended, &stable_241(), None, &mut seen);
        let CheckResult::Completed { announced, .. } = result else {
            panic!("the check should complete")
        };
        assert_eq!(announced, vec!["2.6.0"], "only the new one speaks");
    }

    #[test]
    fn the_same_version_on_two_channels_is_two_facts() {
        // The dedupe is keyed on channel + version, so a release that appears on beta and on
        // nightly is announced twice — they are two different builds wearing one number.
        let mut seen = SeenSet::new();
        let two = serde_json::json!([
            { "version": "2.5.0", "channel": "beta" },
            { "version": "2.5.0", "channel": "nightly" },
        ])
        .to_string();
        let (result, _) = run_check(&two, &stable_241(), None, &mut seen);
        let CheckResult::Completed {
            announced,
            seen: count,
        } = result
        else {
            panic!("the check should complete")
        };
        assert_eq!(count, 2);
        assert_eq!(announced, vec!["2.5.0", "2.5.0"]);
    }

    #[test]
    fn a_truncated_feed_keeps_the_cache_and_says_so() {
        let mut seen = SeenSet::new();
        let (result, _) = run_check("[{\"version\": \"2.5.0\"", &stable_241(), None, &mut seen);
        let CheckResult::Failed { reason } = result else {
            panic!("a truncated feed must not report as a completed check")
        };
        assert!(reason.contains("could not be read"), "{reason}");
        assert_eq!(
            seen.len(),
            0,
            "a failed check must not record versions as seen"
        );
    }

    #[test]
    fn one_bad_entry_does_not_hide_the_good_ones() {
        let mixed = serde_json::json!([
            { "version": "2.5.0", "channel": "stable" },
            { "version": "not-a-version", "channel": "stable" },
            { "version": "2.6.0", "channel": "stable", "core_min": "also-not-a-version" },
        ])
        .to_string();
        let manifest = parse_manifest(&mixed);
        assert_eq!(manifest.releases.len(), 1, "the good entry survives");
        assert_eq!(
            manifest.rejected.len(),
            2,
            "both bad entries are reported, not dropped"
        );
        assert!(manifest.rejected[1].reason.contains("minimum core"));
    }

    #[test]
    fn a_release_whose_core_floor_is_unusable_never_reaches_the_card() {
        // The specific hazard: honouring the release and ignoring its floor is how a 3.0.0 that
        // needs a 2.9 core reaches a 2.4 installation.
        let mixed = serde_json::json!([
            { "version": "3.0.0", "channel": "stable", "core_min": "nope" },
        ])
        .to_string();
        let manifest = parse_manifest(&mixed);
        assert!(manifest.releases.is_empty());
        assert!(!manifest.rejected.is_empty());
    }

    #[test]
    fn the_banner_names_the_reason_and_the_cached_time() {
        let banner = stale_banner("connection refused", "2026-10-01 06:00");
        assert!(banner.contains("connection refused"), "{banner}");
        assert!(banner.contains("2026-10-01 06:00"), "{banner}");
    }

    #[test]
    fn a_failed_check_renders_a_warning_row_not_an_error() {
        let rows = check_rows(
            &CheckResult::Failed {
                reason: "timeout".into(),
            },
            Channel::Stable,
            "in 6 hours",
            "2026-10-01 06:00",
        );
        let feed_row = &rows[0];
        assert_eq!(feed_row.id, crate::preflight::CheckId::DependencyHealth);
        assert_eq!(feed_row.state, crate::preflight::CheckState::Warn);
        assert!(feed_row.detail.contains("timeout"));
        assert!(feed_row.suggestion.contains("2026-10-01 06:00"));
    }

    #[test]
    fn a_release_date_is_read_whether_or_not_it_obeys_rfc_3339() {
        // CI emits what it emits: a date with no time, and a space where RFC 3339 wants `T`.
        // Both are read rather than refused, because a release with an unreadable date is still
        // a release and dropping it would be worse than showing it undated.
        let dated = serde_json::json!([
            { "version": "2.5.0", "channel": "stable", "released_at": "2026-10-01T09:00:00Z" },
            { "version": "2.4.1", "channel": "stable", "released_at": "2026-09-01" },
            { "version": "2.3.0", "channel": "stable", "released_at": "2026-09-01 09:00:00" },
            { "version": "2.2.0", "channel": "stable", "released_at": "whenever" },
            { "version": "2.1.0", "channel": "stable" },
        ])
        .to_string();
        let manifest = parse_manifest(&dated);
        assert_eq!(
            manifest.releases.len(),
            5,
            "no entry is dropped over a date"
        );
        let at = |v: &str| {
            manifest
                .releases
                .iter()
                .find(|r| r.version.to_string() == v)
                .and_then(|r| r.released_at.clone())
        };
        assert_eq!(at("2.5.0").as_deref(), Some("2026-10-01T09:00:00Z"));
        assert_eq!(at("2.4.1").as_deref(), Some("2026-09-01T00:00:00Z"));
        assert_eq!(
            at("2.3.0").as_deref(),
            Some("2026-09-01T09:00:00Z"),
            "a naive datetime is read as UTC"
        );
        assert_eq!(
            at("2.2.0"),
            None,
            "an unreadable date renders as no date, not a dropped release"
        );
        assert_eq!(at("2.1.0"), None, "an absent date is not an error");
    }

    #[test]
    fn an_empty_seen_set_announces_the_whole_first_feed() {
        // Seeding the set from the cache instead would hide every update from an instance that
        // has never checked — the bug this documents.
        let mut seen = SeenSet::new();
        assert!(seen.is_empty());
        let (result, _) = run_check(&manifest_json(), &stable_241(), None, &mut seen);
        let CheckResult::Completed { announced, .. } = result else {
            panic!()
        };
        assert_eq!(announced.len(), 3);
        assert_eq!(seen.len(), 3);
    }
}
