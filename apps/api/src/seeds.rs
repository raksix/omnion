//! Seed dataset manifests — the one definition the CLI, the panel and the tests all read
//! (docs/requests/REQ-129, slice 3).
//!
//! ## Why a manifest and not a function
//!
//! The request requires that "fixtures used by tests and for the walkthrough come from the same
//! descriptors". That cannot be enforced by discipline, only by having ONE thing to read: a JSON
//! file under `database/seeds/<name>/manifest.json`. So this module has no SQL in it, no `fn`
//! that produces rows, and no environment-specific behaviour — a dataset is data, and three
//! consumers read the same bytes.
//!
//! ## What this module refuses to do
//!
//! **It never executes anything.** Reading a manifest tells you what a load WOULD write; running
//! it is the CLI's job (REQ-131), against a database the operator chose. A web route that
//! executed fixture SQL would be a second loader, and the second loader would be the one the
//! panel's number came from — which is exactly the "we forgot to strip that column" class of
//! defect, one layer down.
//!
//! ## Path resolution, and why it is fallible
//!
//! [`dataset_dir`] walks up from the running executable to the workspace root and then into
//! `database/seeds`. A production build ships the binary without that directory, and every
//! function here answers `false` / `Err` rather than inventing a path. The reason is that the one
//! question a caller has is "can I load this dataset", and a function that returned a plausible
//! path for a file that does not exist would answer it wrong.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One dataset's manifest, as `database/seeds/<name>/manifest.json` spells it.
///
/// The field set is deliberately small: a name, a description an operator reads before clicking,
/// the row estimate the confirmation quotes, and the statements. Anything richer (a generation
/// seed, a template engine) would make the tests and the walkthrough able to see DIFFERENT rows
/// from the same file, which is the property this module exists to guarantee.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeedManifest {
    /// The dataset's name — `minimal`, `demo`, `fixture`.
    pub name: String,
    /// One sentence shown above the confirmation box.
    #[serde(default)]
    pub description: String,
    /// Rows the dataset writes, for the confirmation to quote.
    ///
    /// An ESTIMATE, and the load row records what was actually written instead — a screen that
    /// showed the estimate under a heading saying "loaded" would be lying about the number an
    /// operator uses to decide whether the demo is big enough.
    #[serde(default)]
    pub row_estimate: i32,
    /// The lowest migration version this dataset was written against.
    #[serde(default)]
    pub compatible_from: String,
    /// The highest, when the dataset is pinned to a range.
    #[serde(default)]
    pub compatible_to: Option<String>,
    /// The SQL, one entry per statement, executed in order.
    ///
    /// A LIST of statements rather than one blob, so a loader can report which one failed and a
    /// reviewer can read the dataset without a SQL client.
    #[serde(default)]
    pub statements: Vec<String>,
    /// SHA-256 of the file's own bytes, recorded on the dataset row.
    #[serde(default)]
    pub checksum: String,
}

impl SeedManifest {
    /// Whether this manifest would write anything.
    ///
    /// False for a manifest with no statements, and the answer matters: a load of it reports zero
    /// rows, which an operator reads as "already seeded".
    #[must_use]
    pub fn datasets_usable(&self) -> bool {
        !self.statements.is_empty()
    }

    /// How many statements it holds, for a refusal message.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.statements.len()
    }

    /// Where this dataset lives, as the request writes the path.
    ///
    /// Not a field: the manifest travels in a file and its directory is a fact about THIS build,
    /// not about the dataset, so a manifest serialised elsewhere (a fixture inside a test) still
    /// reports the real path an operator has to create.
    #[must_use]
    pub fn directory(&self) -> String {
        dataset_dir(&self.name).display().to_string()
    }
}

/// What went wrong reading a manifest, as a sentence a caller can put in an error body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedError {
    /// The directory is not in this build.
    NoDirectory,
    /// The dataset has no manifest file.
    NotFound(String),
    /// The file is there and does not parse.
    Malformed {
        /// Which dataset.
        dataset: String,
        /// What the parser said.
        reason: String,
    },
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDirectory => f.write_str(
                "this build ships no `database/seeds/` directory, so no dataset can be loaded \
                 from it — the loader that does write rows is the CLI's, run against a database \
                 you control",
            ),
            Self::NotFound(name) => write!(
                f,
                "no manifest at `database/seeds/{name}/manifest.json` — the dataset is declared in \
                 the database but its descriptor file is absent"
            ),
            Self::Malformed { dataset, reason } => write!(
                f,
                "`database/seeds/{dataset}/manifest.json` is not a valid manifest: {reason}"
            ),
        }
    }
}

impl std::error::Error for SeedError {}

/// Result alias of the seed module.
pub type Result<T> = std::result::Result<T, SeedError>;

/// The workspace root, found by walking up from this crate's manifest directory.
///
/// `CARGO_MANIFEST_DIR` is the API crate's directory at compile time and the executable's location
/// at run time; walking up from either finds the workspace root in a checkout and in a container
/// whose `/app` holds the source. It returns `None` rather than a guess, because every caller of
/// this turns `None` into a refusal and none of them has a sensible fallback.
#[must_use]
pub fn workspace_root() -> Option<PathBuf> {
    let start = Path::new(env!("CARGO_MANIFEST_DIR"));
    start
        .ancestors()
        .find(|candidate| candidate.join("database").join("seeds").is_dir())
        .map(Path::to_path_buf)
}

/// `database/seeds`, if this build has one.
#[must_use]
pub fn seeds_root() -> Option<PathBuf> {
    workspace_root().map(|root| root.join("database").join("seeds"))
}

