//! The `media` part: the library's **bytes**, not its row count (REQ-013).
//!
//! # The defect this module exists to remove
//!
//! The first implementation of the `media` part wrote a JSON document saying "site X has
//! 412 files, 88 MiB" and recorded that document as the part's artifact. Every part of that
//! run was `done`, the run was `succeeded`, `verify` read the artifact back and agreed with
//! its own checksum — and **not one byte of the library had been copied anywhere.** The run
//! reported a restore point that could not restore a single image.
//!
//! This is the same class of defect the crate already documents twice — a checksum over a
//! document nobody wrote is a perfectly good checksum, and a manifest is not a backup — but it
//! hides in a different place, and the hiding is the lesson:
//!
//! * **A count is not a copy.** `count(*)` and `sum(size_bytes)` are two correct statements
//!   about a table and neither of them is a statement about the bucket. The number that made
//!   the row look healthy was the number the database can produce without touching the object
//!   store at all, which is exactly why it was available on a run where the store was
//!   unreachable.
//! * **The part must be proved by reading it back from the place it was written**, never by
//!   the checksum of the manifest that describes it. A `Part` whose bytes were never written
//!   has a checksum over a manifest entry, and `verify_manifest` — correctly — compares the
//!   manifest to the artifact. Two halves of the same self-referential loop, both green.
//! * **A part that cannot reach the object store is a FAILED part, not a smaller one.** If the
//!   store refuses to answer, the honest outcome is `failed` with the store's own words. A run
//!   that quietly backed up three of four sites is worse than one that backs up none of them,
//!   because the operator reads `succeeded`.
//!
//! # What this module does, and what it deliberately does not
//!
//! * It copies **object by object** through the deployment's own [`Storage`] abstraction, so a
//!   site backed up from a bucket and a site backed up from a directory produce the same part
//!   with the same manifest shape. It holds **one object in memory at a time** — never the
//!   whole library — and writes each object into the part's own directory as it goes, so a run
//!   that dies halfway leaves the objects it *did* copy behind, which is what makes a resume
//!   possible and a diagnostic possible.
//! * It writes a **per-object index** beside the objects. The index is what the restore path
//!   reads: it maps each archived key back to the `storage_key` the live library expects, and
//!   it carries each object's own SHA-256, so the restore can re-verify a copy without trusting
//!   the source row.
//! * It does **not** invent a tar format. A tar writer is a few hundred lines with a real
//!   number of ways to be subtly wrong (block padding, PAX headers, long names, the 512-byte
//!   boundary), and a corrupt archive discovered during a restore is the single worst outcome
//!   this product can produce. One file per object is boring, verifiable with `bytes_checksum`
//!   per file, and restorable with a directory copy.
//! * It does **not** pretend the object store is a filesystem. `Storage::get` returns bytes or
//!   an error; there is no partial read and no streaming handle today, so an object is read in
//!   full or not at all, and an object too large to hold is named in the failure rather than
//!   truncated. The cap is a named constant, not a silent one.
//!
//! # The two numbers that must not be confused
//!
//! [`MediaCopyReport::objects_copied`] and [`MediaCopyReport::objects_failed`] are separate
//! on purpose, and so is `item_count` on the part. A part that copied every object reports
//! `item_count = objects_copied`; a part that failed reports `item_count = 0` **with the
//! failure recorded**, because `item_count = 3` beside `failed` is a sentence nobody can act on.
//! The restore wizard asks "how many files can I get back?" and the answer has to be the number
//! that actually landed, never the number that was available.

use std::collections::BTreeMap;

use omnion_storage::Storage;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;

/// One object the copy loop will attempt, read out of PostgreSQL.
///
/// The row is the *index* of the library, not its content: a file whose object is missing from
/// the store still has a row here, and the copy must notice that and say so.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MediaObject {
    /// The `media` row's id, carried into the index so a restore can write rows back.
    pub id: Uuid,
    /// Which site the file belongs to.
    pub site_id: Uuid,
    /// Key the object is stored under in the live library.
    pub storage_key: String,
    /// Original file name.
    pub filename: String,
    /// Content type recorded at upload.
    pub content_type: String,
    /// Size the row recorded, used to catch an object that changed under us.
    pub size_bytes: i64,
    /// SHA-256 the row recorded, compared against what came back.
    pub checksum: String,
}

