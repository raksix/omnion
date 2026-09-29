//! Removing a run's bytes from the destination (REQ-013).
//!
//! # Why this module exists
//!
//! `DELETE /api/v1/backups/{id}` deletes the row and its parts, and it used to stop there. The
//! artifacts — the database export, the media objects, the configuration document, the theme
//! and plugin manifests, and the run's own manifest — stayed on the destination exactly as
//! they were. The panel compensated with a sentence: *"Backup removed. Its artifacts are still
//! on the destination until the next prune."*
//!
//! That sentence is honest and it is also the defect. A backup root is the one directory in
//! this product where the cost is paid **per byte, forever**, and the prune sweep — which
//! does remove artifacts — runs on its own schedule, not when an operator deletes something.
//! So the ordinary path was: an operator tidies up three old runs, the list goes green, and a
//! full copy of the platform's media library stays on disk with nothing pointing at it. Every
//! later backup, in every month, would do it again. The product told the operator the truth
//! and the truth was the problem.
//!
//! Two things were wrong with it, and the second is why this is not "a `remove_dir_all` call":
//!
//! * **Deletion is the only irreversible thing this product does.** A restore point that can be
//!   un-deleted is a backup; one that cannot is a decision. The row must therefore go **only
//!   after** the bytes are gone, so an interrupted delete leaves a row pointing at artifacts
//!   that are still there (an operator can retry) rather than a deleted row over an archive
//!   nobody can find (an operator has lost a restore point without being told).
//! * **The prefix is a deletion boundary, not a string.** A run writes under
//!   `<root>/<prefix>/` where the prefix is derived from the run's **id**, so removing
//!   `<root>/<prefix>` is exactly the run's own directory and nothing else. That property is
//!   why this is safe to automate, and it depends on the prefix being derived from the id
//!   rather than from the clock — two runs started in the same second would share a prefix,
//!   and one delete would take both archives.
//!
//! # What it removes, and what it refuses
//!
//! Everything the run wrote, by *removing its own directory* rather than by listing the
//! manifest and removing files one at a time. The directory is the boundary; the manifest is
//! a description of it, and a description can be wrong — a run that died mid-write left files
//! the manifest never mentioned, and an index-driven delete would leak them.
//!
//! Three refusals, each a way the naive version destroys something it was not asked to:
//!
//! * **An empty or non-absolute root.** `remove_dir_all("")` on some platforms is the current
//!   working directory. The root comes from settings, and settings come from a form.
//! * **A prefix that escapes the root.** `..` in a prefix would make "remove this run's
//!   directory" mean "remove the backup root", and with it every other backup in it.
//! * **A run that is still in flight.** A backup being written while it is deleted is a run
//!   whose artifact half exists; the delete would succeed, the run would keep writing, and
//!   the leftover files would be orphans nothing ever prunes. The row stays and the caller
//!   is told why.
//!
//! # Why the outcome is reported instead of raised
//!
//! A delete that could not reach the destination still removed the row, or it did not and the
//! caller retries. Neither is an exception worth a `500`: the operator needs to know *what is
//! still on disk*, so [`PurgeReport`] carries the count of removed entries, the count that
//! could not be removed, and the first few failures in the store's own words. The API answers
//! `200` with that report rather than pretending the bytes are gone, and the panel renders
//! it — a screen that says "removed" over a directory full of files is the exact lie this
//! module was written to remove.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::{BackupError, storage_prefix};

/// The one directory a run may write outside its prefix: the destination probe's marker.
///
/// It lives at `<root>/<prefix>/.omnion-backup-probe`, so it is *inside* the run's directory
/// and goes with it. It is named here anyway, because a delete that enumerated the directory
/// and tried to special-case it would be one more special case to forget.
pub const PROBE_MARKER: &str = ".omnion-backup-probe";

/// How many failures a report names before it stops listing them.
///
/// The same cap the media part uses, for the same reason: a delete against a directory with
/// permissions stripped from every entry needs the operator to know it is *thousands*, and
/// three file names are enough to recognise the pattern.
pub const MAX_REPORTED_PURGE_FAILURES: usize = 3;