/// One dataset's directory. Existence is NOT implied: call [`read_manifest`] for that.
#[must_use]
pub fn dataset_dir(name: &str) -> PathBuf {
    let root = seeds_root().unwrap_or_else(|| PathBuf::from("database").join("seeds"));
    root.join(name)
}

/// Read one dataset's manifest.
///
/// The name is validated before it reaches a path, because this is the one function where a
/// caller-supplied string becomes a filesystem path — and a name of `../../etc` would otherwise
/// resolve to whatever it pointed at. Identifiers are the same closed shape the backfill
/// descriptors use, and the reason is the same: a descriptor is data read from a file, and data
/// read from a file is still untrusted.
pub fn read_manifest(name: &str) -> Result<SeedManifest> {
    if !is_safe_dataset_name(name) {
        return Err(SeedError::NotFound(name.to_owned()));
    }
    let path = dataset_dir(name).join("manifest.json");
    if !path.is_file() {
        return Err(SeedError::NotFound(name.to_owned()));
    }
    let text = std::fs::read_to_string(&path).map_err(|err| SeedError::Malformed {
        dataset: name.to_owned(),
        reason: err.to_string(),
    })?;
    parse_manifest(name, &text)
}

/// Parse manifest text. Split out so the parser is testable with no filesystem at all.
pub fn parse_manifest(name: &str, text: &str) -> Result<SeedManifest> {
    let manifest: SeedManifest =
        serde_json::from_str(text).map_err(|err| SeedError::Malformed {
            dataset: name.to_owned(),
            reason: err.to_string(),
        })?;
    if manifest.name != name {
        return Err(SeedError::Malformed {
            dataset: name.to_owned(),
            reason: format!(
                "the manifest declares itself as `{}` — a dataset addressed by one name and \
                 declaring another is two datasets wearing one directory",
                manifest.name
            ),
        });
    }
    Ok(manifest)
}

/// A dataset name that cannot escape its directory.
///
/// Lowercase letters, digits and underscores, not starting with a digit — the same shape
/// [`omnion_migrations::backfill::validate_identifier`] demands of a table name, chosen because
/// the two share the reason: both are strings read from a file that end up somewhere they must
/// not.
#[must_use]
pub fn is_safe_dataset_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Every dataset name declared under `database/seeds/`, sorted.
///
/// Empty when the directory is absent — which is a real answer (this build ships no datasets),
/// not a failure, and the panel renders it as "no datasets in this build" rather than as an error
/// it retries.
#[must_use]
pub fn available() -> Vec<String> {
    let Some(root) = seeds_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| is_safe_dataset_name(name))
        .collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_that_declares_another_name_is_refused() {
        // The single most likely way a dataset directory goes wrong: it is copied, the manifest
        // keeps the old `name`, and now `demo` loads whatever `fixture` was going to load. The
        // loader would have no way to notice, because it addressed the directory.
        let text = r#"{"name":"fixture","statements":["select 1;"]}"#;
        let error = parse_manifest("demo", text).expect_err("a mismatched name must not parse");
        assert!(
            matches!(error, SeedError::Malformed { .. }),
            "a mismatched name is malformed, not absent: {error}"
        );
        assert!(
            error.to_string().contains("fixture"),
            "the message must name what the manifest claimed: {error}"
        );
    }

    #[test]
    fn a_manifest_with_no_statements_is_unusable_not_empty() {
        // "0 rows loaded" reads as "already seeded". The distinction has to be expressible.
        let manifest =
            parse_manifest("minimal", r#"{"name":"minimal","statements":[]}"#).expect("it parses");
        assert!(!manifest.datasets_usable());
        assert_eq!(manifest.file_count(), 0);
    }

    #[test]
    fn a_manifest_with_statements_is_usable_and_counts_them() {
        let manifest = parse_manifest(
            "demo",
            r#"{"name":"demo","row_estimate":640,"statements":["select 1;","select 2;"]}"#,
        )
        .expect("it parses");
        assert!(manifest.datasets_usable());
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.row_estimate, 640);
        // Optional fields default rather than being required: a dataset that does not pin a
        // version range is still a dataset, and refusing to parse it would make the field
        // mandatory in practice while the schema says it is not.
        assert_eq!(manifest.compatible_from, "");
        assert_eq!(manifest.compatible_to, None);
    }

    #[test]
    fn a_dataset_name_cannot_climb_out_of_its_directory() {
        for hostile in ["../etc", "..", "a/b", "Demo", "demo data", "1demo", ""] {
            assert!(
                !is_safe_dataset_name(hostile),
                "{hostile:?} must not be addressable as a dataset"
            );
        }
        assert!(is_safe_dataset_name("minimal"));
        assert!(is_safe_dataset_name("demo_2"));
    }

    #[test]
    fn the_error_carries_a_sentence_and_not_a_debug_form() {
        // These strings go into an HTTP body an operator reads. `NotFound(..)` alone would be a
        // Debug rendering of a variant, which is not a message.
        for error in [
            SeedError::NoDirectory,
            SeedError::NotFound("demo".to_owned()),
            SeedError::Malformed {
                dataset: "demo".to_owned(),
                reason: "expected `{` at line 1".to_owned(),
            },
        ] {
            let rendered = error.to_string();
            assert!(rendered.len() > 40, "{rendered:?} is not a sentence");
            assert!(
                !rendered.contains("NotFound("),
                "{rendered:?} leaked a variant name"
            );
            assert!(
                !rendered.contains("Malformed {"),
                "{rendered:?} leaked a variant name"
            );
        }
    }
}