/// Largest object the media part will copy, in bytes.
///
/// 256 MiB. The reason it exists is not a hardware limit: `Storage::get` has no streaming
/// handle today, so the object is held in memory in full. An object over this is **named in the
/// part's failure** and skipped rather than silently truncated, because a truncated image is a
/// backup that claims to have restored a file it destroyed.
pub const MAX_OBJECT_BYTES: i64 = 256 * 1024 * 1024;

/// Directory the archived objects are written into, relative to the run's prefix.
///
/// A directory rather than a flat prefix: a library with two files called `logo.png` in
/// different folders must not overwrite one with the other, and flattening by filename is the
/// obvious way to do exactly that.
pub const OBJECTS_DIR: &str = "objects";

/// The name of the per-object index, beside the objects rather than inside them.
pub const INDEX_FILENAME: &str = "media-index.json";

/// Where one archived object lives, as the key an operator sees and the restore path reads.
///
/// The layout mirrors the library rather than flattening it: `<site>/<id>-<name>`. The id is
/// first because it is unique and the name is a convenience, so two files with the same name in
/// the same site still land on different paths — a collision here would mean one library file
/// silently overwrote another inside the archive.
#[must_use]
pub fn object_key(site_id: Uuid, id: Uuid, filename: &str) -> String {
    format!("{OBJECTS_DIR}/{site_id}/{id}-{filename}")
}

/// A file name reduced to something a filesystem and a restore path can both accept.
///
/// Three rules, all forced by what the result is used for — it is a **path segment** inside the
/// archive, written from a name that came out of an upload:
///
/// * Anything outside printable ASCII becomes `_`, because a control character in a path is a
///   bug on every platform and a space in a key is a quoting problem in every shell an operator
///   will use to look at the backup.
/// * **A run of dots collapses to one.** Not because an embedded `..` can traverse — a segment
///   that still contained a `/` would be needed for that, and none can survive — but because
///   `..` inside a key is a string that every path-normalising tool downstream will have an
///   opinion about, and a key that has to be quoted in three languages is a key somebody will
///   eventually get wrong. `report..2026.png` becomes `report.2026.png`; the id keeps it
///   unique.
/// * Leading and trailing dots are dropped, so an archived object is never a dotfile and never
///   ends in the trailing dot that some filesystems refuse.
/// * The length is bounded, because a 255-byte name plus the id and the site segment is over
///   the 255-byte limit on ext4 and would make the copy fail on a perfectly legitimate upload.
///
/// **Truncation cannot merge two files, and that is the property worth stating.** The obvious
/// worry is that two 300-character names sharing a 120-character prefix would collapse into one
/// archive entry. They cannot: the key leads with the `media` row's id, which is unique, so
/// the shortened name is a label beside a discriminator rather than the discriminator itself.
/// The test below pins exactly that, because "the name is short" is not the property — "no two
/// files land on one path" is.
#[must_use]
pub fn safe_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous_was_dot = false;
    for character in name.chars() {
        if character == '.' {
            if previous_was_dot {
                continue;
            }
            previous_was_dot = true;
            out.push('.');
            continue;
        }
        previous_was_dot = false;
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    let trimmed: String = out.trim_matches('.').chars().take(120).collect();
    if trimmed.is_empty() {
        // A name made entirely of dots. The id still makes the path unique, so a fixed
        // fallback is enough.
        "file".to_owned()
    } else {
        trimmed
    }
}

/// The rows a run's media part will attempt, newest first within a site.
///
/// `deleted_at is null` and `purged_at is null` on purpose: a soft-deleted file is still a file
/// the operator may have to recover, and a purged one has been promised to be gone. Copying
/// either would be a surprise; skipping both is the promise.
pub async fn pending_objects(
    pool: &PgPool,
    site_id: Option<Uuid>,
) -> Result<Vec<MediaObject>> {
    let objects = sqlx::query_as::<_, MediaObject>(
        "select id, site_id, storage_key, filename, content_type, size_bytes, checksum from media \
         where deleted_at is null and purged_at is null \
           and ($1::uuid is null or site_id = $1) \
         order by site_id, created_at desc",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await?;
    Ok(objects)
}

/// One object after the copy loop has had a go at it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopiedObject {
    /// The `media` row this came from.
    pub id: Uuid,
    /// Site it belongs to.
    pub site_id: Uuid,
    /// Key in the live library — what a restore writes back to.
    pub storage_key: String,
    /// Name at upload time, kept verbatim for the operator's index.
    pub filename: String,
    /// Content type recorded at upload.
    pub content_type: String,
    /// Key inside the archive.
    pub archive_key: String,
    /// Bytes actually written.
    pub size_bytes: i64,
    /// SHA-256 of what was actually written, not what the row claimed.
    pub checksum: String,
}

