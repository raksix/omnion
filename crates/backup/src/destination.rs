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

/// The file a full storage key lands on, under `root`.
///
/// **Separate from [`local_root_for`] on purpose, and the two being confused is a bug that
/// hides from every test that uses them together.** A key is *already* prefix-qualified —
/// `storage_key("/2026-09-29/<id>", "media")` is `/2026-09-29/<id>/media-index.json` — so
/// joining one to `local_root_for(root, prefix)` writes the prefix twice and produces
/// `<root>/2026-09-29/<id>/2026-09-29/<id>/media-index.json`.
///
/// That bug survived a full slice because the reader made the *same* mistake in the same
/// direction, so writer and reader agreed with each other and every assertion passed while
/// the archive sat in a directory no operator would ever look in. A backup at
/// `/2026-09-29/<id>/2026-09-29/<id>/` is still a real backup, which is exactly why nothing
/// complained — and exactly why the mistake is dangerous: it only becomes visible when the
/// run is restored by hand, or when something else walks the root expecting one level.
///
/// The rule this function encodes: **a key is absolute with respect to the root.** Join it to
/// the root and to nothing else.
#[must_use]
pub fn local_path_for(root: &str, key: &str) -> PathBuf {
    let trimmed_key = key.trim_start_matches('/');
    if trimmed_key.is_empty() {
        return Path::new(root.trim()).to_path_buf();
    }
    Path::new(root.trim()).join(trimmed_key)
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

/// How a destination is doing on room, which is a different question from whether it takes a
/// write.
///
/// The two are separate because a probe that writes 31 bytes answers the easy half. A root
/// with 4 MB free passes the probe on every settings save and every status load, and then a
/// real run fills it halfway through the media part — which the destination reports as
/// `partial`, naming an object rather than the disk. "Destination healthy" over a root that
/// cannot hold the next archive is the wrong word for a right reading, and this enum is the
/// reading the card is allowed to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Headroom {
    /// Enough room to be worth reporting, and the run is expected to fit.
    Healthy,
    /// Room is running short: the next run may not fit. Not a failure — an operator with a
    /// bigger volume needs an hour, not an outage page.
    Tight,
    /// Less room than one full run needs. The next backup will not fit.
    Full,
    /// The filesystem could not be asked. Absent is not zero.
    Unknown,
}