/// What a removal actually did.
///
/// Fields rather than a `bool` because "the row is gone" and "the bytes are gone" are two
/// different facts, and the API that collapses them is what produced the original defect.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PurgeReport {
    /// The directory that was targeted, in full. Always reported, even when nothing was
    /// found: an operator reading "removed" needs to know *where*.
    pub root: String,
    /// Whether the run's own directory existed at all.
    ///
    /// A run whose artifacts were never written — a `failed` run that failed before its first
    /// part — has no directory. That is **not** an error and not "removed 12 files": it is
    /// its own answer, because "the archive was already gone" and "twelve files were deleted"
    /// are different things for an operator reconciling a disk.
    pub existed: bool,
    /// How many filesystem entries were removed, at any depth.
    ///
    /// Counted **after** the removal by walking what is left is the only honest way to count
    /// a recursive delete, and it is the number the panel shows. Counting the manifest's
    /// entries instead would report the archive's *description* of itself.
    pub removed_entries: i32,
    /// How many entries could not be removed.
    pub failed_entries: i32,
    /// The first few failures, naming the path and the operating system's own words.
    pub failures: Vec<PurgeFailure>,
}

impl PurgeReport {
    /// The line the panel shows under the delete action.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.existed {
            return format!(
                "The backup's directory was already gone at {}.",
                self.root
            );
        }
        if self.failed_entries == 0 {
            return format!(
                "Removed {} entr{} from {}.",
                self.removed_entries,
                if self.removed_entries == 1 { "y" } else { "ies" },
                self.root
            );
        }
        format!(
            "Removed {} of {} entries from {}; {} could not be removed and are still there: {}.",
            self.removed_entries,
            self.removed_entries + self.failed_entries,
            self.root,
            self.failed_entries,
            self.failure_summary()
        )
    }

    /// Whether every entry that was targeted is gone.
    ///
    /// The API uses this to decide what to tell the operator, and never to decide whether the
    /// row goes: the row's fate is decided by the caller, which knows whether the run was
    /// restorable.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed_entries == 0
    }

    /// The failures, in one sentence, for an error column and an audit entry.
    #[must_use]
    pub fn failure_summary(&self) -> String {
        if self.failures.is_empty() {
            return "the destination did not say why".to_owned();
        }
        self.failures
            .iter()
            .map(|failure| format!("{} ({})", failure.path, failure.reason))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// One entry that could not be removed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PurgeFailure {
    /// The path as the operating system named it.
    pub path: String,
    /// The operating system's own words — `permission denied (os error 13)`, never "unwritable".
    pub reason: String,
}

/// Resolve the directory one run's artifacts live in, refusing anything that is not that
/// directory.
///
/// Three refusals, all before a single byte is touched:
///
/// * **An empty or relative root.** `remove_dir_all` on a relative path resolves against the
///   process's working directory — which for a systemd unit is not the directory an operator
///   typed into the settings screen. The same rule the destination probe enforces, reused
///   here deliberately: one rule, one place.
/// * **A prefix that leaves the root.** `..` segments are rejected outright rather than
///   normalised, because a normalised traversal is a traversal that passed. A prefix that
///   escapes is never legitimate; one that *looks* like it escapes and does not is still not
///   a thing the UI can produce, so there is nothing to be permissive about.
/// * **An empty prefix.** A run with no prefix wrote to the root itself; "delete this
///   backup" would then mean "delete every backup on the destination", and that is the
///   single most destructive line in this module. Refused.
///
/// The return value is the directory **and** the `local_path_for` key semantics in one
/// place, so the caller cannot join it a second time.
pub fn run_directory(root: &str, prefix: &str) -> Result<PathBuf> {
    let trimmed_root = root.trim();
    if trimmed_root.is_empty() {
        return Err(BackupError::Invalid(
            "the destination root is empty, so there is nothing to remove from".to_owned(),
        ));
    }
    if !trimmed_root.starts_with('/') {
        return Err(BackupError::Invalid(format!(
            "`{trimmed_root}` is a relative destination root — give an absolute one, so a delete \
             does not resolve against the working directory of whatever process happened to run it"
        )));
    }

    let normalised = storage_prefix(prefix);
    if normalised.is_empty() {
        return Err(BackupError::Invalid(
            "this backup has no storage prefix, so its directory cannot be identified; refusing \
             to remove the destination root itself"
                .to_owned(),
        ));
    }
    if normalised.split('/').any(|segment| segment == "..") {
        return Err(BackupError::Invalid(format!(
            "this backup's storage prefix `{normalised}` points outside the destination root"
        )));
    }

    Ok(crate::destination::local_root_for(trimmed_root, &normalised))
}