/// What the copy loop did, and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaCopyReport {
    /// How many objects landed in the archive.
    pub objects_copied: i32,
    /// How many bytes the archive holds.
    pub bytes_copied: i64,
    /// How many objects could not be copied.
    pub objects_failed: i32,
    /// The first few failures, each naming the object and the store's own words.
    ///
    /// Capped and *reported* rather than silently dropped: a part that failed on 4 000 objects
    /// needs the operator to know it is 4 000, and the first three are enough to recognise the
    /// pattern (every key 404s, the store refuses the connection, one file is over the cap).
    pub failures: Vec<ObjectFailure>,
    /// Per-site totals, so the manifest can say which site a partial copy belongs to.
    pub sites: Vec<SiteCount>,
    /// The objects that landed, in the order they were copied.
    ///
    /// Held in the report rather than returned separately because the index has to be written
    /// from exactly the set the loop counted — two return values invites a caller that writes
    /// the index from one of them and records the size of the other, and a part whose recorded
    /// size is not the size of its own artifact is a part `verify` will call corrupt on a
    /// perfectly good archive.
    pub objects: Vec<CopiedObject>,
}

impl MediaCopyReport {
    /// Whether every object landed. A part whose `objects_failed` is non-zero is not a part.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.objects_failed == 0
    }

    /// One line for a part's error column.
    #[must_use]
    pub fn failure_summary(&self) -> String {
        if self.is_complete() {
            return String::new();
        }
        let first = self
            .failures
            .iter()
            .map(|failure| format!("{}: {}", failure.filename, failure.reason))
            .collect::<Vec<_>>()
            .join("; ");
        let mut summary = format!(
            "{} of {} objects could not be copied — {first}",
            self.objects_failed,
            self.objects_copied + self.objects_failed
        );
        if self.failures.len() < self.objects_failed as usize {
            summary.push_str(&format!(" (and {} more)", self.objects_failed as usize - self.failures.len()));
        }
        summary
    }
}

/// One object the copy could not make, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectFailure {
    /// The `media` row that could not be copied.
    pub id: Uuid,
    /// Its name, so an operator can find it in the library.
    pub filename: String,
    /// The store's own words, or this crate's reason.
    pub reason: String,
}

/// One site's share of a run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SiteCount {
    /// The site.
    pub site_id: Uuid,
    /// Objects copied for it.
    pub files: i32,
    /// Bytes copied for it.
    pub size_bytes: i64,
}

/// How many failure entries a report keeps.
///
/// A named constant rather than a literal at the call site, because the report's *total* and
/// its *sample* are two different numbers and the sample's size is the kind of thing that
/// changes in a later commit and breaks a test written against it. Public so a caller
/// rendering the report can say "and N more" from the same number the writer used.
pub const MAX_REPORTED_FAILURES: usize = 3;

/// Build the per-site rollup from the objects that landed.
///
/// Split out as a pure function so the interesting part — that a *failed* object's bytes are
/// counted for nobody — is testable without a database or an object store. Getting this wrong
/// in the other direction is just as bad: counting an object that failed would have the manifest
/// claim a size the archive does not have.
#[must_use]
pub fn roll_up_sites(copied: &[CopiedObject]) -> Vec<SiteCount> {
    let mut by_site: BTreeMap<Uuid, SiteCount> = BTreeMap::new();
    for object in copied {
        let entry = by_site.entry(object.site_id).or_insert_with(|| SiteCount {
            site_id: object.site_id,
            files: 0,
            size_bytes: 0,
        });
        entry.files += 1;
        entry.size_bytes += object.size_bytes;
    }
    by_site.into_values().collect()
}

/// The index the restore path reads: the objects, and the part's own contract about them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaIndex {
    /// Index format version, so a future restore can refuse an archive it does not understand
    /// instead of half-reading one.
    pub version: u32,
    /// The run's storage prefix this index was written under.
    pub prefix: String,
    /// The objects, in the order they were copied.
    pub objects: Vec<CopiedObject>,
}

/// Current index format version.
pub const INDEX_VERSION: u32 = 1;

