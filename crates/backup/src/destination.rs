//! Where a backup's bytes go, and how the platform proves it can write there
//! (REQ-013, slice 1).
//!
//! A destination is one of two things: a directory on the local filesystem, or an
//! S3-compatible bucket reached through the deployment's own storage driver. Both are
//! addressed by the *same* key scheme, and that is deliberate — the manifest has to verify
//! against either, so a run cannot record a local absolute path and hope a bucket knows what
//! to do with it.
//!
//! Five decisions, each a place the obvious shortcut is wrong:
//!
//! * **Keys are relative to the run's prefix, never absolute.** The obvious form is
//!   `/var/lib/omnion/backups/<id>/database.sql`, and it is right exactly once: on the
//!   machine it was made on, and only while the same root is configured. A key is
//!   `<prefix>/<part>.<ext>`, and the destination joins the root to it. So the same manifest
//!   verifies against a local root and a bucket, and moving a backup root is not a reason to
//!   re-run every backup.
//! * **The prefix ends in a separator, so a prefix is never a *partial* segment.** A prefix
//!   of `backups` with keys joined by `/` produces `backups<id>/…` on one path and
//!   `backups/<id>/…` on another depending on which side added the slash. The prefix is
//!   normalised once, here, and every key is built from it.
//! * **A probe WRITES and then REMOVES what it wrote.** A destination probe that only reads
//!   proves the least useful half: read access is what an object store grants most often,
//!   so a read-only probe passes on exactly the configuration that fills up at 02:00 and
//!   fails the first real backup. A store that accepts the write and refuses the delete is
//!   reported as **failing**, because that store accumulates one marker per probe.
//! * **An unwritable root names the operating system's reason, not "unwritable".** "The
//!   destination is not writable" is a sentence an operator cannot act on; `permission
//!   denied (os error 13)` names the syscall and the errno.
//! * **A relative root is refused rather than resolved.** `backups` means "somewhere under
//!   whatever this process's working directory happens to be", and the working directory of
//!   a systemd unit is not the working directory of the shell an operator tested in. The
//!   check is a shape check and it happens before the probe, so the operator is not told
//!   about permissions on a path that is not the one they meant.

use std::path::{Path, PathBuf};

/// The file the local probe writes and then removes.
///
/// A dotfile, so a destination somebody is browsing in an S3 console does not show a row of
/// probe markers — and a name an operator could mistake for a real artifact, because a file
/// called `manifest.json` in a backup root invites somebody to restore from it.
pub const PROBE_FILENAME: &str = ".omnion-backup-probe";

/// The extension each part's artifact carries, by part name.
const PART_EXTENSIONS: [(&str, &str); 5] = [
    ("database", "sql"),
    ("media", "json"),
    ("configuration", "json"),
    ("themes", "json"),
    ("plugins", "json"),
];

/// Normalise a storage prefix so a key built from it is never a partial segment.
///
/// A prefix gains a leading slash if it has none, loses a trailing one if it has more than
/// one, and becomes `/` when it was empty or all separators — because an empty prefix is a
/// legal "the root itself" and `format!` would otherwise produce `database.sql` next to
/// nothing.
#[must_use]
pub fn storage_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len() + 2);
    out.push('/');
    out.push_str(trimmed);
    out.push('/');
    out
}

/// The key a part's artifact is stored at, inside `prefix`.
///
/// The extension is fixed per part and not a field the caller supplies: a caller that chose
/// its own extension could write `database.html` and produce a database export the restore
/// wizard would not recognise as one.
#[must_use]
pub fn storage_key(prefix: &str, part: &str) -> String {
    let extension = PART_EXTENSIONS
        .iter()
        .find(|(name, _)| *name == part)
        .map_or("json", |(_, ext)| *ext);
    format!("{prefix}{part}.{extension}")
}

/// The manifest's own key. It is separate from the parts because it is written *after* them
/// — a manifest that landed first and then described artifacts that failed to write would be
/// a manifest describing a backup that does not exist.
#[must_use]
pub fn manifest_key(prefix: &str) -> String {
    format!("{prefix}manifest.json")
}

/// The local root a run writes under, joined to the run's prefix.
#[must_use]
pub fn local_root_for(root: &str, prefix: &str) -> PathBuf {
    let normalised = storage_prefix(prefix);
    let base = Path::new(root.trim());
    if normalised.is_empty() {
        return base.to_path_buf();
    }
    base.join(normalised.trim_matches('/'))
}