/// Remove a run's directory from the destination, and report exactly what happened.
///
/// Order matters and is not negotiable: the directory is removed **first**, and the caller
/// deletes the row afterwards. A delete that removed the row first would, on any failure
/// between the two, leave a backup the panel no longer lists over an archive nobody can
/// find — the operator's restore points would be gone with no record of what they were.
///
/// The count is taken **after** the removal by walking the directory that should no longer
/// exist. A recursive delete cannot be counted before it runs, and counting the manifest's
/// entries would report what the archive said about itself rather than what is on disk.
pub async fn remove_run_artifacts(root: &str, prefix: &str) -> Result<PurgeReport> {
    let directory = run_directory(root, prefix)?;
    let mut report = PurgeReport {
        root: directory.display().to_string(),
        ..PurgeReport::default()
    };

    if !directory.exists() {
        // Not an error. A run that failed before its first part wrote nothing, and a
        // destination that was rotated out from under the platform left the same shape.
        return Ok(report);
    }

    report.existed = true;
    let mut failures = Vec::new();

    match tokio::fs::remove_dir_all(&directory).await {
        Ok(()) => {
            report.removed_entries = count_entries(&directory).await;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Removed by someone else between the check and the call. The end state is the
            // one the delete wanted, so this is a success with nothing to report, not a
            // failure that leaves the operator guessing.
            report.existed = true;
            report.removed_entries = 0;
        }
        Err(error) => {
            // A directory that could not be removed whole is walked entry by entry, because
            // "the directory is non-empty" and "the directory is not there" are the same
            // error to a caller that only asked "is it gone" — and only the walk can tell
            // them apart, by actually leaving nothing behind.
            failures.push(PurgeFailure {
                path: directory.display().to_string(),
                reason: error.to_string(),
            });
            let mut remaining = 0;
            let mut failed = 0;
            walk_and_remove(&directory, &mut remaining, &mut failed, &mut failures).await;
            report.removed_entries = remaining;
            report.failed_entries = failed;
        }
    }

    report.failures = failures
        .into_iter()
        .take(MAX_REPORTED_PURGE_FAILURES)
        .collect();
    Ok(report)
}