/// Build the index for a run.
#[must_use]
pub fn build_index(prefix: &str, objects: &[CopiedObject]) -> MediaIndex {
    MediaIndex {
        version: INDEX_VERSION,
        prefix: crate::destination::storage_prefix(prefix),
        objects: objects.to_vec(),
    }
}

/// Copy every pending object into the archive, and say exactly what happened.
///
/// `write` is the caller's sink — a closure over the destination's own writer — so this
/// function is a loop with a contract rather than a second, backup-specific storage client.
/// The destination for backups is a *local root* today, and the media library's is a bucket;
/// making the copy work across both by giving the part one writer callback is what keeps the
/// two from growing a third storage abstraction that neither of them owns.
///
/// The loop's rules:
///
/// * **One object at a time.** The report is built as it goes, and a failure of one object does
///   not stop the others — a run that copied 4 998 of 5 000 files has still saved 4 998 files,
///   and the two that failed are named.
/// * **A mismatch is a failure.** The bytes that came back are hashed again rather than
///   believing the row's `checksum`; an object whose content disagrees with the row is exactly
///   the corruption this part exists to detect, and recording the row's value would launder it.
/// * **A size disagreement is a failure**, for the same reason, and it is checked *before* the
///   write so a truncated object never reaches the archive at all.
pub async fn copy_objects<F, Fut>(
    storage: &Storage,
    objects: &[MediaObject],
    prefix: &str,
    mut write: F,
) -> Result<MediaCopyReport>
where
    F: FnMut(String, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut report = MediaCopyReport::default();
    let mut copied: Vec<CopiedObject> = Vec::new();

    for object in objects {
        if object.size_bytes > MAX_OBJECT_BYTES {
            record_failure(
                &mut report,
                object,
                format!(
                    "the object is {} bytes, over this part's {} byte cap — it was not copied, \
                     not truncated",
                    object.size_bytes, MAX_OBJECT_BYTES
                ),
            );
            continue;
        }

        let bytes = match storage.get(&object.storage_key).await {
            Ok(bytes) => bytes,
            Err(error) => {
                record_failure(&mut report, object, error.to_string());
                continue;
            }
        };

        if bytes.len() as i64 != object.size_bytes {
            record_failure(
                &mut report,
                object,
                format!(
                    "the object is {} bytes but the library row records {} — the row and the \
                     store disagree, so neither can be trusted",
                    bytes.len(),
                    object.size_bytes
                ),
            );
            continue;
        }

        let actual = crate::part::bytes_checksum(&bytes);
        if !object.checksum.is_empty() && !actual.eq_ignore_ascii_case(&object.checksum) {
            record_failure(
                &mut report,
                object,
                format!(
                    "the object's SHA-256 is {actual} but the library row records {} — the \
                     object has changed since it was uploaded",
                    object.checksum
                ),
            );
            continue;
        }

        let archive_key = format!(
            "{}{}",
            crate::destination::storage_prefix(prefix),
            object_key(object.site_id, object.id, &safe_filename(&object.filename))
        );
        // Everything the index needs is read off the buffer BEFORE the write, so the buffer can
        // be moved into the writer and dropped there. A `clone()` here would hold two copies of
        // every object in memory at once, which is exactly what "one object at a time" is
        // supposed to mean — the peak is what decides whether a 200 MiB upload is copied or
        // makes the process swap.
        let size_bytes = bytes.len() as i64;
        if let Err(error) = write(archive_key.clone(), bytes).await {
            record_failure(&mut report, object, error.to_string());
            continue;
        }

        report.objects_copied += 1;
        report.bytes_copied += size_bytes;
        copied.push(CopiedObject {
            id: object.id,
            site_id: object.site_id,
            storage_key: object.storage_key.clone(),
            filename: object.filename.clone(),
            content_type: object.content_type.clone(),
            archive_key,
            size_bytes,
            checksum: actual,
        });
    }

    report.sites = roll_up_sites(&copied);
    report.objects = copied;
    Ok(report)
}

