//! The five parts, a run, and the rules that decide what a run may contain (REQ-013).
//!
//! Everything in this module is a pure function of its arguments: no database, no storage,
//! no clock. That is not an aesthetic choice, it is the reason the interesting rules are
//! testable at all, and there is one rule here that a test cannot find any other way.
//!
//! **A part that produced nothing is still a part.** The obvious model is a list of
//! artifacts — "the run wrote these files" — and it is wrong for the platform's own
//! library: `plugins` is an honest empty result until a package installer exists, and a
//! manifest that lists only what worked cannot answer the question the restore wizard asks
//! first, which is *was media part of this run at all*. So a part is a value with a status
//! and a count, `0` and `0` is a legitimate outcome, and the *distinction* between
//! "produced nothing" and "was never attempted" is carried by the part's presence rather
//! than inferred from its size.
//!
//! Five rules, each a place the obvious shortcut is wrong:
//!
//! * **`partial` is a terminal state of its own, and it is a *result* rather than an error.**
//! A five-part run with four successes is not `failed`, because that throws away four
//! artifacts an operator can still restore from, and it is not `succeeded`, because the
//! restore wizard would then act on a claim it cannot verify. `summarise` derives the
//! status from the parts rather than accepting the caller's word for it, so a run cannot
//! reach `succeeded` by assertion.
//! * **The order of the parts is fixed, and it is the order that fails cheapest first.**
//! `database` runs first because it is the one part a restore cannot proceed without;
//! `media` second because it is the largest; `plugins` last because it is the part that
//! most often has nothing to do. A run that is cut short by a full destination has
//! therefore already written the thing worth keeping.
//! * **A scope list is a *set* even though it is a list.** Duplicates are refused rather
//!   than deduplicated, because `{database, database}` is a caller bug and silently
//!   repairing it is how a run ends up exporting the database twice and reporting the
//!   second attempt as a failure against its own first output.
//! * **A label is optional and an empty one is not a defect.** The screen renders the
//!   created instant as the title, and an operator who wants the timestamp and nothing
//!   else should not have to invent a name. The length bound exists because a label is
//!   rendered in a table cell next to four other columns.
//! * **The manifest's own checksum covers the manifest *as serialised*, not the parts.** A
//!   checksum that mixed the two would change whenever a part was re-verified, so a
//!   `verify` would report a mismatch against the very run it had just confirmed. The
//!   canonical form is sorted, so two runs that produced the same parts in a different
//!   order agree — which is the property the verification path actually depends on.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{BackupError, Result};

/// The five parts a backup can carry, in the order a run executes them.
pub const PARTS: [&str; 5] = ["database", "media", "configuration", "themes", "plugins"];

/// Longest a label may be. It is rendered in a table cell beside four other columns.
pub const MAX_LABEL_LENGTH: usize = 80;

/// Longest a part's error message may be stored at.
///
/// A producer's error is written to the row and shown in a table cell. A stack trace or a
/// multi-kilobyte driver message would push every other column off the row, and the
/// message that matters — the first line, the cause — is at the front.
pub const MAX_ERROR_LENGTH: usize = 500;

/// One part's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartStatus {
    /// Not started.
    Queued,
    /// Being produced.
    Running,
    /// Produced, with a size and a checksum.
    Done,
    /// Refused, with a reason.
    Failed,
}

impl PartStatus {
    /// The value stored in `backup_parts.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Read a stored status, refusing an unknown one.
    ///
    /// A row whose status is not one of the four is a row the platform cannot render, and
    /// `as_str` above would have silently answered "queued" for it — turning a database
    /// that disagrees with the code into a run that looks like it never started.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(BackupError::Invalid(format!(
                "part status `{other}` is not one of queued, running, done, failed"
            ))),
        }
    }
}

/// A run's terminal status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    /// Accepted, not started.
    Queued,
    /// In flight.
    Running,
    /// Every asked-for part produced its artifact.
    Succeeded,
    /// At least one part produced its artifact and at least one did not.
    Partial,
    /// No part produced its artifact.
    Failed,
}

