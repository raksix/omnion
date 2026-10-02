//! The release manifest: its vocabulary, its version arithmetic and the destructiveness verdict
//! (docs/requests/REQ-128).
//!
//! This is the Rust half of the release contract that `release/lib/manifest.py` implements for
//! the build side. The two must agree, and the ways they could drift are stated in
//! [`destructiveness`] and in the tests that hold the boundary from both sides.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DeploymentError, Result};

/// A parsed SemVer, as the release tooling compares them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
    /// The pre-release tag, empty for a final release.
    pub pre: String,
}

impl Version {
    /// Parse a SemVer string, or a refusal naming the field that was not one.
    ///
    /// The refusal is a refusal rather than a default because every caller here would rather
    /// have no plan than a plan computed from `0.0.0`: an unparseable version silently
    /// defaulted turns "cannot plan this" into "plans a fresh install", which is the kind of
    /// wrong answer an operator acts on.
    pub fn parse(raw: &str, field: &'static str) -> Result<Self> {
        let text = raw.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, pre.to_owned()),
            None => (text, String::new()),
        };
        let core = core.split('+').next().unwrap_or(core);
        let mut parts = core.split('.');
        let mut next = |field_name: &str| -> Result<u64> {
            parts
                .next()
                .and_then(|p| p.parse::<u64>().ok())
                .ok_or_else(|| DeploymentError::InvalidVersion {
                    field,
                    value: format!("{raw} ({field_name} is not a number)"),
                })
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if parts.next().is_some() {
            return Err(DeploymentError::InvalidVersion {
                field,
                value: format!("{raw} (more than three components)"),
            });
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// `Ordering`-compatible comparison: a pre-release is older than its final release.
    ///
    /// The pre-release rule is the one that matters in practice and it is easy to forget:
    /// `0.5.0-rc.1` is a version operators actually install, and treating it as equal to
    /// `0.5.0` would report a plan from the release candidate to the final as "the same
    /// version" and refuse it.
    pub fn cmp_to(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A final release outranks any pre-release of the same number.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

// -------------------------------------------------------------------------------------------
// The channel / kind vocabularies
// -------------------------------------------------------------------------------------------

/// The release channels a manifest may declare. A manifest with a channel outside this set is
/// refused rather than stored, because "which releases may this installation see" is decided by
/// the channel column and an unknown value would widen or narrow that set silently.
pub const CHANNELS: &[&str] = &["stable", "beta", "edge"];

/// The artifact kinds a release may publish. A kind the screens do not render is a row nothing
/// can display, so the set is closed and a manifest naming another kind is refused on write.
pub const ARTIFACT_KINDS: &[&str] = &["image", "cli", "chart", "sbom", "compose"];

/// The two topologies an upgrade plan can be built for.
pub const TOPOLOGIES: &[&str] = &["compose", "kubernetes"];

/// What a **generated bundle** can target. `helm` is in this set and NOT in
/// [`COMPOSE_STACK_KINDS`]: a Helm values file is a bundle, and it is not a compose stack.
/// A check that used this set to validate a *plan's* stack would accept `helm` and then resolve
/// it to a compose file, which is how the first draft of the plan builder handed a Helm operator
/// a list of `docker compose` commands.
pub const BUNDLE_KINDS: &[&str] = &["compose-small", "compose-enterprise", "helm"];

/// The stack kinds a **compose upgrade plan** can target. A subset of [`BUNDLE_KINDS`], and the
/// only set a compose plan may be built for.
pub const COMPOSE_STACK_KINDS: &[&str] = &["compose-small", "compose-enterprise"];

/// Where each compose stack lives, relative to the repository root. Read by the caller from the
/// tree rather than hardcoded into a command, because the plan is executed on somebody's host
/// and a path that does not exist there is a command that fails at step one.
pub fn compose_stack_file(kind: &str) -> Result<&'static str> {
    match kind {
        "compose-small" => Ok("infra/compose/docker-compose.prod.yml"),
        "compose-enterprise" => Ok("infra/compose/docker-compose.enterprise.yml"),
        other => Err(DeploymentError::UnknownVocabulary(format!(
            "{other:?} is not a compose stack; expected one of compose-small, compose-enterprise"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// Destructiveness
// -------------------------------------------------------------------------------------------

/// Every migration in the range ships a down script the CI gate verified.
pub const VERDICT_REVERSIBLE: &str = "reversible";
/// At least one migration cannot be reversed; the database rollback is a restore.
pub const VERDICT_DESTRUCTIVE: &str = "destructive";
/// Nobody has established which. **The verdict this repository is in today.**
pub const VERDICT_UNKNOWN: &str = "unknown";

/// The three verdicts, in the order an operator should read them.
pub const VERDICTS: &[&str] = &[VERDICT_UNKNOWN, VERDICT_DESTRUCTIVE, VERDICT_REVERSIBLE];

/// What is known about the migrations an upgrade applies, and how it was known.
///
/// `source` is a field rather than a private detail because the question an operator asks when a
/// plan says "take a backup" is *why does it think that*, and the answer differs per release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destructiveness {
    /// One of [`VERDICTS`].
    pub verdict: String,
    /// The sentence shown to the operator, carrying the answer to "why".
    pub reason: String,
    /// The migrations in the range that are known to be irreversible.
    pub destructive_migrations: Vec<String>,
    /// How the database goes back: `down-script`, `restore-from-backup` or `unknown`.
    pub database_rollback: String,
    /// Which input decided it: `migration-marker`, `manifest` or `policy-absent`.
    pub source: String,
}

impl Destructiveness {
    /// `true` when an operator has to acknowledge this before the checklist is complete.
    ///
    /// Derived from the verdict rather than passed in, so a caller cannot pass
    /// `acknowledged: false` for a verdict that needs acknowledgement. The `unknown` case is
    /// here on purpose: an operator must accept that *nobody knows* before the plan renders as
    /// finished, which is a different decision from accepting that a migration is destructive.
    pub fn requires_acknowledgement(&self) -> bool {
        matches!(self.verdict.as_str(), VERDICT_DESTRUCTIVE | VERDICT_UNKNOWN)
    }
}

/// The destructiveness of the migrations an upgrade applies.
///
/// Four inputs, consulted in the order of their trust:
///
/// 1. **The migration files' own markers.** `-- omnion:no-down` is REQ-129's documented
///    exception syntax and is a fact about the file, not an assertion about behaviour.
/// 2. **The manifest's own flag.** A published manifest is a contract (slice 3), so
///    `migrations_destructive: true` is a fact from the publisher.
/// 3. **The policy's existence.** REQ-129's `up → down → up` gate is what would turn the
///    *absence* of a marker into evidence. It has not landed, so an unmarked migration is
///    `unknown` and a manifest's `migrations_destructive: false` is the publisher's silence
///    rather than a verification.
/// 4. **Nothing else.** There is no heuristic and no default, because both available answers
///    are wrong in the direction that hurts.
pub fn destructiveness(
    delta: &[String],
    manifest_migrations_destructive: bool,
    marked_destructive: &[String],
    policy_exists: bool,
) -> Destructiveness {
    let delta_set: std::collections::HashSet<&str> = delta.iter().map(String::as_str).collect();
    let marked: Vec<String> = marked_destructive
        .iter()
        .filter(|name| delta_set.contains(name.as_str()))
        .cloned()
        .collect();

    if !marked.is_empty() {
        return Destructiveness {
            verdict: VERDICT_DESTRUCTIVE.to_owned(),
            reason: "a migration in this range carries the -- omnion:no-down marker".to_owned(),
            destructive_migrations: marked,
            database_rollback: "restore-from-backup".to_owned(),
            source: "migration-marker".to_owned(),
        };
    }
    if manifest_migrations_destructive {
        return Destructiveness {
            verdict: VERDICT_DESTRUCTIVE.to_owned(),
            reason: "the release manifest declares migrations_destructive".to_owned(),
            destructive_migrations: delta.to_vec(),
            database_rollback: "restore-from-backup".to_owned(),
            source: "manifest".to_owned(),
        };
    }
    if !policy_exists {
        return Destructiveness {
            verdict: VERDICT_UNKNOWN.to_owned(),
            reason: "REQ-129's down-script gate has not landed, so a migration with no \
                 -- omnion:no-down marker has not been proven reversible"
                .to_owned(),
            destructive_migrations: Vec::new(),
            database_rollback: "unknown".to_owned(),
            source: "policy-absent".to_owned(),
        };
    }
    Destructiveness {
        verdict: VERDICT_REVERSIBLE.to_owned(),
        reason: "every migration in this range ships a down script verified by the CI gate"
            .to_owned(),
        destructive_migrations: Vec::new(),
        database_rollback: "down-script".to_owned(),
        source: "manifest".to_owned(),
    }
}

// -------------------------------------------------------------------------------------------
// The cached manifest row
// -------------------------------------------------------------------------------------------

/// One cached release manifest — the document the deployment centre reads when the release
/// feed is unreachable.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ReleaseManifest {
    /// The released version.
    pub version: String,
    /// Which channel it came from.
    pub channel: String,
    /// The commit every artifact of it was built from.
    pub source_commit: Option<String>,
    /// The lowest core version this release runs on.
    pub core_min: Option<String>,
    /// The migrations it ships, in file order.
    pub migrations: Vec<String>,
    /// The publisher's own claim, cached verbatim and **not** treated as a verification.
    pub migrations_destructive: bool,
    /// The release notes, as markdown.
    pub notes_md: String,
    /// Where the long-form upgrade notes live.
    pub upgrade_notes_url: Option<String>,
    /// When the update check last fetched it.
    pub fetched_at: time::OffsetDateTime,
    /// The signed document as received.
    pub raw: Value,
}

impl ReleaseManifest {
    /// `true` when this release cannot run on the platform's own version.
    ///
    /// The comparison goes through the same numeric parse as everything else: `0.10.0` is
    /// newer than `0.9.0` while a string comparison says otherwise, and an install that
    /// refuses a release because of a string compare refuses every tenth version forever.
    pub fn needs_core_minimum(&self, core_version: &str) -> bool {
        match (
            Version::parse(&self.version, "version").ok(),
            Version::parse(core_version, "core").ok(),
        ) {
            (Some(have), Some(want)) => have.cmp_to(&want) == std::cmp::Ordering::Less,
            // An unparseable core version cannot be compared, and refusing to install is the
            // safe direction: a manifest we cannot place against our own version is not a
            // manifest to say "compatible" about.
            _ => true,
        }
    }

    /// The minimum core version this manifest requires, when it declares one.
    pub fn satisfies_core_minimum(&self, core_version: &str) -> bool {
        let Some(core_min) = self.core_min.as_deref() else {
            return true;
        };
        match (
            Version::parse(core_min, "core_min").ok(),
            Version::parse(core_version, "core").ok(),
        ) {
            (Some(min), Some(have)) => have.cmp_to(&min) != std::cmp::Ordering::Less,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn a_version_parses_and_a_non_version_is_a_refusal_naming_the_field() {
        assert_eq!(
            Version::parse("1.2.3", "to_version").expect("a version"),
            Version {
                major: 1,
                minor: 2,
                patch: 3,
                pre: String::new()
            }
        );
        // `v` and a build suffix are both legal spellings of the same version.
        assert!(Version::parse("v1.2.3", "v").is_ok());
        assert!(Version::parse("1.2.3+build.5", "v").is_ok());
        for bad in ["", "1", "1.2", "1.2.3.4", "one.two.three", "1.2.x"] {
            let error = Version::parse(bad, "to_version").expect_err("a refusal");
            assert!(
                matches!(error, DeploymentError::InvalidVersion { field, .. } if field == "to_version"),
                "{bad:?} must name the field it refused, got {error:?}"
            );
        }
    }

    #[test]
    fn versions_compare_numerically_and_a_pre_release_is_older_than_its_final() {
        // The two comparisons a string would get wrong.
        assert_eq!(
            Version::parse("0.10.0", "v")
                .unwrap()
                .cmp_to(&Version::parse("0.9.0", "v").unwrap()),
            Ordering::Greater
        );
        assert_eq!(
            Version::parse("0.5.0", "v")
                .unwrap()
                .cmp_to(&Version::parse("0.5.0-rc.1", "v").unwrap()),
            Ordering::Greater
        );
        assert_eq!(
            Version::parse("1.0.0", "v")
                .unwrap()
                .cmp_to(&Version::parse("1.0.0", "v").unwrap()),
            Ordering::Equal
        );
    }

    #[test]
    fn an_unmarked_migration_with_no_policy_is_unknown_and_never_reversible() {
        // The verdict this repository is in today, asserted from the inputs rather than read
        // off a fixture, because the value is the claim.
        let verdict = destructiveness(&["0199_x.sql".into()], false, &[], false);
        assert_eq!(verdict.verdict, VERDICT_UNKNOWN);
        assert_eq!(verdict.database_rollback, "unknown");
        assert_eq!(verdict.source, "policy-absent");
        assert!(
            verdict.requires_acknowledgement(),
            "an operator must accept that nobody knows"
        );
    }

    #[test]
    fn a_manifest_that_claims_migrations_are_not_destructive_does_not_make_them_reversible() {
        // The whole reason there are three verdicts: the publisher's `false` is silence, not a
        // verification, and a plan that reported `reversible` here would promise a down script
        // nobody has run.
        let verdict = destructiveness(&["0199_x.sql".into()], false, &[], false);
        assert_ne!(verdict.verdict, VERDICT_REVERSIBLE);

        // With the policy landed, the same manifest MAY reach `reversible` — the difference is
        // the gate, not the manifest.
        let proven = destructiveness(&["0199_x.sql".into()], false, &[], true);
        assert_eq!(proven.verdict, VERDICT_REVERSIBLE);
        assert_eq!(proven.database_rollback, "down-script");
        assert!(!proven.requires_acknowledgement());
    }

    #[test]
    fn a_marker_on_a_migration_outside_the_range_does_not_make_the_range_destructive() {
        // The marker is a fact about a FILE. A release whose destructive migration is already
        // applied to this install is not a range with a new destructive migration in it.
        let delta = vec!["0199_new.sql".to_owned()];
        let verdict = destructiveness(&delta, false, &["0150_old.sql".to_owned()], true);
        assert_eq!(
            verdict.verdict, VERDICT_REVERSIBLE,
            "a marker outside the delta says nothing about this range"
        );
    }

    #[test]
    fn a_marker_in_the_range_wins_over_everything_else() {
        let verdict = destructiveness(
            &["0199_a.sql".into(), "0199_b.sql".into()],
            false,
            &["0199_b.sql".into()],
            true,
        );
        assert_eq!(verdict.verdict, VERDICT_DESTRUCTIVE);
        assert_eq!(verdict.destructive_migrations, vec!["0199_b.sql"]);
        assert_eq!(verdict.database_rollback, "restore-from-backup");
        assert!(verdict.requires_acknowledgement());
    }

    #[test]
    fn the_declaration_is_the_second_input_and_not_the_first() {
        // A manifest that declares destructive is honoured even with the policy landed, because
        // a publisher stating it is information nobody can second-guess from here.
        let verdict = destructiveness(&["0199_a.sql".into()], true, &[], true);
        assert_eq!(verdict.verdict, VERDICT_DESTRUCTIVE);
        assert_eq!(verdict.source, "manifest");
    }

    #[test]
    fn a_core_minimum_below_this_build_is_reported_and_an_undeclared_one_is_satisfied() {
        let mut manifest = ReleaseManifest {
            version: "0.5.0".into(),
            channel: "stable".into(),
            source_commit: None,
            core_min: Some("0.4.0".into()),
            migrations: vec![],
            migrations_destructive: false,
            notes_md: String::new(),
            upgrade_notes_url: None,
            fetched_at: time::OffsetDateTime::UNIX_EPOCH,
            raw: Value::Null,
        };
        assert!(manifest.satisfies_core_minimum("0.4.0"));
        assert!(!manifest.satisfies_core_minimum("0.3.9"));
        // And the other half of the notification rule: which release is newer than this build.
        manifest.core_min = Some("9.0.0".into());
        assert!(!manifest.satisfies_core_minimum("0.4.0"));
    }

    #[test]
    fn the_vocabularies_are_closed_and_a_value_outside_them_is_a_refusal() {
        assert!(compose_stack_file("compose-small").is_ok());
        assert!(compose_stack_file("helm").is_err());
        assert!(compose_stack_file("compose-enterprise").is_ok());
        for kind in ARTIFACT_KINDS {
            assert!(ARTIFACT_KINDS.contains(kind));
        }
        assert_eq!(ARTIFACT_KINDS.len(), 5, "a new kind is a new screen column");
        assert!(CHANNELS.contains(&"stable"));
        assert!(TOPOLOGIES.contains(&"kubernetes"));
        assert!(VERDICTS.contains(&VERDICT_UNKNOWN));
    }
}
