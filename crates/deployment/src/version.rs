//! Versions, channels, and the question the environment card actually asks.
//!
//! The card in the brief is four lines and one of them is the hard one:
//!
//! ```text
//! Version     2.4.1
//! Available   2.5.0
//! ```
//!
//! Deciding whether `2.5.0` belongs on that second line is not string comparison, and this module
//! exists because every shortcut at it is wrong in a way that ships. Comparing as text puts
//! `1.10.0` before `1.9.0`; ignoring pre-releases offers a nightly build as an upgrade over a
//! stable one; and "no newer release" rendering as an empty field is the exact failure the spec
//! calls out by name ("when up to date it reads `Available — (up to date)`").
//!
//! So the three decisions are named types here rather than expressions at the call site:
//! [`Release::is_upgrade_over`], [`Channel::admits`] and [`Availability::label`].

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

/// How far from stable a release is allowed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Tagged, supported releases only.
    #[default]
    Stable,
    /// Feature-complete releases that are not yet tagged.
    Beta,
    /// Every commit. Never offered to a stable installation.
    Nightly,
}

impl Channel {
    /// The value stored in `releases_cache.channel` and sent by the feed.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Nightly => "nightly",
        }
    }

    /// Parse a channel name, case-insensitively. `None` for anything the platform does not ship.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            "nightly" => Some(Channel::Nightly),
            _ => None,
        }
    }

    /// Every channel, in the order the release list filter offers them.
    pub const ALL: [Channel; 3] = [Channel::Stable, Channel::Beta, Channel::Nightly];

    /// May a release be offered to an installation that subscribes to *this* channel?
    ///
    /// `self` is the **installation's** channel; `candidate` is the release on offer. Two rules,
    /// and the first one is the whole answer to the mistake this type exists to prevent:
    ///
    /// 1. **The channel must be the same one.** A nightly build is not a newer stable release —
    ///    it is a different promise about support, and the number beside it is not what makes it
    ///    newer. Any cross-channel offer is refused here, whatever the versions say, and
    ///    [`crate::availability`] reports it as *blocked with a reason* rather than silently
    ///    dropping it, so the card can say `2.5.0 — published on the nightly channel` instead of
    ///    claiming the instance is up to date.
    /// 2. **A stable installation takes no pre-release tag**, even on its own channel. A `-rc.1`
    ///    build published into the stable feed is a publisher's mistake, and taking it is how an
    ///    untagged artifact reaches a production instance.
    ///
    /// A beta or nightly installation may take a tagged release or a pre-release, because that is
    /// what it signed up for.
    pub fn admits(self, candidate: &Release) -> bool {
        if candidate.channel != self {
            return false;
        }
        match self {
            Channel::Stable => !candidate.version.is_pre_release(),
            Channel::Beta | Channel::Nightly => true,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A version that did not parse.
///
/// Carries the offending text because it is written into a form field: an operator who typed
/// `2.5.0 (beta)` needs to be told which part of what they typed is not a version, and "invalid
/// version" with no echo of the input is the message that produces a support ticket.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{raw:?} is not a version: {reason}")]
pub struct VersionError {
    /// Exactly what was supplied.
    pub raw: String,
    /// What was wrong with it, phrased for a person.
    pub reason: &'static str,
}

/// A parsed semantic version with an optional pre-release tag.
///
/// `major.minor.patch` is required; the build metadata after `+` is **not** parsed, because two
/// builds of the same version must compare equal — a deployment history that sorts `2.4.1+b2`
/// above `2.4.1+b1` invents an upgrade that has no artifact behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// The breaking-change counter.
    pub major: u32,
    /// The feature counter.
    pub minor: u32,
    /// The fix counter.
    pub patch: u32,
    /// The pre-release tag without its leading `-`, if any.
    pub pre: Option<String>,
}

impl Version {
    /// Parse `2.4.1`, `v2.4.1`, `2.5.0-rc.1` or `2.4.1+build.7`.
    pub fn parse(raw: &str) -> Result<Self, VersionError> {
        let trimmed = raw.trim();
        let fail = |reason: &'static str| VersionError {
            raw: raw.to_string(),
            reason,
        };

        // A leading `v` is what git tags actually carry, and the release feed is generated from
        // tags — refusing it would make every real tag need hand-editing before it is offered.
        let body = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
        // Build metadata is explicitly not ordering information (semver §10).
        let body = body.split('+').next().unwrap_or(body);
        let (core, pre) = match body.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (body, None),
        };

        let mut parts = core.split('.');
        let mut next = |what: &'static str| -> Result<u32, VersionError> {
            let seg = parts.next().ok_or_else(|| fail(what))?;
            if seg.is_empty() {
                return Err(fail("one of the version segments is empty"));
            }
            // `01` is rejected rather than read as 1: two manifests that disagree about whether a
            // segment is zero-padded name the same version, and only one of them is on the feed.
            if seg.len() > 1 && seg.starts_with('0') {
                return Err(fail("a version segment is zero-padded"));
            }
            seg.parse::<u32>().map_err(|_| fail(what))
        };

        let major = next("the major segment is not a number")?;
        let minor = next("the minor segment is not a number")?;
        let patch = next("the patch segment is not a number")?;
        if parts.next().is_some() {
            return Err(fail("it has more than three segments"));
        }
        if let Some(pre) = pre {
            if pre.is_empty() {
                return Err(fail("the pre-release tag is empty"));
            }
            if !pre
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(fail("the pre-release tag has unsupported characters"));
            }
        }

        Ok(Version {
            major,
            minor,
            patch,
            pre: pre.map(str::to_string),
        })
    }

    /// The core triple, ignoring any pre-release tag.
    pub fn core(&self) -> (u32, u32, u32) {
        (self.major, self.minor, self.patch)
    }

    /// Is this a pre-release build rather than a tagged release?
    pub fn is_pre_release(&self) -> bool {
        self.pre.is_some()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core()
            .cmp(&other.core())
            // A tagged release outranks any pre-release of the same numbers: 2.5.0 is newer than
            // 2.5.0-rc.1, and this is the comparison that keeps an rc from being offered as the
            // upgrade for the release it was a candidate for.
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One row of the release manifest, as cached by the update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The parsed version.
    pub version: Version,
    /// The channel it was published on.
    pub channel: Channel,
    /// The published notes, already rendered to plain text.
    pub notes: String,
    /// Whether the notes declare breaking changes.
    pub breaking: bool,
    /// Migrations this release ships, in the order they must run.
    pub migrations: Vec<String>,
    /// The oldest core that can run this release, if it declares one.
    pub core_min: Option<Version>,
    /// The artifact digest, so a deploy can prove it fetched what the feed described.
    pub artifact_checksum: Option<String>,
    /// When the release was published, when the feed says so.
    ///
    /// A `String` and not a parsed timestamp, deliberately: the release list renders the feed's own
    /// string, and a value that fails to parse must still be *shown* rather than dropped or
    /// reformatted into a timezone the publisher did not write.
    pub released_at: Option<String>,
}