impl RunStatus {
    /// The value stored in `backups.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    /// Read a stored status, refusing an unknown one.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "partial" => Ok(Self::Partial),
            "failed" => Ok(Self::Failed),
            other => Err(BackupError::Invalid(format!(
                "run status `{other}` is not one of queued, running, succeeded, partial, failed"
            ))),
        }
    }
}

/// One part of a run, as the producers and the detail screen see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// Which part this is: one of [`PARTS`].
    pub part: String,
    /// Where it got to.
    pub status: PartStatus,
    /// How many things it accounted for — rows for `database`, objects for `media`.
    #[serde(default)]
    pub item_count: i32,
    /// How many bytes the artifact occupies.
    #[serde(default)]
    pub size_bytes: i64,
    /// Hex SHA-256 of the artifact, once it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// The artifact's key under the run's prefix, once it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Part {
    /// A part that has not started.
    #[must_use]
    pub fn queued(part: &str) -> Self {
        Self {
            part: part.to_owned(),
            status: PartStatus::Queued,
            item_count: 0,
            size_bytes: 0,
            checksum: None,
            storage_path: None,
            error: None,
        }
    }

    /// A part that produced `item_count` things and `size_bytes` bytes at `storage_path`,
    /// with `checksum` over the bytes.
    #[must_use]
    pub fn done(
        part: &str,
        item_count: i32,
        size_bytes: i64,
        checksum: impl Into<String>,
        storage_path: impl Into<String>,
    ) -> Self {
        Self {
            part: part.to_owned(),
            status: PartStatus::Done,
            item_count,
            size_bytes,
            checksum: Some(checksum.into()),
            storage_path: Some(storage_path.into()),
            error: None,
        }
    }

    /// A part that was attempted and refused. An empty result is **not** this: a part that
    /// found nothing is [`Part::done`] with zero of everything, and the difference is the
    /// whole reason the part is a row.
    #[must_use]
    pub fn failed(part: &str, error: impl AsRef<str>) -> Self {
        Self {
            part: part.to_owned(),
            status: PartStatus::Failed,
            item_count: 0,
            size_bytes: 0,
            checksum: None,
            storage_path: None,
            error: Some(truncate_error(error.as_ref())),
        }
    }

    /// Whether this part produced a readable artifact.
    #[must_use]
    pub fn is_readable(&self) -> bool {
        self.status == PartStatus::Done && self.storage_path.is_some() && self.checksum.is_some()
    }
}

/// Clamp a producer's message to [`MAX_ERROR_LENGTH`] on a character boundary.
///
/// Truncating at a byte index in the middle of a multi-byte character produces a `String`
/// that cannot be stored — and the failure appears at the *insert*, so the error message
/// about the message is what the operator sees. This walks to a boundary instead.
#[must_use]
pub fn truncate_error(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_LENGTH {
        return message.to_owned();
    }
    let kept: String = message.chars().take(MAX_ERROR_LENGTH).collect();
    let mut out = kept;
    out.push('…');
    out
}