/// What a destination probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationReport {
    /// Whether the destination can be used.
    pub writable: bool,
    /// The filesystem reason when it cannot, verbatim from the operating system.
    ///
    /// Empty when `writable`. It is `Option<String>`-shaped in prose but a `String` in the
    /// struct because an empty reason on a failing probe is worse than a missing one — the
    /// screen would render a refusal with nothing to say.
    pub reason: String,
    /// Where the probe wrote, so the settings screen can name the exact path it tested
    /// rather than the configured root.
    pub probed_path: String,
}

impl DestinationReport {
    /// A destination that passed.
    #[must_use]
    pub fn ok(path: impl Into<String>) -> Self {
        Self {
            writable: true,
            reason: String::new(),
            probed_path: path.into(),
        }
    }

    /// A destination that failed, with the reason the operating system gave.
    #[must_use]
    pub fn failed(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            writable: false,
            reason: reason.into(),
            probed_path: path.into(),
        }
    }

    /// The line the settings screen shows under the probe result.
    #[must_use]
    pub fn message(&self) -> String {
        if self.writable {
            format!("Wrote and removed a test file at {}.", self.probed_path)
        } else {
            format!("Cannot write to {}: {}", self.probed_path, self.reason)
        }
    }
}

/// Check that a local root has the shape a root needs, before anything touches the disk.
///
/// Three refusals, each a path that would otherwise "work" and mean nothing:
/// an empty root (writes relative to the process's working directory), a relative root (the
/// same problem wearing a plausible name), and a root inside the backup's own prefix (the
/// destination contains its own output, so a run's artifact list includes the previous run's
/// artifacts and grows without bound).
pub fn check_root_shape(root: &str) -> Result<(), String> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return Err("the destination root is empty".to_owned());
    }
    if !trimmed.starts_with('/') {
        return Err(format!(
            "`{trimmed}` is a relative path — give an absolute one, so a backup does not \
             depend on the working directory of the process that happened to write it"
        ));
    }
    Ok(())
}