impl Release {
    /// A release with the minimum a card needs, for tests and for a feed entry with no notes.
    pub fn minimal(version: &str, channel: Channel) -> Self {
        Release {
            version: Version::parse(version).expect("a test version parses"),
            channel,
            notes: String::new(),
            breaking: false,
            migrations: Vec::new(),
            core_min: None,
            artifact_checksum: None,
            released_at: None,
        }
    }

    /// Is this release an upgrade worth putting on the card's `Available` line?
    ///
    /// Three conditions, all required: it is newer than what is installed, the **installation's**
    /// channel admits it, and this installation's core can run it.
    ///
    /// The channel check is `current.channel.admits(...)` and not `self.channel.admits(...)`, and
    /// the distinction is the whole rule. A channel is what *this* instance signed up for, so it
    /// is the reader that decides; asking the candidate is the same bug written twice — it asks
    /// "is a nightly channel allowed to offer things?" (always yes) rather than "may this stable
    /// installation take a nightly build?" (never). The third condition is the one a pure version
    /// comparison forgets, and it is the reason a card that says `Available 3.0.0` next to a 2.x
    /// core is a support ticket rather than an upgrade.
    pub fn is_upgrade_over(&self, current: &Release, current_core: Option<&Version>) -> bool {
        if self.version <= current.version {
            return false;
        }
        if !current.channel.admits(self) {
            return false;
        }
        match (self.core_min.as_ref(), current_core) {
            (Some(min), Some(core)) => core >= min,
            // A release with no declared floor, or an installation that does not report its
            // core version, cannot be proven incompatible — and blocking the card on an unknown
            // would make an upgrade impossible without a core version, which is the wrong
            // default for a value the operator can supply in the wizard.
            _ => true,
        }
    }
}

/// What the card's second line says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Availability {
    /// A specific newer release is on offer.
    Upgrade {
        /// The version string to render.
        version: String,
        /// Whether the notes declare breaking changes, so the card can flag it.
        breaking: bool,
    },
    /// Nothing newer on this channel. The spec names this state: an empty field is a bug.
    UpToDate,
    /// Newer releases exist but none may be offered here, and the card says why.
    /// `reason` is written for an operator, not for a log.
    Blocked {
        /// The best candidate that was held back.
        candidate: String,
        /// Why it was not offered.
        reason: String,
    },
}