/// Check a scope list and return it in [`PARTS`] order.
///
/// Refuses, rather than repairs: see the module note. The returned order is the *execution*
/// order, so two runs asking for the same parts always write them the same way round — which
/// is what makes a manifest comparison between two runs meaningful.
pub fn normalise_scopes(scopes: &[String]) -> Result<Vec<String>> {
    if scopes.is_empty() {
        return Err(BackupError::Invalid(
            "scopes: choose at least one part to back up".to_owned(),
        ));
    }
    if scopes.len() > PARTS.len() {
        return Err(BackupError::Invalid(format!(
            "scopes: at most {} parts may be requested, {} were given",
            PARTS.len(),
            scopes.len()
        )));
    }
    let mut unknown: Vec<&str> = scopes
        .iter()
        .map(String::as_str)
        .filter(|scope| !PARTS.contains(scope))
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return Err(BackupError::Invalid(format!(
            "scopes: {} is not one of {}",
            unknown.join(", "),
            PARTS.join(", ")
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(scopes.len());
    for scope in scopes.iter().map(String::as_str) {
        if seen.contains(&scope) {
            return Err(BackupError::Invalid(format!(
                "scopes: `{scope}` is listed twice"
            )));
        }
        seen.push(scope);
    }
    Ok(PARTS
        .iter()
        .filter(|part| scopes.iter().any(|scope| scope == *part))
        .map(|part| (*part).to_owned())
        .collect())
}

/// Check a label. An empty one is legal; see the module note.
pub fn validate_label(label: &str) -> Result<()> {
    let trimmed = label.trim();
    if trimmed.chars().count() > MAX_LABEL_LENGTH {
        return Err(BackupError::Invalid(format!(
            "label: at most {MAX_LABEL_LENGTH} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(BackupError::Invalid(
            "label: control characters are not allowed".to_owned(),
        ));
    }
    Ok(())
}

/// Derive a run's terminal status from its parts.
///
/// The caller's own status is ignored, and that is the point: a run reaches `succeeded`
/// because every asked-for part produced its artifact, never because a handler wrote the
/// word. The three-way result is the module's second rule.
#[must_use]
pub fn summarise(parts: &[Part]) -> RunStatus {
    let mut done = 0_i32;
    let mut failed = 0_i32;
    for part in parts {
        match part.status {
            PartStatus::Done => done += 1,
            PartStatus::Failed => failed += 1,
            PartStatus::Queued | PartStatus::Running => {}
        }
    }
    if failed == 0 && done == 0 {
        // Nothing finished: the run is still in flight, whatever the caller called it.
        return RunStatus::Running;
    }
    if failed == 0 {
        return RunStatus::Succeeded;
    }
    if done == 0 {
        return RunStatus::Failed;
    }
    RunStatus::Partial
}

/// The run-level sentence a detail panel and the runs list show for a run that is not green.
///
/// [`summarise`] answers *which* state a run is in; this answers *why*, and the two were
/// separate questions with one implementation's worth of disagreement. The run's `error`
/// column was written from `find(...)` — the first failed part's message — so a run whose
/// media copy and whose database export both failed named one of them. That is the wrong
/// half to keep, and it is the wrong half precisely because a `partial` run *is* the
/// multi-failure case: `partial` is only reachable when at least one part succeeded and at
/// least one failed, so the shape the summary drops is the shape the state exists for.
///
/// Every message is kept, joined in [`PARTS`] order so two runs with the same failures
/// produce the same sentence, and clamped with [`truncate_error`] because the column is
/// `text` and a producer's message is not bounded before it arrives here. `None` when no
/// part failed — a green run has nothing to explain, and a run whose only failures carry no
/// message says nothing rather than a bare separator.
#[must_use]
pub fn summarise_failure(parts: &[Part]) -> Option<String> {
    let mut ordered: Vec<&Part> = parts
        .iter()
        .filter(|part| part.status == PartStatus::Failed)
        .collect();
    ordered.sort_by_key(|part| {
        PARTS
            .iter()
            .position(|candidate| *candidate == part.part)
            .unwrap_or(usize::MAX)
    });
    let messages: Vec<&str> = ordered
        .iter()
        .filter_map(|part| part.error.as_deref())
        .filter(|message| !message.trim().is_empty())
        .collect();
    if messages.is_empty() {
        None
    } else {
        Some(truncate_error(&messages.join("; ")))
    }
}

/// The manifest a run leaves behind, and the thing `verify` compares against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The run this manifest describes.
    pub backup_id: String,
    /// The schema version, so a verifier from a later release can refuse an old manifest
    /// rather than read fields it thinks it knows.
    pub version: u32,
    /// The parts, in [`PARTS`] order.
    pub parts: Vec<Part>,
    /// When the manifest was written, RFC 3339 in UTC.
    pub created_at: String,
}

/// The manifest schema version this build writes.
pub const MANIFEST_VERSION: u32 = 1;

/// Build a manifest for a run.
#[must_use]
pub fn build_manifest(backup_id: &str, parts: &[Part], created_at: &str) -> Manifest {
    let mut ordered: Vec<Part> = parts.to_vec();
    ordered.sort_by_key(|part| {
        PARTS
            .iter()
            .position(|candidate| *candidate == part.part)
            .unwrap_or(usize::MAX)
    });
    Manifest {
        backup_id: backup_id.to_owned(),
        version: MANIFEST_VERSION,
        parts: ordered,
        created_at: created_at.to_owned(),
    }
}

/// The canonical bytes a manifest's checksum is taken over.
///
/// Canonical means: the parts sorted, and every optional field present or absent by a rule
/// rather than by how the value arrived. Two runs that produced the same artifacts therefore
/// serialise identically, which is the property `verify` depends on — a checksum that changed
/// because a field happened to be `Some("")` on one run and `None` on another would report a
/// mismatch against two perfectly matching sets of bytes.
#[must_use]
pub fn canonical_json(manifest: &Manifest) -> String {
    let mut parts = manifest.parts.clone();
    parts.sort_by(|a, b| a.part.cmp(&b.part));
    let normalised: Vec<serde_json::Value> = parts
        .iter()
        .map(|part| {
            serde_json::json!({
                "part": part.part,
                "status": part.status.as_str(),
                "item_count": part.item_count,
                "size_bytes": part.size_bytes,
                "checksum": part.checksum,
                "storage_path": part.storage_path,
            })
        })
        .collect();
    serde_json::json!({
        "version": manifest.version,
        "backup_id": manifest.backup_id,
        "created_at": manifest.created_at,
        "parts": normalised,
    })
    .to_string()
}

/// The hex SHA-256 of a manifest's canonical form.
#[must_use]
pub fn manifest_checksum(manifest: &Manifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(manifest).as_bytes());
    hex::encode(hasher.finalize())
}

/// The hex SHA-256 of an artifact's bytes.
#[must_use]
pub fn bytes_checksum(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// What one part's artifact looked like when it was re-read off the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPart {
    /// Which part was re-read.
    pub part: String,
    /// The bytes' SHA-256, hex.
    pub checksum: String,
    /// How many bytes the destination actually held.
    pub size_bytes: i64,
}

/// What a verification pass found.
///
/// `Ok` here means *the whole check ran*; the findings are in the struct. A verification that
/// discovered a mismatch is not an error to propagate — it is the answer the operator asked
/// for, and returning it as `Err` would make the screen say "verification failed" without
/// saying *what* failed, which is the sentence they need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    /// Parts whose recorded checksum and size both match what the destination holds.
    pub matched: Vec<String>,
    /// Parts whose recorded checksum does not match.
    pub mismatched: Vec<String>,
    /// Parts that were recorded but could not be read back at all.
    pub unreadable: Vec<String>,
    /// Parts recorded in the manifest that the run never asked for.
    pub unexpected: Vec<String>,
}

impl Verification {
    /// Whether every part the manifest recorded was re-read and matched.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.mismatched.is_empty() && self.unreadable.is_empty() && self.unexpected.is_empty()
    }

    /// One sentence for the screen's result line, naming what is wrong rather than only
    /// that something is.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_clean() {
            return format!("All {} parts match the manifest.", self.matched.len());
        }
        let mut parts: Vec<String> = Vec::new();
        if !self.mismatched.is_empty() {
            parts.push(format!(
                "checksum differs for {} ({})",
                count_or_all(self.mismatched.len()),
                self.mismatched.join(", ")
            ));
        }
        if !self.unreadable.is_empty() {
            parts.push(format!(
                "could not be read back: {}",
                self.unreadable.join(", ")
            ));
        }
        if !self.unexpected.is_empty() {
            parts.push(format!(
                "not part of this run: {}",
                self.unexpected.join(", ")
            ));
        }
        parts.join("; ")
    }
}