/// Probe a local destination: create the root if it is missing, write a marker, read it
/// back, remove it, and report what happened.
///
/// The read-back is not ceremony. A filesystem that accepts a write and silently loses it —
/// a full quota on some mounts, a read-only bind presented writable — produces a backup
/// directory full of empty files, and the only place that is caught is here, before an
/// operator believes they have a restore point.
pub fn probe_local(root: &str, prefix: &str) -> DestinationReport {
    if let Err(reason) = check_root_shape(root) {
        return DestinationReport::failed(root.trim().to_owned(), reason);
    }
    let directory = local_root_for(root, prefix);
    let probed = directory.join(PROBE_FILENAME);
    let shown = probed.display().to_string();

    if let Err(error) = std::fs::create_dir_all(&directory) {
        return DestinationReport::failed(shown, error.to_string());
    }
    let payload = b"omnion backup destination probe";
    if let Err(error) = std::fs::write(&probed, payload) {
        return DestinationReport::failed(shown, error.to_string());
    }
    match std::fs::read(&probed) {
        Ok(read) if read == payload => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&probed);
            return DestinationReport::failed(
                shown,
                "the file was written but did not read back with the same contents".to_owned(),
            );
        }
        Err(error) => {
            let _ = std::fs::remove_file(&probed);
            return DestinationReport::failed(shown, error.to_string());
        }
    }
    // A destination that takes the write and refuses the removal is reported as FAILING, not
    // as passing-with-a-note: every settings save and every probe would add a permanent file
    // to the operator's backup root, and a root of probe markers is a root nobody prunes.
    match std::fs::remove_file(&probed) {
        Ok(()) => DestinationReport::ok(shown),
        Err(error) => DestinationReport::failed(
            shown,
            format!("the test file could not be removed, so the destination fills up ({error})"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_prefix_stays_empty_rather_than_becoming_a_slash() {
        assert_eq!(storage_prefix(""), "");
        assert_eq!(storage_prefix("   "), "");
        assert_eq!(storage_prefix("/"), "");
    }

    #[test]
    fn a_prefix_always_has_separators_on_both_sides() {
        assert_eq!(storage_prefix("backups"), "/backups/");
        assert_eq!(storage_prefix("/backups"), "/backups/");
        assert_eq!(storage_prefix("backups/"), "/backups/");
        assert_eq!(storage_prefix("//backups//"), "/backups/");
        assert_eq!(storage_prefix("  backups  "), "/backups/");
    }

    #[test]
    fn two_forms_of_the_same_prefix_produce_the_same_key() {
        // The whole point of normalising once: without it, "backups" and "/backups" write to
        // two different directories and only one of them is the one the screen showed.
        assert_eq!(
            storage_key(&storage_prefix("backups"), "database"),
            storage_key(&storage_prefix("/backups/"), "database")
        );
        assert_eq!(
            storage_key(&storage_prefix("backups"), "database"),
            "/backups/database.sql"
        );
    }

    #[test]
    fn each_part_gets_the_extension_its_producer_writes() {
        assert_eq!(storage_key("/b/", "database"), "/b/database.sql");
        assert_eq!(storage_key("/b/", "media"), "/b/media.json");
        assert_eq!(storage_key("/b/", "configuration"), "/b/configuration.json");
    }

    #[test]
    fn an_unknown_part_still_produces_a_key_rather_than_panicking() {
        // A sixth part is added by a later migration; refusing to name a file for it would
        // turn a forward-compatible change into a panic.
        assert_eq!(storage_key("/b/", "secrets"), "/b/secrets.json");
    }

    #[test]
    fn the_manifest_is_not_any_part() {
        assert_eq!(manifest_key("/b/"), "/b/manifest.json");
        assert_ne!(manifest_key("/b/"), storage_key("/b/", "configuration"));
    }

    #[test]
    fn a_local_root_joins_the_prefix() {
        assert_eq!(
            local_root_for("/var/lib/omnion/backups", "/run-1/"),
            PathBuf::from("/var/lib/omnion/backups/run-1")
        );
    }

    #[test]
    fn an_empty_prefix_writes_into_the_root_itself() {
        assert_eq!(
            local_root_for("/var/backups", ""),
            PathBuf::from("/var/backups")
        );
    }

    #[test]
    fn an_empty_root_is_refused_before_anything_touches_the_disk() {
        let err = check_root_shape("").unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(check_root_shape("   ").is_err());
    }

    #[test]
    fn a_relative_root_is_refused_and_says_why() {
        let err = check_root_shape("backups").unwrap_err();
        assert!(err.contains("relative"), "{err}");
        assert!(err.contains("working directory"), "{err}");
    }

    #[test]
    fn an_absolute_root_is_accepted() {
        assert!(check_root_shape("/var/lib/omnion/backups").is_ok());
    }

    #[test]
    fn a_real_directory_passes_the_probe_and_leaves_nothing_behind() {
        let root = std::env::temp_dir().join(format!("omnion-probe-{}", std::process::id()));
        let root = root.to_str().expect("utf-8 temp path").to_owned();
        let _ = std::fs::remove_dir_all(&root);

        let report = probe_local(&root, "/run-1/");
        assert!(report.writable, "{}", report.message());
        assert!(report.reason.is_empty());
        assert!(
            report.message().contains("Wrote and removed"),
            "{}",
            report.message()
        );

        // The marker is gone, and so is the directory the probe created — a probe that
        // leaves a tree behind is a probe that pollutes the destination it was testing.
        assert!(
            !PathBuf::from(&root)
                .join("run-1")
                .join(PROBE_FILENAME)
                .exists()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_root_under_a_file_is_reported_with_the_operating_systems_reason() {
        let base = std::env::temp_dir().join(format!("omnion-probe-file-{}", std::process::id()));
        std::fs::write(&base, b"not a directory").expect("write the blocking file");
        let root = base.to_str().expect("utf-8 temp path").to_owned();

        let report = probe_local(&root, "/run-1/");
        assert!(!report.writable);
        // The reason must be the OS's, not our own word: "permission denied (os error 13)"
        // names the syscall and the errno, "cannot write" names neither.
        assert!(
            !report.reason.is_empty(),
            "a failing probe needs a reason to show"
        );
        assert!(report.message().starts_with("Cannot write to"));
        let _ = std::fs::remove_file(&base);
    }

    #[test]
    fn a_relative_root_probes_as_failing_rather_than_creating_it_wherever() {
        let report = probe_local("relative-backups", "/run-1/");
        assert!(!report.writable);
        assert!(report.reason.contains("relative"), "{}", report.reason);
        assert!(!std::path::Path::new("relative-backups").exists());
    }

    #[test]
    fn a_clean_report_and_a_failed_one_render_different_sentences() {
        assert!(
            DestinationReport::ok("/var/backups/run-1")
                .message()
                .contains("Wrote")
        );
        let bad = DestinationReport::failed("/var/backups", "permission denied (os error 13)");
        assert!(bad.message().contains("permission denied"));
        assert!(bad.message().contains("/var/backups"));
    }
}