/// Record one failure, keeping the report's failure list bounded but honest about the total.
fn record_failure(report: &mut MediaCopyReport, object: &MediaObject, reason: String) {
    report.objects_failed += 1;
    if report.failures.len() < MAX_REPORTED_FAILURES {
        report.failures.push(ObjectFailure {
            id: object.id,
            filename: object.filename.clone(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(site: Uuid, name: &str, size: i64, checksum: &str) -> MediaObject {
        MediaObject {
            id: Uuid::new_v4(),
            site_id: site,
            storage_key: format!("sites/{site}/{name}"),
            filename: name.to_owned(),
            content_type: "image/png".to_owned(),
            size_bytes: size,
            checksum: checksum.to_owned(),
        }
    }

    fn copied(site: Uuid, name: &str, size: i64) -> CopiedObject {
        CopiedObject {
            id: Uuid::new_v4(),
            site_id: site,
            storage_key: format!("sites/{site}/{name}"),
            filename: name.to_owned(),
            content_type: "image/png".to_owned(),
            archive_key: format!("objects/{site}/{name}"),
            size_bytes: size,
            checksum: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn an_archive_key_cannot_collide_inside_one_site() {
        // The whole reason the id comes first: two libraries both contain "logo.png".
        let site = Uuid::new_v4();
        let a = object_key(site, Uuid::new_v4(), "logo.png");
        let b = object_key(site, Uuid::new_v4(), "logo.png");
        assert_ne!(a, b, "two files of the same name would overwrite one another");
    }

    #[test]
    fn a_name_from_another_site_cannot_collide_either() {
        let id = Uuid::new_v4();
        assert_ne!(
            object_key(Uuid::new_v4(), id, "logo.png"),
            object_key(Uuid::new_v4(), id, "logo.png")
        );
    }

    #[test]
    fn a_path_separator_in_a_filename_cannot_escape_the_objects_directory() {
        // The name reaches this module from an upload, and a name is a path segment. Without
        // the sanitiser, `../../manifest.json` would write over the run's own manifest and a
        // backup would overwrite the index that describes it.
        let key = object_key(Uuid::new_v4(), Uuid::new_v4(), &safe_filename("../../manifest.json"));
        assert!(!key.contains(".."), "{key}");
        assert_eq!(key.matches('/').count(), 2, "site and file only: {key}");
    }

    #[test]
    fn a_name_of_only_dots_still_produces_a_usable_segment() {
        // `...` trims to empty and falls back. A name made of separators does NOT — each one
        // becomes an ordinary underscore, which is a perfectly normal segment, and treating
        // that as degenerate would be a rule with no reason behind it.
        assert_eq!(safe_filename("..."), "file");
        assert_eq!(safe_filename("...."), "file");
        assert_eq!(safe_filename("/"), "_");
        assert_eq!(safe_filename("   "), "___");
    }

    #[test]
    fn no_archived_object_is_a_dotfile_or_ends_in_a_dot() {
        assert_eq!(safe_filename(".hidden"), "hidden");
        assert_eq!(safe_filename("trailing."), "trailing");
        assert!(!safe_filename(".gitignore").starts_with('.'));
    }

    #[test]
    fn a_long_name_is_shortened_to_fit_a_path_segment() {
        let long = format!("{}.png", "a".repeat(400));
        let shortened = safe_filename(&long);
        assert!(shortened.len() <= 120, "{}", shortened.len());
    }

    #[test]
    fn shortening_a_name_can_merge_two_names_and_still_lose_nothing() {
        // The honest version of the test above. Two 400-character names that share their first
        // 120 characters DO shorten to the same segment — the property is not "names stay
        // distinct", it is "no two files land on the same path", and that is true because the
        // id leads the key. Written the naive way this test would have forced a hash suffix on
        // every name, which is uglier and buys nothing.
        let a = format!("{}-1.png", "a".repeat(400));
        let b = format!("{}-2.png", "a".repeat(400));
        assert_eq!(safe_filename(&a), safe_filename(&b), "the names do merge");
        let site = Uuid::new_v4();
        let left = object_key(site, Uuid::new_v4(), &safe_filename(&a));
        let right = object_key(site, Uuid::new_v4(), &safe_filename(&b));
        assert_ne!(left, right, "but the ids keep the paths apart");
    }

    #[test]
    fn a_name_keeps_its_extension_so_an_operator_can_browse_the_archive() {
        assert!(safe_filename("Holiday Photo (1).JPEG").ends_with(".JPEG"));
    }

    #[test]
    fn only_copied_objects_count_towards_a_site() {
        // A failed object contributes nothing: the manifest must not claim bytes the archive
        // does not hold, or the restore wizard sizes itself off a number that is a fiction.
        let site = Uuid::new_v4();
        let other = Uuid::new_v4();
        let roll = roll_up_sites(&[copied(site, "a.png", 10), copied(site, "b.png", 20)]);
        assert_eq!(roll.len(), 1);
        assert_eq!(roll[0].files, 2);
        assert_eq!(roll[0].size_bytes, 30);
        assert_eq!(roll_up_sites(&[]), Vec::new());
        assert_eq!(roll_up_sites(&[copied(site, "a.png", 1), copied(other, "b.png", 2)]).len(), 2);
    }

    #[test]
    fn sites_are_ordered_so_two_runs_over_the_same_library_produce_the_same_manifest() {
        let a = Uuid::from_u128(9);
        let b = Uuid::from_u128(1);
        let forward = roll_up_sites(&[copied(a, "x", 1), copied(b, "y", 1)]);
        let backward = roll_up_sites(&[copied(b, "y", 1), copied(a, "x", 1)]);
        assert_eq!(forward, backward);
        assert_eq!(forward[0].site_id, b, "BTreeMap order, not insertion order");
    }

    #[test]
    fn a_report_with_no_failures_says_nothing_about_failure() {
        let report = MediaCopyReport {
            objects_copied: 3,
            bytes_copied: 30,
            ..MediaCopyReport::default()
        };
        assert!(report.is_complete());
        assert_eq!(report.failure_summary(), "");
    }

    #[test]
    fn a_failure_summary_names_the_file_and_the_reason_and_keeps_the_total() {
        let mut report = MediaCopyReport {
            objects_copied: 2,
            objects_failed: 5,
            failures: vec![ObjectFailure {
                id: Uuid::new_v4(),
                filename: "hero.png".to_owned(),
                reason: "no such key".to_owned(),
            }],
            ..MediaCopyReport::default()
        };
        let summary = report.failure_summary();
        assert!(summary.contains("hero.png"), "{summary}");
        assert!(summary.contains("no such key"), "{summary}");
        assert!(summary.contains("5 of 7"), "{summary}");
        assert!(summary.contains("and 4 more"), "{summary}");
        // The count is the whole truth, so it cannot be a number the report chose.
        report.objects_failed = 1;
        assert!(!report.failure_summary().contains("and"));
    }

    #[test]
    fn a_report_is_incomplete_the_moment_one_object_fails() {
        let report = MediaCopyReport {
            objects_copied: 99,
            objects_failed: 1,
            ..MediaCopyReport::default()
        };
        assert!(!report.is_complete(), "99 of 100 is not a backup");
    }

    #[test]
    fn the_index_records_the_prefix_it_was_written_under() {
        let index = build_index("backups/2026-09-29/run", &[]);
        assert_eq!(index.prefix, "/backups/2026-09-29/run/");
        assert_eq!(index.version, INDEX_VERSION);
        assert!(index.objects.is_empty());
    }

    #[tokio::test]
    async fn an_object_over_the_cap_is_named_rather_than_truncated() {
        // The store is never asked: the cap is a statement about what this part will do, and
        // the point is that the operator is told, not that the read is attempted.
        let store = Storage::from_env().expect("the default store configures");
        let big = object(Uuid::new_v4(), "huge.mov", MAX_OBJECT_BYTES + 1, "");
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = written.clone();
        let report = copy_objects(&store, &[big], "/run/", move |key: String, _body: Vec<u8>| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(key);
                Ok(())
            }
        })
        .await
        .expect("the report is not an error");
        assert_eq!(report.objects_copied, 0);
        assert_eq!(report.objects_failed, 1);
        assert!(written.lock().unwrap().is_empty(), "nothing may be written");
        let summary = report.failure_summary();
        assert!(summary.contains("huge.mov"), "{summary}");
        assert!(summary.contains("not truncated"), "{summary}");
    }

    #[tokio::test]
    async fn a_key_the_store_does_not_have_is_a_failure_and_the_loop_continues() {
        let store = Storage::from_env().expect("the default store configures");
        let mut missing = object(Uuid::new_v4(), "gone.png", 4, "");
        missing.storage_key = "omnion/does/not/exist/anything.png".to_owned();
        let report = copy_objects(&store, &[missing], "/run/", |_key, _body: Vec<u8>| async {
            Ok(())
        })
        .await
        .expect("a missing object is a report, not an error");
        assert_eq!(report.objects_failed, 1);
        assert_eq!(report.objects_copied, 0);
        assert!(!report.is_complete());
    }
}