impl Availability {
    /// The exact second line of the card.
    ///
    /// Three shapes, and the third exists because it is the honest answer in the two cases the
    /// first two get wrong: a nightly build exists but must not be offered, and a newer release
    /// exists but this installation's core is too old to run it.
    pub fn label(&self) -> String {
        match self {
            Availability::Upgrade { version, breaking } => {
                if *breaking {
                    format!("{version} (breaking changes)")
                } else {
                    version.clone()
                }
            }
            Availability::UpToDate => "— (up to date)".to_string(),
            Availability::Blocked { candidate, reason } => format!("{candidate} — {reason}"),
        }
    }

    /// Is there something to press `Deploy` against?
    pub fn is_actionable(&self) -> bool {
        matches!(self, Availability::Upgrade { .. })
    }
}

/// A `best so far` accumulator: keep `release` unless what is already held is newer.
///
/// Written once and used three times, because the inverted spelling of this comparison — the one
/// that keeps the *oldest* release seen so far — is a bug that produces a plausible-looking card
/// and a downgrade, and it is invisible in every test that offers a single release.
fn newer_than(release: &Release, held: Option<&Release>) -> bool {
    !matches!(held, Some(b) if b.version >= release.version)
}

/// Decide the card's `Available` line from every cached release on the channel.
///
/// `releases` may arrive in any order and may include the installed version itself, older
/// versions, and releases on other channels — the function is the one place that knows all three
/// are irrelevant, so a caller cannot get it wrong by picking the "last" row.
pub fn availability(
    current: &Release,
    current_core: Option<&Version>,
    releases: &[Release],
) -> Availability {
    let mut best: Option<&Release> = None;
    let mut newest_other_channel: Option<&Release> = None;
    let mut newest_mismatched: Option<&Release> = None;
    let mut newest_too_new_core: Option<&Release> = None;

    for release in releases {
        // Older-than or equal-to what is installed is irrelevant on any channel, so this is the
        // only filter applied before the channel is looked at.
        if release.version <= current.version {
            continue;
        }
        if release.channel != current.channel {
            if newer_than(release, newest_other_channel) {
                newest_other_channel = Some(release);
            }
            continue;
        }
        if !current.channel.admits(release) {
            if newer_than(release, newest_mismatched) {
                newest_mismatched = Some(release);
            }
            continue;
        }
        if let (Some(min), Some(core)) = (release.core_min.as_ref(), current_core) {
            if core < min {
                if newer_than(release, newest_too_new_core) {
                    newest_too_new_core = Some(release);
                }
                continue;
            }
        }
        if newer_than(release, best) {
            best = Some(release);
        }
    }

    if let Some(release) = best {
        debug_assert!(
            release.is_upgrade_over(current, current_core),
            "`best` only holds releases that passed every gate above"
        );
        return Availability::Upgrade {
            version: release.version.to_string(),
            breaking: release.breaking,
        };
    }
    // The blocked reasons are ordered by how much the operator can act on them: a core that is
    // too old is a deploy that needs a different order, a release the instance's channel does not
    // admit is a settings change, and neither is "you are up to date".
    if let Some(release) = newest_too_new_core {
        let min = release.core_min.as_ref().map_or_else(
            || "a newer core".to_string(),
            |v| format!("core {v} or newer"),
        );
        return Availability::Blocked {
            candidate: release.version.to_string(),
            reason: format!("needs {min}"),
        };
    }
    if let Some(release) = newest_other_channel {
        return Availability::Blocked {
            candidate: release.version.to_string(),
            reason: format!("published on the {} channel", release.channel),
        };
    }
    if let Some(release) = newest_mismatched {
        return Availability::Blocked {
            candidate: release.version.to_string(),
            reason: format!("not a release this channel accepts ({})", current.channel),
        };
    }
    Availability::UpToDate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str, ch: Channel) -> Release {
        Release::minimal(v, ch)
    }

    #[test]
    fn parses_the_shapes_a_release_feed_actually_carries() {
        assert_eq!(Version::parse("2.4.1").unwrap().to_string(), "2.4.1");
        assert_eq!(Version::parse("v2.4.1").unwrap().to_string(), "2.4.1");
        assert_eq!(
            Version::parse(" 2.5.0-rc.1 ").unwrap().to_string(),
            "2.5.0-rc.1"
        );
        assert_eq!(
            Version::parse("2.4.1+build.7").unwrap().to_string(),
            "2.4.1"
        );
    }

    #[test]
    fn refuses_what_two_manifests_would_disagree_about() {
        for bad in ["2.4", "2.4.1.9", "2.4.x", "2.04.1", "2.4.1-", "latest", ""] {
            let err = Version::parse(bad).unwrap_err();
            assert!(
                !err.reason.is_empty(),
                "{bad:?} needs a reason an operator can read"
            );
            assert_eq!(err.raw, bad, "the message must echo what was typed");
        }
    }

    #[test]
    fn build_metadata_does_not_create_an_upgrade() {
        // Two builds of the same release are the same version; a history that ordered them
        // would offer a second one as a new version with no artifact behind it.
        let a = Version::parse("2.4.1+b1").unwrap();
        let b = Version::parse("2.4.1+b2").unwrap();
        assert_eq!(a, b);
        assert!(!(a < b) && !(b < a));
    }

    #[test]
    fn orders_ten_above_nine() {
        // The whole reason this is a parsed type: text comparison puts "1.10.0" before "1.9.0"
        // and would offer a downgrade.
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.0").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
    }

    #[test]
    fn a_tagged_release_outranks_its_own_release_candidate() {
        assert!(Version::parse("2.5.0").unwrap() > Version::parse("2.5.0-rc.1").unwrap());
        assert!(Version::parse("2.5.0-rc.2").unwrap() > Version::parse("2.5.0-rc.1").unwrap());
        assert!(Version::parse("2.4.9").unwrap() < Version::parse("2.5.0-rc.1").unwrap());
    }

    #[test]
    fn a_nightly_is_never_offered_to_a_stable_installation() {
        let stable = rel("2.4.1", Channel::Stable);
        let nightly = rel("2.5.0", Channel::Nightly);
        assert_eq!(
            availability(&stable, None, &[nightly.clone()]),
            Availability::Blocked {
                candidate: "2.5.0".into(),
                reason: "published on the nightly channel".into(),
            },
        );
        assert!(!nightly.is_upgrade_over(&stable, None));
    }

    #[test]
    fn up_to_date_names_itself_instead_of_leaving_the_field_empty() {
        let stable = rel("2.4.1", Channel::Stable);
        assert_eq!(availability(&stable, None, &[]), Availability::UpToDate);
        assert_eq!(Availability::UpToDate.label(), "— (up to date)");
        // The installed version being present in the feed is the normal case, not a bug.
        assert_eq!(
            availability(
                &stable,
                None,
                &[rel("2.4.1", Channel::Stable), rel("2.3.0", Channel::Stable)]
            ),
            Availability::UpToDate,
        );
    }

    #[test]
    fn picks_the_newest_offer_regardless_of_feed_order() {
        let stable = rel("2.4.1", Channel::Stable);
        let feed = [
            rel("2.5.0", Channel::Stable),
            rel("2.10.0", Channel::Stable),
            rel("2.9.0", Channel::Stable),
        ];
        assert_eq!(
            availability(&stable, None, &feed),
            Availability::Upgrade {
                version: "2.10.0".into(),
                breaking: false
            },
        );
    }

    #[test]
    fn a_release_needing_a_newer_core_is_held_back_with_the_reason() {
        let stable = rel("2.4.1", Channel::Stable);
        let mut big = rel("3.0.0", Channel::Stable);
        big.core_min = Some(Version::parse("2.9.0").unwrap());
        let core = Version::parse("2.4.0").unwrap();

        assert_eq!(
            availability(&stable, Some(&core), &[big.clone()]),
            Availability::Blocked {
                candidate: "3.0.0".into(),
                reason: "needs core 2.9.0 or newer".into(),
            },
        );
        assert!(!big.is_upgrade_over(&stable, Some(&core)));
        // The same release is offered once the core is new enough — the block is about this
        // installation, not about the release being forbidden.
        let new_core = Version::parse("2.9.0").unwrap();
        assert!(big.is_upgrade_over(&stable, Some(&new_core)));
    }

    #[test]
    fn breaking_releases_are_flagged_on_the_card() {
        let stable = rel("2.4.1", Channel::Stable);
        let mut breaking = rel("3.0.0", Channel::Stable);
        breaking.breaking = true;
        let availability = availability(&stable, None, &[breaking]);
        assert!(availability.is_actionable());
        assert_eq!(availability.label(), "3.0.0 (breaking changes)");
    }

    #[test]
    fn a_blocked_candidate_never_reports_as_actionable() {
        let stable = rel("2.4.1", Channel::Stable);
        let availability = availability(&stable, None, &[rel("2.5.0", Channel::Beta)]);
        assert!(!availability.is_actionable());
    }

    #[test]
    fn channel_names_round_trip() {
        for channel in Channel::ALL {
            assert_eq!(Channel::parse(channel.as_str()), Some(channel));
            assert_eq!(
                Channel::parse(&channel.as_str().to_uppercase()),
                Some(channel)
            );
        }
        assert_eq!(Channel::parse("edge"), None);
        assert_eq!(Channel::default(), Channel::Stable);
    }
}