fn count_or_all(count: usize) -> String {
    if count == 1 {
        "1 part".to_owned()
    } else {
        format!("{count} parts")
    }
}

/// Compare a manifest against what the destination actually holds.
///
/// The comparison is by **checksum and size together**, and both are needed: a checksum over
/// an empty body matches the checksum of an empty body, so a truncated artifact that happens
/// to have lost its whole content can agree on the hash while the size says otherwise. The
/// obvious check — hash equality alone — passes on the one corruption that matters most,
/// which is the empty file.
pub fn verify_manifest(manifest: &Manifest, observed: &[ObservedPart]) -> Verification {
    let mut matched = Vec::new();
    let mut mismatched = Vec::new();
    let mut unreadable = Vec::new();
    let mut unexpected: Vec<String> = observed
        .iter()
        .filter(|item| !manifest.parts.iter().any(|part| part.part == item.part))
        .map(|item| item.part.clone())
        .collect();
    unexpected.sort();

    for part in &manifest.parts {
        if part.status != PartStatus::Done {
            // A part that did not produce an artifact is not a missing artifact. Counting it
            // as one turns every `partial` run into a verification failure that says
            // "could not be read back" about a part that was never written.
            continue;
        }
        match observed.iter().find(|item| item.part == part.part) {
            None => unreadable.push(part.part.clone()),
            Some(item) => {
                let size_matches = part.size_bytes == item.size_bytes;
                let checksum_matches = part.checksum.as_deref() == Some(item.checksum.as_str());
                if size_matches && checksum_matches {
                    matched.push(part.part.clone());
                } else {
                    mismatched.push(part.part.clone());
                }
            }
        }
    }
    matched.sort();
    mismatched.sort();
    unreadable.sort();
    Verification {
        matched,
        mismatched,
        unreadable,
        unexpected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str) -> Part {
        Part::queued(name)
    }

    #[test]
    fn scopes_are_returned_in_execution_order_not_caller_order() {
        let out = normalise_scopes(&["plugins".to_owned(), "database".to_owned()]).unwrap();
        assert_eq!(out, vec!["database".to_owned(), "plugins".to_owned()]);
    }

    #[test]
    fn a_scope_listed_twice_is_refused_rather_than_repaired() {
        let err = normalise_scopes(&["database".to_owned(), "database".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    #[test]
    fn an_empty_scope_list_is_refused() {
        let err = normalise_scopes(&[]).unwrap_err();
        assert!(err.to_string().contains("at least one"), "{err}");
    }

    #[test]
    fn an_unknown_scope_names_itself_and_the_five() {
        let err = normalise_scopes(&["database".to_owned(), "mailbox".to_owned()]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("mailbox"), "{text}");
        assert!(text.contains("plugins"), "{text}");
    }

    #[test]
    fn every_scope_is_refused_when_unknown_so_the_list_names_them_all() {
        let err = normalise_scopes(&["mailbox".to_owned(), "secrets".to_owned()]).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("mailbox") && text.contains("secrets"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_label_is_legal_and_a_long_one_is_not() {
        assert!(validate_label("").is_ok());
        assert!(validate_label("nightly before the migration").is_ok());
        let err = validate_label(&"x".repeat(MAX_LABEL_LENGTH + 1)).unwrap_err();
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[test]
    fn a_label_of_max_length_is_accepted() {
        assert!(validate_label(&"x".repeat(MAX_LABEL_LENGTH)).is_ok());
    }

    #[test]
    fn a_label_is_measured_in_characters_not_bytes() {
        // 80 four-byte characters is 320 bytes, and it is exactly at the limit in characters.
        assert!(validate_label(&"ş".repeat(MAX_LABEL_LENGTH)).is_ok());
        let err = validate_label(&"ş".repeat(MAX_LABEL_LENGTH + 1)).unwrap_err();
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[test]
    fn a_label_with_a_newline_is_refused() {
        assert!(validate_label("nightly\nrm -rf").is_err());
    }

    #[test]
    fn summarise_reads_the_parts_not_the_caller() {
        let all_done = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::done("media", 2, 200, "bb", "m"),
        ];
        assert_eq!(summarise(&all_done), RunStatus::Succeeded);
    }

    #[test]
    fn four_successes_and_one_failure_is_partial_not_failed() {
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::done("media", 2, 200, "bb", "m"),
            Part::done("configuration", 5, 5, "cc", "c"),
            Part::done("themes", 1, 1, "dd", "t"),
            Part::failed("plugins", "no package installer"),
        ];
        assert_eq!(summarise(&parts), RunStatus::Partial);
    }

    #[test]
    fn one_success_and_the_rest_failing_is_still_partial() {
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::failed("media", "object store refused"),
            Part::failed("plugins", "no package installer"),
        ];
        assert_eq!(summarise(&parts), RunStatus::Partial);
    }

    #[test]
    fn every_part_failing_is_failed() {
        let parts = vec![Part::failed("media", "x"), Part::failed("plugins", "y")];
        assert_eq!(summarise(&parts), RunStatus::Failed);
    }

    #[test]
    fn no_part_finished_is_running_whatever_the_caller_said() {
        let parts = vec![Part::queued("database"), Part::queued("media")];
        assert_eq!(summarise(&parts), RunStatus::Running);
    }

    // ---- `summarise_failure`, the "why" behind `summarise`'s "which".
    //
    // Each test states the claim its name states, because the defect this function fixes was
    // invisible to every assertion that already existed: `summarise` reported the state
    // correctly throughout, and the state is not what was wrong. A test on `summarise` can
    // never see a sentence that lost a message.

    #[test]
    fn a_green_run_has_nothing_to_explain() {
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::done("media", 2, 200, "bb", "m"),
        ];
        assert_eq!(summarise_failure(&parts), None);
    }

    #[test]
    fn every_failed_part_is_named_not_only_the_first() {
        // This is the defect. `find` returned the FIRST failed part's message and discarded
        // the rest, so this run — the exact shape `partial` exists for — reported one of its
        // two failures. Both words have to be in the sentence.
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::failed("media", "object store refused"),
            Part::failed("plugins", "no package installer"),
        ];
        assert_eq!(summarise(&parts), RunStatus::Partial);
        let summary = summarise_failure(&parts).expect("a failed run has a reason");
        assert!(
            summary.contains("object store refused"),
            "the media failure is missing from {summary:?}"
        );
        assert!(
            summary.contains("no package installer"),
            "the plugins failure is missing from {summary:?}"
        );
    }

    #[test]
    fn the_summary_is_ordered_by_parts_not_by_who_failed_first() {
        // Given in the reverse of execution order, so an implementation that merely
        // concatenates whatever it is handed produces a different string. The run's error
        // has to be the same sentence for the same set of failures whatever order the parts
        // were walked in, or two identical runs are not comparable.
        let parts = vec![
            Part::done("configuration", 5, 5, "cc", "c"),
            Part::failed("plugins", "no package installer"),
            Part::failed("media", "object store refused"),
        ];
        assert_eq!(
            summarise_failure(&parts).as_deref(),
            Some("object store refused; no package installer"),
            "media is produced before plugins in PARTS order"
        );
    }

    #[test]
    fn a_failed_part_with_no_message_does_not_leave_a_bare_separator() {
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::failed("media", ""),
            Part::failed("plugins", "no package installer"),
        ];
        assert_eq!(
            summarise_failure(&parts).as_deref(),
            Some("no package installer"),
            "an empty message must not contribute \"; \" to the sentence"
        );
    }

    #[test]
    fn a_run_whose_only_failures_are_silent_says_nothing_rather_than_a_separator() {
        let parts = vec![
            Part::done("database", 10, 100, "aa", "d"),
            Part::failed("media", "   "),
        ];
        assert_eq!(summarise_failure(&parts), None);
    }

    #[test]
    fn the_summary_is_clamped_on_a_character_boundary_like_every_other_message() {
        // The column is `text` and a producer's message is unbounded before it arrives, so a
        // long one is clamped here exactly as a part's is. Multi-byte input is the case that
        // matters: clamping at a byte index would produce a string that cannot be stored, and
        // the operator would read an error about the error message.
        let long = "ş".repeat(MAX_ERROR_LENGTH + 40);
        let parts = vec![
            Part::done("database", 1, 1, "aa", "d"),
            Part::failed("media", &long),
        ];
        let summary = summarise_failure(&parts).expect("a failed part has a reason");
        assert_eq!(
            summary.chars().count(),
            MAX_ERROR_LENGTH + 1,
            "clamped to the cap plus the ellipsis, on a boundary"
        );
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn a_part_that_produced_nothing_is_done_and_readable() {
        // The distinction the whole module exists for: plugins before an installer exists.
        let empty = Part::done("plugins", 0, 0, "e3b0c442...", "plugins/manifest.json");
        assert_eq!(empty.status, PartStatus::Done);
        assert!(empty.is_readable());
        assert_ne!(Part::failed("plugins", "no installer"), empty);
    }

    #[test]
    fn a_failed_part_is_not_readable_even_with_a_path() {
        let mut broken = Part::failed("media", "store refused");
        broken.storage_path = Some("media.tar".to_owned());
        assert!(!broken.is_readable());
    }

    #[test]
    fn a_long_error_is_truncated_on_a_character_boundary() {
        let message = "ş".repeat(MAX_ERROR_LENGTH + 50);
        let stored = truncate_error(&message);
        // The point is that this is a String at all: truncating at a byte index inside a
        // multi-byte character is not, and the failure would surface at the insert.
        assert!(stored.chars().count() <= MAX_ERROR_LENGTH + 1);
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn a_short_error_is_untouched() {
        assert_eq!(truncate_error("store refused"), "store refused");
    }

    #[test]
    fn statuses_round_trip_and_an_unknown_one_is_refused() {
        for status in [
            PartStatus::Queued,
            PartStatus::Running,
            PartStatus::Done,
            PartStatus::Failed,
        ] {
            assert_eq!(PartStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(PartStatus::parse("halfway").is_err());
        for status in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::Partial,
            RunStatus::Failed,
        ] {
            assert_eq!(RunStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(RunStatus::parse("cancelled").is_err());
    }

    #[test]
    fn a_manifest_orders_its_parts_however_they_arrived() {
        let parts = vec![
            Part::done("plugins", 0, 0, "aa", "p"),
            Part::done("database", 1, 2, "bb", "d"),
        ];
        let manifest = build_manifest("id", &parts, "2026-09-29T00:00:00Z");
        let names: Vec<&str> = manifest.parts.iter().map(|p| p.part.as_str()).collect();
        assert_eq!(names, vec!["database", "plugins"]);
    }

    #[test]
    fn two_runs_with_the_same_parts_in_different_orders_hash_the_same() {
        let a = build_manifest(
            "id",
            &[
                Part::done("database", 1, 2, "bb", "d"),
                Part::done("media", 3, 4, "cc", "m"),
            ],
            "2026-09-29T00:00:00Z",
        );
        let b = build_manifest(
            "id",
            &[
                Part::done("media", 3, 4, "cc", "m"),
                Part::done("database", 1, 2, "bb", "d"),
            ],
            "2026-09-29T00:00:00Z",
        );
        assert_eq!(manifest_checksum(&a), manifest_checksum(&b));
    }

    #[test]
    fn a_changed_checksum_changes_the_manifest_checksum() {
        let a = build_manifest("id", &[Part::done("media", 3, 4, "cc", "m")], "t");
        let b = build_manifest("id", &[Part::done("media", 3, 4, "dd", "m")], "t");
        assert_ne!(manifest_checksum(&a), manifest_checksum(&b));
    }

    #[test]
    fn verification_passes_when_every_part_matches() {
        let manifest = build_manifest(
            "id",
            &[
                Part::done("database", 10, 100, "aa", "d"),
                Part::done("media", 2, 200, "bb", "m"),
            ],
            "t",
        );
        let observed = vec![
            ObservedPart {
                part: "database".to_owned(),
                checksum: "aa".to_owned(),
                size_bytes: 100,
            },
            ObservedPart {
                part: "media".to_owned(),
                checksum: "bb".to_owned(),
                size_bytes: 200,
            },
        ];
        let result = verify_manifest(&manifest, &observed);
        assert!(result.is_clean());
        assert_eq!(result.matched.len(), 2);
    }

    #[test]
    fn a_part_whose_checksum_moved_is_reported_by_name() {
        let manifest = build_manifest("id", &[Part::done("media", 2, 200, "bb", "m")], "t");
        let observed = vec![ObservedPart {
            part: "media".to_owned(),
            checksum: "ff".to_owned(),
            size_bytes: 200,
        }];
        let result = verify_manifest(&manifest, &observed);
        assert!(!result.is_clean());
        assert_eq!(result.mismatched, vec!["media".to_owned()]);
        assert!(result.summary().contains("media"), "{}", result.summary());
    }

    #[test]
    fn an_artifact_truncated_to_nothing_is_caught_by_the_size_and_not_only_the_hash() {
        // The SHA-256 of an empty body agrees with itself, so a hash-only comparison passes
        // the one corruption that matters: the artifact that lost its whole content.
        let empty_hash = bytes_checksum(b"");
        let manifest = build_manifest("id", &[Part::done("media", 2, 4096, &empty_hash, "m")], "t");
        let observed = vec![ObservedPart {
            part: "media".to_owned(),
            checksum: empty_hash.clone(),
            size_bytes: 0,
        }];
        let result = verify_manifest(&manifest, &observed);
        assert!(!result.is_clean());
        assert_eq!(result.mismatched, vec!["media".to_owned()]);
    }

    #[test]
    fn a_part_that_never_produced_an_artifact_is_not_a_missing_artifact() {
        // Otherwise every `partial` run verifies as broken, and the message points at the
        // one part that was never written in the first place.
        let manifest = build_manifest(
            "id",
            &[
                Part::done("database", 10, 100, "aa", "d"),
                Part::failed("plugins", "no package installer"),
            ],
            "t",
        );
        let observed = vec![ObservedPart {
            part: "database".to_owned(),
            checksum: "aa".to_owned(),
            size_bytes: 100,
        }];
        let result = verify_manifest(&manifest, &observed);
        assert!(result.is_clean());
        assert_eq!(result.matched, vec!["database".to_owned()]);
    }

    #[test]
    fn a_part_missing_from_the_destination_is_unreadable_not_mismatched() {
        let manifest = build_manifest(
            "id",
            &[
                Part::done("database", 10, 100, "aa", "d"),
                Part::done("media", 2, 200, "bb", "m"),
            ],
            "t",
        );
        let observed = vec![ObservedPart {
            part: "database".to_owned(),
            checksum: "aa".to_owned(),
            size_bytes: 100,
        }];
        let result = verify_manifest(&manifest, &observed);
        assert_eq!(result.unreadable, vec!["media".to_owned()]);
        assert!(result.summary().contains("could not be read back"));
    }

    #[test]
    fn an_artifact_the_run_never_asked_for_is_named() {
        let manifest = build_manifest("id", &[Part::done("database", 1, 1, "aa", "d")], "t");
        let observed = vec![
            ObservedPart {
                part: "database".to_owned(),
                checksum: "aa".to_owned(),
                size_bytes: 1,
            },
            ObservedPart {
                part: "media".to_owned(),
                checksum: "zz".to_owned(),
                size_bytes: 9,
            },
        ];
        let result = verify_manifest(&manifest, &observed);
        assert_eq!(result.unexpected, vec!["media".to_owned()]);
        assert!(result.summary().contains("not part of this run"));
    }

    #[test]
    fn an_empty_verification_is_clean_because_nothing_was_claimed() {
        let manifest = Manifest {
            backup_id: "id".to_owned(),
            version: MANIFEST_VERSION,
            parts: Vec::new(),
            created_at: "t".to_owned(),
        };
        assert!(verify_manifest(&manifest, &[]).is_clean());
    }

    #[test]
    fn bytes_checksum_is_the_digest_of_the_bytes() {
        assert_eq!(
            bytes_checksum(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_manifest_carries_its_version() {
        let manifest = build_manifest("id", &[part("media")], "t");
        assert_eq!(manifest.version, MANIFEST_VERSION);
    }
}