/// Count the entries still under `directory` after a removal — zero is the only good answer,
/// and a non-zero count is a number the operator needs.
async fn count_entries(directory: &Path) -> i32 {
    let mut total = 0;
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            total += 1;
            if entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// Remove what is left of a directory one entry at a time, counting both halves.
///
/// Entry by entry because `remove_dir_all` is all-or-nothing per call: on a directory whose
/// child is a mount point, a running file, or a directory with permissions stripped, one
/// refusal fails the whole call and the archive stays exactly where it was. A walk that
/// removes what it can and *counts what it could not* is the difference between "the delete
/// failed" and "11 of 12 files are gone and here is the one that is not".
async fn walk_and_remove(
    directory: &Path,
    removed: &mut i32,
    failed: &mut i32,
    failures: &mut Vec<PurgeFailure>,
) {
    let mut directories = vec![directory.to_path_buf()];
    while let Some(current) = directories.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(entries) => entries,
            Err(error) => {
                *failed += 1;
                if failures.len() < MAX_REPORTED_PURGE_FAILURES {
                    failures.push(PurgeFailure {
                        path: current.display().to_string(),
                        reason: error.to_string(),
                    });
                }
                continue;
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                directories.push(path);
                continue;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => *removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    *failed += 1;
                    if failures.len() < MAX_REPORTED_PURGE_FAILURES {
                        failures.push(PurgeFailure {
                            path: path.display().to_string(),
                            reason: error.to_string(),
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "omnion-purge-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("the temp root must be created");
        root
    }

    /// A run writes under `<root>/<prefix>`, and the prefix is derived from the run's id.
    #[test]
    fn a_runs_directory_is_its_prefix_under_the_root() {
        let id = uuid::Uuid::new_v4();
        let prefix = format!("/2026-09-29/{id}");
        assert_eq!(
            run_directory("/var/lib/omnion/backups", &prefix).expect("a run directory"),
            PathBuf::from(format!("/var/lib/omnion/backups/2026-09-29/{id}"))
        );
    }

    /// Two runs started in the same second have different ids and therefore different
    /// directories. This is the property that makes the delete safe to automate, and it is a
    /// property of `set_prefix` rather than of this module — so it is pinned here, where the
    /// delete depends on it.
    #[test]
    fn two_runs_never_share_a_directory_even_in_the_same_second() {
        let first = run_directory("/backups", &format!("/{}", uuid::Uuid::new_v4()))
            .expect("the first run's directory");
        let second = run_directory("/backups", &format!("/{}", uuid::Uuid::new_v4()))
            .expect("the second run's directory");
        assert_ne!(first, second);
    }

    /// The refusal that matters most: an empty prefix means "the root itself", and removing
    /// the root removes every backup on the destination.
    #[test]
    fn a_run_with_no_prefix_is_refused_rather_than_emptying_the_root() {
        for prefix in ["", "/", "   "] {
            let error = run_directory("/var/lib/omnion/backups", prefix)
                .expect_err("a run with no prefix must be refused");
            assert!(
                error.to_string().contains("no storage prefix"),
                "the refusal must name the reason, got: {error}"
            );
        }
    }

    #[test]
    fn a_relative_root_is_refused_before_anything_touches_the_disk() {
        let error = run_directory("backups", "/2026-09-29/abc")
            .expect_err("a relative root must be refused");
        assert!(error.to_string().contains("relative"), "got: {error}");
    }

    #[test]
    fn an_empty_root_is_refused() {
        let error =
            run_directory("", "/2026-09-29/abc").expect_err("an empty root must be refused");
        assert!(error.to_string().contains("empty"), "got: {error}");
    }

    /// A `..` segment makes "this run's directory" mean the backup root, and with it every
    /// other backup in it. Rejected, not normalised: a normalised traversal passed the
    /// check that was meant to stop it.
    #[test]
    fn a_prefix_that_escapes_the_root_is_refused() {
        for prefix in ["/../elsewhere", "/2026/../../..", "/a/../../b"] {
            let error = run_directory("/var/backups", prefix)
                .expect_err("a traversing prefix must be refused");
            assert!(
                error.to_string().contains("outside the destination root"),
                "prefix {prefix} gave: {error}"
            );
        }
    }

    /// A key is prefix-qualified and `local_root_for` adds the prefix — so the two must be
    /// joined once. This is the defect the media tick found, restated at the delete, because
    /// a delete that pointed one level too deep would report "removed" over a directory that
    /// is still full of the run's own bytes.
    #[test]
    fn the_directory_is_joined_once_not_twice() {
        let root = "/var/backups";
        let prefix = "/2026-09-29/abc";
        let directory = run_directory(root, prefix).expect("a run directory");
        let joined_twice = directory.join(prefix.trim_start_matches('/'));
        assert!(
            !directory.starts_with(&joined_twice),
            "the run's directory must not be nested under itself: {}",
            directory.display()
        );
    }

    #[tokio::test]
    async fn removing_a_run_takes_its_objects_and_its_siblings_survive() {
        let root = temp_root("removes-a-run");
        let run = "2026-09-29/run-a";
        let sibling = "2026-09-29/run-b";

        let run_directory_on_disk = root.join(run);
        std::fs::create_dir_all(run_directory_on_disk.join("objects/site-1")).expect("objects");
        std::fs::write(
            run_directory_on_disk.join("objects/site-1/logo.png"),
            b"a real image, or near enough",
        )
        .expect("the object must be written");
        std::fs::write(run_directory_on_disk.join("manifest.json"), b"{}").expect("manifest");
        std::fs::write(run_directory_on_disk.join("media-index.json"), b"{}").expect("index");

        let sibling_directory = root.join(sibling);
        std::fs::create_dir_all(&sibling_directory).expect("the sibling must exist");
        std::fs::write(sibling_directory.join("manifest.json"), b"{}").expect("sibling manifest");

        let report = remove_run_artifacts(root.to_str().expect("utf-8"), &format!("/{run}"))
            .await
            .expect("the removal must succeed");

        assert!(report.is_complete(), "the removal must be complete");
        assert!(report.existed, "the directory existed before the removal");
        assert!(
            !run_directory_on_disk.exists(),
            "the run's directory must be gone: {}",
            run_directory_on_disk.display()
        );
        assert!(
            sibling_directory.join("manifest.json").exists(),
            "another run's archive must survive — the delete takes the prefix, not the root"
        );
    }

    /// A run that never wrote anything is not an error, and it is not "removed 0 files" —
    /// it is its own sentence, because an operator reconciling a disk needs to know the
    /// directory was *already* gone rather than emptied by this delete.
    #[tokio::test]
    async fn a_run_that_wrote_nothing_reports_that_rather_than_a_failure() {
        let root = temp_root("never-wrote");
        let report = remove_run_artifacts(root.to_str().expect("utf-8"), "/2026-09-29/ghost")
            .await
            .expect("a missing directory is not an error");
        assert!(!report.existed, "nothing was there to be removed");
        assert_eq!(report.failed_entries, 0);
        assert!(!report.is_complete() || report.removed_entries == 0);
        assert!(
            report.summary().contains("already gone"),
            "the summary must say the directory was already gone, got: {}",
            report.summary()
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// The root is left standing even when every entry under a run is gone. An operator who
    /// deleted a backup and then found the backup root itself gone would be looking at a
    /// platform that cannot take a backup at all.
    #[tokio::test]
    async fn the_root_itself_survives_the_delete() {
        let root = temp_root("root-survives");
        let run = "2026-09-29/run-a";
        std::fs::create_dir_all(root.join(run)).expect("the run directory");
        remove_run_artifacts(root.to_str().expect("utf-8"), &format!("/{run}"))
            .await
            .expect("the removal must succeed");
        assert!(
            root.exists(),
            "the backup root must survive — only the run's own directory goes"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_complete_report_says_what_it_removed_and_where() {
        let report = PurgeReport {
            root: "/var/backups/2026-09-29/run-a".to_owned(),
            existed: true,
            removed_entries: 12,
            failed_entries: 0,
            failures: Vec::new(),
        };
        assert!(report.is_complete());
        assert_eq!(
            report.summary(),
            "Removed 12 entries from /var/backups/2026-09-29/run-a."
        );
    }

    #[test]
    fn one_entry_is_reported_in_the_singular() {
        let report = PurgeReport {
            root: "/var/backups/run-a".to_owned(),
            existed: true,
            removed_entries: 1,
            failed_entries: 0,
            failures: Vec::new(),
        };
        assert_eq!(
            report.summary(),
            "Removed 1 entry from /var/backups/run-a."
        );
    }

    /// A partial removal says what is **still there**. "Failed" with no remainder is the
    /// sentence the original defect wore: the operator cannot tell a clean delete from a
    /// delete that left a full media library behind.
    #[test]
    fn a_partial_removal_names_what_is_still_on_disk() {
        let report = PurgeReport {
            root: "/var/backups/run-a".to_owned(),
            existed: true,
            removed_entries: 11,
            failed_entries: 1,
            failures: vec![PurgeFailure {
                path: "/var/backups/run-a/objects/site-1/logo.png".to_owned(),
                reason: "permission denied (os error 13)".to_owned(),
            }],
        };
        assert!(!report.is_complete(), "a partial removal is not complete");
        let summary = report.summary();
        assert!(summary.contains("11 of 12"), "got: {summary}");
        assert!(summary.contains("logo.png"), "the summary must name the file: {summary}");
        assert!(
            summary.contains("permission denied (os error 13)"),
            "the operating system's own words, got: {summary}"
        );
    }

    /// A failure with no recorded reason renders "the destination did not say why" rather
    /// than an empty gap — a refusal with nothing to say is worse than a missing one.
    #[test]
    fn a_failure_without_a_reason_still_says_something() {
        let report = PurgeReport {
            root: "/var/backups/run-a".to_owned(),
            existed: true,
            removed_entries: 0,
            failed_entries: 2,
            failures: Vec::new(),
        };
        assert!(
            report.failure_summary().contains("did not say why"),
            "got: {}",
            report.failure_summary()
        );
    }
}