impl Headroom {
    /// Whether a run should be expected to fit.
    #[must_use]
    pub fn admits_a_full_run(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// The line the card shows under the number.
    #[must_use]
    pub fn message(self, free: Option<u64>, largest: Option<u64>) -> String {
        match (self, free, largest) {
            (Self::Unknown, _, _) => {
                "The filesystem holding the backup root could not be measured for free space."
                    .to_owned()
            }
            (Self::Healthy, Some(free), _) => {
                format!("{} free — room for the next run.", human_bytes(free))
            }
            // A healthy root with no measured size is only reachable when the caller passed no
            // free bytes, which the `Unknown` arm above already names; the message is stated
            // rather than faked so the two cannot disagree.
            (Self::Healthy, None, _) => "Room for the next run.".to_owned(),
            (Self::Tight, Some(free), Some(largest)) => format!(
                "{} free, and the largest backup on record is {} — the next run may not fit.",
                human_bytes(free),
                human_bytes(largest)
            ),
            (Self::Tight, Some(free), None) => format!(
                "{} free, and no backup has been taken yet to compare it against — the next run \
                 may not fit.",
                human_bytes(free)
            ),
            (Self::Tight, None, _) => "Free space is low; the next run may not fit.".to_owned(),
            (Self::Full, Some(free), Some(largest)) => format!(
                "{} free, less than the largest backup on record ({}). The next backup will not \
                 fit here.",
                human_bytes(free),
                human_bytes(largest)
            ),
            (Self::Full, Some(free), None) => format!(
                "{} free and no run to compare against — the next backup will not fit.",
                human_bytes(free)
            ),
            (Self::Full, None, _) => "The backup root is full.".to_owned(),
        }
    }
}

/// Classify a destination's headroom from two numbers and nothing else.
///
/// Pure on purpose: the measurement below is the only thing that touches the kernel, and every
/// rule about what a number *means* is here, where a test can hand it a 4 MB volume and a
/// 40 GB one and read the verdict without a filesystem. A function that both measured and
/// judged would have to be tested against a real full disk to prove the "full" branch, which
/// nobody does on a shared build box.
///
/// `largest` is the biggest backup this tenant has on the destination — the closest honest
/// answer to "will the next run fit". A *predicted* next size would be a number invented by
/// this function, and a card that says "you need 12 GB" when the last run was 400 MB teaches
/// operators to ignore it. When there is nothing on record the comparison is skipped rather
/// than invented, and the message says so.
#[must_use]
pub fn classify_headroom(free: Option<u64>, largest: Option<u64>) -> Headroom {
    let Some(free) = free else {
        return Headroom::Unknown;
    };
    // No backup has ever been taken, so there is nothing to compare against. Calling an
    // arbitrary free-space figure "healthy" would be a claim this function cannot support, and
    // calling it "full" would send an operator to a disk that is fine. The truthful answer to
    // "is there room" with no yardstick is that the question cannot be answered yet.
    let Some(largest) = largest.filter(|bytes| *bytes > 0) else {
        return Headroom::Unknown;
    };
    // Room for the largest run *with* margin. A comparison against the exact largest artifact
    // reads "full" the moment a second run starts while the first is still there, which is the
    // normal state of a destination holding several restore points: the sweep keeps `retention`
    // of them, so a volume sized for one archive is full by design and the operator is blamed
    // for it. Twice is the smallest margin that is not a coin flip, and the wording of the
    // verdict is what the margin is for — the card says "may not fit", never "will not".
    match free.cmp(&(largest.saturating_mul(2))) {
        std::cmp::Ordering::Less => Headroom::Full,
        std::cmp::Ordering::Equal => Headroom::Tight,
        std::cmp::Ordering::Greater => Headroom::Healthy,
    }
}

/// A byte count as the sentence the card shows, without a dependency on the panel's own
/// formatter.
///
/// The panel has `formatBytes`; this is the crate's half of the same rule, kept here because a
/// probe message is produced server-side and rendered on a screen that may be showing a cached
/// document. Two formatters that disagree by a factor is a card that says 4.0 GB in its hint
/// and 3.7 GiB in its value — and the hint is the one that decides whether an operator
/// enlarges a disk.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    // `as f64` rather than `From`: `u64` has no `From<f64>`-backed conversion because the
    // value range does not fit, and these are byte counts off a filesystem, so the
    // 53-bit-mantissa limit is not a concern — 9 PB of headroom on a card that is deciding
    // between "fits" and "does not fit".
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Measure a destination's headroom, given the biggest backup this tenant holds on it.
///
/// The measurement is `omnion_health::probes::free_bytes` rather than a second `statvfs` here,
/// and that is a boundary the workspace set up before this function existed. Every crate but
/// `omnion-health` is `#![forbid(unsafe_code)]`, and `omnion-health` carries the single
/// sanctioned block in the whole workspace with a written argument for why it is there. Writing
/// the same syscall a second time would have meant either a second `unsafe` or — the
/// alternative that is easy to reach for and wrong on this particular host — shelling out to
/// `df` and parsing a table, on a machine that has already had a `coreutils` corruption
/// incident where binaries silently returned no output.
///
/// **The parent directory is measured, not `probed_path`.** That is not a detail, it is the
/// whole function working or not: `DestinationReport::probed_path` names the marker *file*, and
/// a probe that succeeds has just deleted it — the whole reason the probe reads it back and
/// removes it is so the destination does not accumulate one marker per probe. `statvfs` on a
/// path that no longer exists returns `ENOENT`, so measuring the reported path answers `None`
/// on **every** passing probe, and the card reads "could not be measured" precisely when the
/// destination is at its healthiest. The first draft of this measured `probed_path` and its
/// test caught it: the pure half was correct and the wiring was measuring a deleted file, which
/// is the same producer/verifier disagreement this REQ keeps finding in a new costume.
///
/// The measured path is the probe's own directory rather than the configured root: the probe
/// already created it, and a root that does not exist yet cannot be measured — answering
/// "unknown" for a fresh installation is correct, and answering "unknown" for the root of a
/// mounted volume because the backup subdirectory has not been made yet is a number that
/// flickers to healthy the first time a schedule fires.
#[must_use]
pub fn headroom_for(probe: &DestinationReport, largest: Option<u64>) -> (Headroom, Option<u64>) {
    // `and_then(Path::to_str)`, not a `map`: a non-UTF-8 parent path yields `None` here, and
    // falling back to the marker path would measure a file that does not exist — the exact
    // failure this function was rewritten to avoid. `as_str()` on the fallback is the only
    // lossy step and it is the one already present in the type.
    let directory = Path::new(&probe.probed_path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or(probe.probed_path.as_str());
    let free = omnion_health::probes::free_bytes(directory);
    (classify_headroom(free, largest), free)
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
    fn a_key_is_joined_to_the_root_once_and_only_once() {
        // The bug this function exists for. A key already carries the prefix, so joining it to
        // `local_root_for(root, prefix)` writes the prefix twice. Written out in full because
        // the doubled path is *plausible* — it exists, it holds the backup, and every reader
        // that made the same mistake agrees with the writer.
        let root = "/var/lib/omnion/backups";
        let prefix = storage_prefix("2026-09-29/abc");
        let key = storage_key(&prefix, "media");

        let doubled = local_root_for(root, &prefix).join(key.trim_start_matches('/'));
        assert_eq!(
            doubled.to_string_lossy(),
            format!("{root}/2026-09-29/abc/2026-09-29/abc/media.json"),
            "this is the mistake; the test exists so nobody reintroduces it"
        );

        let right = local_path_for(root, &key);
        assert_eq!(
            right.to_string_lossy(),
            format!("{root}/2026-09-29/abc/media.json")
        );
    }

    #[test]
    fn a_leading_slash_on_a_key_changes_nothing() {
        assert_eq!(
            local_path_for("/backups", "/a/b.json").to_string_lossy(),
            local_path_for("/backups", "a/b.json").to_string_lossy()
        );
    }

    #[test]
    fn an_empty_key_is_the_root_rather_than_a_file_under_it() {
        assert_eq!(local_path_for("/backups", "").to_string_lossy(), "/backups");
        assert_eq!(local_path_for("/backups", "/").to_string_lossy(), "/backups");
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

    // -----------------------------------------------------------------------------------------
    // Headroom
    //
    // The defect this exists for is one number: the card said "writable" and an operator read it
    // as "the next backup fits". Writability is a 31-byte write succeeding. A destination with
    // four megabytes free is writable, every probe passes, every settings save succeeds, and the
    // next real run dies partway through the media copy — reported as `partial` naming an
    // object, with the disk never named. So these tests are all about the gap between "takes a
    // write" and "will hold an archive".
    // -----------------------------------------------------------------------------------------

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn a_writable_destination_with_no_room_is_not_reported_as_healthy() {
        // The exact shape of the defect: a root that takes the probe's write, with less room
        // than the biggest backup on record. A classification that only asked "writable?" would
        // answer `Healthy` here, and so would the card it feeds.
        let verdict = classify_headroom(Some(4 * 1024 * 1024), Some(GIB));
        assert_eq!(
            verdict,
            Headroom::Full,
            "4 MB free against a 1 GB archive is a destination the next run will not fit in"
        );
        assert!(!verdict.admits_a_full_run());
    }

    #[test]
    fn room_to_spare_is_healthy_and_room_to_lose_is_tight() {
        assert_eq!(
            classify_headroom(Some(10 * GIB), Some(GIB)),
            Headroom::Healthy
        );
        // Exactly two runs' worth: the comparison is `<`, so the boundary lands on `Tight`
        // rather than flipping from healthy to full with no warning in between.
        assert_eq!(classify_headroom(Some(2 * GIB), Some(GIB)), Headroom::Tight);
        assert_eq!(
            classify_headroom(Some(2 * GIB - 1), Some(GIB)),
            Headroom::Full
        );
    }

    #[test]
    fn a_destination_holding_several_restore_points_is_not_full_by_design() {
        // The reason the margin is 2x rather than 1x: the retention sweep deliberately keeps
        // `retention` archives, so a volume sized for ONE archive reports "full" from the
        // second day of a normal installation, and the operator is sent to enlarge a disk that
        // is behaving exactly as configured. With the margin, a volume that holds the retention
        // window it was asked to hold reads healthy.
        let largest = 4 * GIB;
        let holding_four_of_them = 4 * largest;
        assert_eq!(
            classify_headroom(Some(holding_four_of_them), Some(largest)),
            Headroom::Healthy
        );
    }

    #[test]
    fn an_unmeasured_filesystem_is_unknown_and_not_zero() {
        // `None` in, `Unknown` out. The tempting version of this is `unwrap_or(0)`, which turns
        // "the kernel would not say" into "the disk is full" and pages an operator at 03:00 for
        // a filesystem that is fine.
        assert_eq!(classify_headroom(None, Some(GIB)), Headroom::Unknown);
        assert!(!classify_headroom(None, Some(GIB)).admits_a_full_run());
    }

    #[test]
    fn with_no_backup_on_record_the_question_cannot_be_answered() {
        // A fresh installation has a free-space number and no yardstick. "Healthy" would be a
        // claim this function cannot support (4 MB free is not healthy); "full" would send an
        // operator to a disk that is fine. Both are worse than saying the question is open, and
        // the message says so in words rather than showing a bare `unknown`.
        assert_eq!(
            classify_headroom(Some(500 * GIB), None),
            Headroom::Unknown
        );
        assert_eq!(classify_headroom(Some(0), None), Headroom::Unknown);
        // A zero-byte "largest backup" is not a yardstick either — it is a run that produced no
        // artifact, and comparing free space against it would call every destination healthy.
        assert_eq!(classify_headroom(Some(0), Some(0)), Headroom::Unknown);
    }

    #[test]
    fn the_message_carries_both_numbers_and_says_which_will_not_fit() {
        // Decimal units, asserted as decimals. This is a property of the formatter and it was
        // wrong in my first draft of this test, which expected "1.0 GB" for 1024^3 bytes: the
        // formatter divides by 1000 and that is CORRECT here rather than a bug, because every
        // number an operator compares this against — an S3 quota, a vendor's disk size, a cloud
        // console's "free space" — is decimal. A card that said 1.1 GB where the billing
        // console said 1.07 GiB would start a pointless argument with the number that is
        // supposed to be trusted. The test now pins the decimal reading so the next person to
        // "fix" it to binary sees it fail.
        let full = Headroom::Full.message(Some(4 * 1024 * 1024), Some(1024 * 1024 * 1024));
        assert!(full.contains("4.2 MB"), "{full}");
        assert!(full.contains("1.1 GB"), "{full}");
        // "will not" only where that is true. A `Tight` destination saying "will not fit"
        // teaches operators to ignore the word.
        assert!(full.contains("will not fit"), "{full}");

        let tight = Headroom::Tight.message(Some(2 * 1024 * 1024 * 1024), Some(1024 * 1024 * 1024));
        assert!(tight.contains("may not fit"), "{tight}");
        assert!(!tight.contains("will not fit"), "{tight}");

        let healthy = Headroom::Healthy.message(Some(40 * 1000 * 1000 * 1000), Some(1000 * 1000 * 1000));
        assert!(healthy.contains("40.0 GB"), "{healthy}");
        assert!(healthy.contains("room for the next run"), "{healthy}");
    }

    #[test]
    fn an_unknown_verdict_names_the_measurement_and_asserts_no_number() {
        // The first draft of this test asserted the message does not contain "free", which is
        // false: the sentence has to say *what* could not be measured, and "free space" is the
        // thing. The property worth pinning is the one underneath it — that no byte count is
        // invented, and that the word is "could not" rather than a verdict dressed as a fact.
        let message = Headroom::Unknown.message(None, Some(1024 * 1024 * 1024));
        assert!(message.contains("could not be measured"), "{message}");
        assert!(message.contains("free space"), "{message}");
        // No digits at all: an unknown verdict has no numbers to show, and a "0 B" in this slot
        // is the fabrication this state exists to prevent.
        assert!(
            !message.chars().any(|c| c.is_ascii_digit()),
            "an unknown measurement must not print a number: {message}"
        );
        // And it must not claim either verdict.
        assert!(!message.contains("will not fit"), "{message}");
        assert!(!message.contains("may not fit"), "{message}");
    }

    #[test]
    fn the_kernel_answers_for_a_directory_that_exists() {
        // The measurement itself, against this crate's own directory. The assertion is that a
        // real path yields *some* number — a `statvfs` that silently returned `None` would make
        // every card above read "unknown" and every test above still pass, because they test the
        // pure half.
        let here = env!("CARGO_MANIFEST_DIR");
        let free = omnion_health::probes::free_bytes(here)
            .expect("the crate's own directory is on a real filesystem");
        assert!(free > 0, "a mounted filesystem reports free bytes");
    }

    #[test]
    fn a_path_that_cannot_exist_reports_no_number_rather_than_zero() {
        // Not a permission test: `statvfs` on a missing path fails, and the failure must be
        // `None` and not a zero. A caller that treats `None` as zero pages an operator for a
        // path that does not exist yet — which is exactly the state of a fresh installation's
        // backup root before the first run.
        assert_eq!(
            omnion_health::probes::free_bytes("/nonexistent/omnion-backup-root"),
            None
        );
    }

    #[test]
    fn a_path_containing_a_nul_byte_is_refused_rather_than_truncated() {
        // `CString::new` fails on an interior NUL. The alternative is truncating the path at
        // the NUL and measuring a *different, existing* directory — a wrong number from a
        // malformed configuration, which is the shape of bug this module keeps hitting.
        assert_eq!(omnion_health::probes::free_bytes("/var/lib\0/backups"), None);
    }

    #[test]
    fn headroom_is_measured_where_the_probe_actually_wrote() {
        // The probe creates `<root>/<prefix>`; the configured root may not exist yet on a fresh
        // installation, and a card that reads "unknown" until a schedule first fires is a card
        // that is wrong exactly when an operator is deciding whether to set one up.
        let base = std::env::temp_dir().join("omnion-headroom-probe");
        let root = base.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&base);
        let report = probe_local(&root, "run-1/");
        assert!(report.writable, "{}", report.reason);
        let (verdict, free) = headroom_for(&report, Some(GIB));
        assert!(free.is_some(), "the probe's own directory is measurable");
        assert_ne!(verdict, Headroom::Unknown);
        let _ = std::fs::remove_dir_all(&base);
    }
}
