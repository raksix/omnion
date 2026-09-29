//! Writing an archive's objects **back into the live library** (REQ-013, slice 2b).
//!
//! # Why this is a separate file from the preview
//!
//! [`crate::preview`] *reads* the live side and the archive; this *writes* one back into the
//! other. The two are kept apart for the reason they always are: the reading half is safe to
//! run for anybody, the writing half is not, and a file that does both is a file whose
//! safety argument has to be argued for the whole module.
//!
//! # The four rules, each one a shortcut that produces a plausible wrong answer
//!
//! * **An object is written back only after its bytes are re-hashed and compared with the
//!   index's own checksum.** The index is the archive's account of what it copied, and a
//!   truncated object on the destination is the exact case the whole `verify` route exists
//!   for. Writing it blindly would be the backup defect at the other end: a restore that
//!   reports success over bytes it never checked.
//! * **The library row is matched by the archive's `storage_key`, not by its id.** A restore
//!   that wrote rows back under the ids the run happened to have would resurrect deleted
//!   files and drop live ones whose ids differ; the key is what a page resolves.
//! * **A part that cannot finish is reported, not rolled back.** Objects already written
//!   stay: a restore is *not* transactional against an object store, and a rollback that
//!   deletes what it just wrote is the operation nobody can audit afterwards. The report
//!   says exactly how many landed.
//! * **Nothing is deleted by a restore.** The archive is added to the library; a file
//!   uploaded after the run is *priced* by the preview and removed by nothing here, because
//!   the pricing already happened and the operator agreed to it. [`MediaRestoreReport`]
//!   carries a `dropped` field that is zero by construction, and that is what stops a second
//!   implementation of the drop from appearing somewhere quieter.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{BackupError, Result};
use crate::media::{CopiedObject, MediaIndex, INDEX_VERSION};

/// How many failures one restore will name before it says "and N more".
///
/// A restore of a 200 000-object library with a store outage in the middle would otherwise
/// return a 40 MB error body, and the operator's browser would show a wall of text instead
/// of the one sentence that matters. The **count** is never capped — only the list is.
pub const MAX_REPORTED_FAILURES: usize = 20;

/// A boxed future, so the two collaborators below can be closures without pulling in a
/// futures crate for a type alias.
pub type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Where an archive's bytes are read from.
///
/// A trait rather than a closure parameter because there are two collaborators and three
/// call sites, and a function with three higher-ranked closure parameters is a signature
/// nobody can read. Implemented for `dyn Fn` below so a caller still passes a closure.
pub trait ArchiveReader: Send + Sync {
    /// Read one archived object, or say why it could not be read.
    ///
    /// **Owned, not borrowed** — and that is not a style choice. The borrowed form needs a
    /// `for<'a> Fn(&'a CopiedObject)` bound, which a closure literal at a call site
    /// infers for one *specific* lifetime, so the trait's blanket impl is unreachable from
    /// exactly the call sites it exists to serve: every test and the route alike. The clone
    /// is a 7-field struct of Strings and costs nothing next to reading an object.
    fn read(&self, object: CopiedObject) -> Boxed<Result<Vec<u8>>>;
}

impl<F> ArchiveReader for F
where
    F: Fn(CopiedObject) -> Boxed<Result<Vec<u8>>> + Send + Sync,
{
    fn read(&self, object: CopiedObject) -> Boxed<Result<Vec<u8>>> {
        (self)(object)
    }
}

/// Where a restored object is written to.
pub trait LibraryWriter: Send + Sync {
    /// Store one object, or say why the store refused.
    fn write(&self, object: CopiedObject, bytes: Vec<u8>) -> Boxed<Result<()>>;
}

impl<F> LibraryWriter for F
where
    F: Fn(CopiedObject, Vec<u8>) -> Boxed<Result<()>> + Send + Sync,
{
    fn write(&self, object: CopiedObject, bytes: Vec<u8>) -> Boxed<Result<()>> {
        (self)(object, bytes)
    }
}

/// Refreshes the library row that points at an object; `false` when the site is gone.
pub trait RowToucher: Send + Sync {
    /// Touch the row, reporting whether it still exists.
    fn touch(&self, object: CopiedObject) -> Boxed<bool>;
}

impl<F> RowToucher for F
where
    F: Fn(CopiedObject) -> Boxed<bool> + Send + Sync,
{
    fn touch(&self, object: CopiedObject) -> Boxed<bool> {
        (self)(object)
    }
}

/// One object that could not be written back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreFailure {
    /// The `storage_key` the library knows it by.
    pub storage_key: String,
    /// The archive's key inside its own prefix, for a hand-check.
    pub archive_key: String,
    /// What went wrong, in the store's words.
    pub reason: String,
}

/// What a media restore did, and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRestoreReport {
    /// Objects written back into the live library.
    pub objects_restored: i32,
    /// Rows inserted or refreshed to match them.
    pub rows_touched: i32,
    /// Bytes the live store now holds for this archive.
    pub bytes_restored: i64,
    /// Objects that could not be written.
    pub objects_failed: i32,
    /// The named failures, capped at [`MAX_REPORTED_FAILURES`].
    pub failures: Vec<RestoreFailure>,
    /// Live items the restore removed.
    ///
    /// Always zero, and present anyway. The preview priced this number and the operator
    /// agreed to it; a field that *could* hold a non-zero value is a field somebody will
    /// fill in somewhere else.
    pub dropped: i64,
}

impl MediaRestoreReport {
    /// Whether every archived object landed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.objects_failed == 0
    }

    /// One sentence for the route's response and the audit entry.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_complete() {
            return format!(
                "{} object{} restored ({} row{} touched)",
                self.objects_restored,
                if self.objects_restored == 1 { "" } else { "s" },
                self.rows_touched,
                if self.rows_touched == 1 { "" } else { "s" },
            );
        }
        let named = self
            .failures
            .iter()
            .take(MAX_REPORTED_FAILURES)
            .map(|failure| failure.storage_key.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        // From the **count**, not from the list. `failures` is already capped, so
        // `failures.len() - MAX` is always zero — a summary that announces "and 0 more"
        // after naming twenty of thirty failures, which is a report that under-reports
        // exactly the case the cap exists for.
        let hidden = self
            .objects_failed
            .saturating_sub(self.failures.len() as i32);
        format!(
            "{}/{} objects restored, {} could not be written: {}{}",
            self.objects_restored,
            self.objects_restored + self.objects_failed,
            self.objects_failed,
            named,
            if hidden > 0 {
                format!(", and {hidden} more")
            } else {
                String::new()
            }
        )
    }
}

/// Read the archive's index, refusing an archive this build does not understand.
///
/// The version check is **not** a formality. `MediaIndex` is `serde`, and serde's default
/// is to ignore unknown fields and accept a missing one as its `Default` — so a v2 index
/// read by this build would deserialise into an empty `objects` list and the restore would
/// report "0 objects restored" for a full archive. That is a success that restored nothing,
/// which is the one sentence this whole feature must never produce.
pub fn read_index(bytes: &[u8]) -> Result<MediaIndex> {
    let index: MediaIndex = serde_json::from_slice(bytes)
        .map_err(|error| BackupError::Rejected(format!("the media index is not readable: {error}")))?;
    if index.version != INDEX_VERSION {
        return Err(BackupError::Rejected(format!(
            "this archive's media index is version {} and this build reads version {INDEX_VERSION}",
            index.version
        )));
    }
    Ok(index)
}

/// The archive's objects, or the reason there are none.
pub fn index_objects(bytes: &[u8]) -> Result<Vec<CopiedObject>> {
    Ok(read_index(bytes)?.objects)
}

/// Distinct sites among the archived objects, capped, with the cap reported.
#[must_use]
pub fn archived_sites(objects: &[CopiedObject], cap: usize) -> (Vec<String>, bool) {
    let mut sites: Vec<String> = Vec::new();
    let mut truncated = false;
    for object in objects {
        let id = object.site_id.to_string();
        if sites.contains(&id) {
            continue;
        }
        if sites.len() == cap {
            truncated = true;
            break;
        }
        sites.push(id);
    }
    (sites, truncated)
}

/// Write every archived object back into the live library.
///
/// The order is the index's order, and the first failure does **not** stop the loop: a
/// restore that aborts on the first missing object leaves the archive half-applied and the
/// operator with no way to tell which half. Every object is attempted, and the report says
/// how many landed.
pub async fn restore_objects(
    objects: &[CopiedObject],
    reader: &(dyn ArchiveReader + '_),
    writer: &(dyn LibraryWriter + '_),
    toucher: &(dyn RowToucher + '_),
) -> MediaRestoreReport {
    let mut report = MediaRestoreReport::default();

    for object in objects {
        // The bytes come back and are re-hashed **before** anything is written. A corrupt
        // object that is written first and checked afterwards leaves a wrong file in the
        // live library with a correct-looking row, and the only repair is a second restore.
        let bytes = match reader.read(object.clone()).await {
            Ok(bytes) => bytes,
            Err(error) => {
                count_failure(&mut report, object, error.to_string());
                continue;
            }
        };

        let checksum = crate::part::bytes_checksum(&bytes);
        if checksum != object.checksum {
            count_failure(
                &mut report,
                object,
                format!("the archived bytes hash to {checksum}, the index recorded {}", object.checksum),
            );
            continue;
        }

        if let Err(error) = writer.write(object.clone(), bytes).await {
            count_failure(&mut report, object, error.to_string());
            continue;
        }

        report.objects_restored += 1;
        report.bytes_restored += object.size_bytes.max(0);
        if toucher.touch(object.clone()).await {
            report.rows_touched += 1;
        }
    }

    report
}

/// Record one failure, capping the **list** and never the count.
fn count_failure(report: &mut MediaRestoreReport, object: &CopiedObject, reason: String) {
    report.objects_failed += 1;
    if report.failures.len() < MAX_REPORTED_FAILURES {
        report.failures.push(RestoreFailure {
            storage_key: object.storage_key.clone(),
            archive_key: object.archive_key.clone(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn object(id: u8, site: u8, checksum: &str) -> CopiedObject {
        CopiedObject {
            id: Uuid::from_u128(u128::from(id)),
            site_id: Uuid::from_u128(u128::from(site)),
            storage_key: format!("site/{site}/file-{id}.png"),
            filename: format!("file-{id}.png"),
            content_type: "image/png".to_owned(),
            archive_key: format!("archive/file-{id}.png"),
            size_bytes: 10,
            checksum: checksum.to_owned(),
        }
    }

    fn hashed(bytes: &[u8]) -> String {
        crate::part::bytes_checksum(bytes)
    }

    /// Every object is written, every row is touched, and the order is the index's.
    #[tokio::test]
    async fn every_archived_object_is_written_back_and_counted() {
        let objects = vec![object(1, 1, &hashed(b"one")), object(2, 1, &hashed(b"two"))];
        let written: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = written.clone();
        let reader = |object: CopiedObject| {
            let bytes = if object.storage_key.ends_with("-1.png") {
                b"one".to_vec()
            } else {
                b"two".to_vec()
            };
            Box::pin(async move { Ok(bytes) }) as Boxed<Result<Vec<u8>>>
        };
        let writer = move |object: CopiedObject, _bytes: Vec<u8>| {
            let sink = sink.clone();
            let key = object.storage_key.clone();
            Box::pin(async move {
                sink.lock().expect("a lock").push(key);
                Ok(())
            }) as Boxed<Result<()>>
        };
        let report = restore_objects(
            &objects,
            &reader,
            &writer,
            &|_object| Box::pin(async { true }) as Boxed<bool>,
        )
        .await;
        assert_eq!(
            *written.lock().expect("a lock"),
            vec!["site/1/file-1.png", "site/1/file-2.png"],
            "the index's order, and every object"
        );
        assert_eq!(report.objects_restored, 2);
        assert_eq!(report.rows_touched, 2);
        assert_eq!(report.bytes_restored, 20);
        assert!(report.is_complete());
    }

    /// A corrupt object is refused and **not written**. This is the whole point of
    /// re-hashing: the first version of the backup path wrote bytes nobody checked, and
    /// this is the same defect read from the other end.
    #[tokio::test]
    async fn a_corrupt_archived_object_is_refused_and_never_written() {
        let objects = vec![object(1, 1, &hashed(b"the original"))];
        let written: Arc<Mutex<usize>> = Arc::default();
        let sink = written.clone();
        let report = restore_objects(
            &objects,
            &|_object| Box::pin(async { Ok(b"truncated".to_vec()) }) as Boxed<Result<Vec<u8>>>,
            &move |_object: CopiedObject, _bytes: Vec<u8>| {
                let sink = sink.clone();
                Box::pin(async move {
                    *sink.lock().expect("a lock") += 1;
                    Ok(())
                }) as Boxed<Result<()>>
            },
            &|_object| Box::pin(async { true }) as Boxed<bool>,
        )
        .await;
        assert_eq!(
            *written.lock().expect("a lock"),
            0,
            "a corrupt object must not be written"
        );
        assert_eq!(report.objects_restored, 0);
        assert_eq!(report.objects_failed, 1);
        assert!(!report.is_complete());
        assert!(report.failures[0]
            .reason
            .contains("the archived bytes hash to"));
    }

    /// One missing object does not stop the others. A restore that aborted on the first
    /// failure would leave the archive half-applied and the operator with no way to tell
    /// which half.
    #[tokio::test]
    async fn one_unreadable_object_does_not_stop_the_others() {
        // Real checksums, deliberately: a placeholder like "x" makes the reader's bytes
        // fail the hash check, so all three are refused as *corrupt* rather than one being
        // refused as *missing*, and the test passes for the wrong reason.
        let objects = vec![
            object(1, 1, &hashed(b"x")),
            object(2, 1, &hashed(b"x")),
            object(3, 1, &hashed(b"x")),
        ];
        let report = restore_objects(
            &objects,
            &|object: CopiedObject| {
                let name = object.storage_key.clone();
                Box::pin(async move {
                    if name.ends_with("-2.png") {
                        Err(BackupError::Rejected(
                            "the object is not in the archive".to_owned(),
                        ))
                    } else {
                        Ok(b"x".to_vec())
                    }
                }) as Boxed<Result<Vec<u8>>>
            },
            &|_object: CopiedObject, _bytes: Vec<u8>| Box::pin(async { Ok(()) }) as Boxed<Result<()>>,
            &|_object| Box::pin(async { true }) as Boxed<bool>,
        )
        .await;
        assert_eq!(report.objects_restored, 2);
        assert_eq!(report.objects_failed, 1);
        assert!(!report.is_complete());
    }

    /// A write the store refuses is a failure, and the loop continues.
    #[tokio::test]
    async fn a_refused_write_is_a_failure_not_a_panic() {
        let objects = vec![object(1, 1, &hashed(b"one"))];
        let report = restore_objects(
            &objects,
            &|_object| Box::pin(async { Ok(b"one".to_vec()) }) as Boxed<Result<Vec<u8>>>,
            &|_object: CopiedObject, _bytes: Vec<u8>| {
                Box::pin(async { Err(BackupError::Rejected("disk full".to_owned())) })
                    as Boxed<Result<()>>
            },
            &|_object| Box::pin(async { true }) as Boxed<bool>,
        )
        .await;
        assert_eq!(report.objects_failed, 1);
        assert!(report.failures[0].reason.contains("disk full"));
        assert_eq!(report.bytes_restored, 0);
        assert_eq!(report.objects_restored, 0);
    }

    /// An object written but whose row could not be refreshed is still a restored object —
    /// the bytes are in the store, and a missing row is a different, recoverable problem
    /// that the report's own split is there to show.
    #[tokio::test]
    async fn a_write_whose_row_is_gone_still_counts_as_restored() {
        let objects = vec![object(1, 1, &hashed(b"one"))];
        let report = restore_objects(
            &objects,
            &|_object| Box::pin(async { Ok(b"one".to_vec()) }) as Boxed<Result<Vec<u8>>>,
            &|_object: CopiedObject, _bytes: Vec<u8>| Box::pin(async { Ok(()) }) as Boxed<Result<()>>,
            &|_object| Box::pin(async { false }) as Boxed<bool>,
        )
        .await;
        assert_eq!(report.objects_restored, 1);
        assert_eq!(report.rows_touched, 0);
        assert!(report.is_complete());
    }

    /// A **v2 index read by this build** would deserialise into an empty object list under
    /// serde's default, and the restore would answer "0 objects restored" for a full
    /// archive. The version check is what stops that.
    #[test]
    fn an_index_from_a_future_build_is_refused_not_read_as_empty() {
        let mut index = crate::media::build_index(
            "backups/run",
            &[object(1, 1, &hashed(b"one"))],
        );
        index.version = INDEX_VERSION + 1;
        let bytes = serde_json::to_vec(&index).expect("an index");
        let error = read_index(&bytes).expect_err("must be refused");
        assert!(
            error.to_string().contains("this build reads version"),
            "must name the versions: {error}"
        );
    }

    /// An index with an **unknown extra field** is still read, because that is what forward
    /// compatibility looks like and refusing it would break every restore the day the format
    /// grows a field.
    #[test]
    fn an_index_with_an_unknown_field_is_still_read() {
        let index = crate::media::build_index("backups/run", &[object(1, 1, "x")]);
        let mut value: serde_json::Value = serde_json::to_value(&index).expect("json");
        value["something_new"] = serde_json::json!(true);
        let read = read_index(&serde_json::to_vec(&value).expect("json")).expect("readable");
        assert_eq!(read.objects.len(), 1);
    }

    #[test]
    fn unreadable_bytes_are_refused() {
        let error = read_index(b"{not json").expect_err("must be refused");
        assert!(error.to_string().contains("not readable"));
    }

    /// The site list is capped **and says so**. Ten sites is a list; a hundred thousand is
    /// a payload that times out the request, and a request that times out looks exactly like
    /// one that found nothing.
    #[test]
    fn a_capped_site_list_reports_the_cap() {
        let objects: Vec<CopiedObject> = (1..=12_u8).map(|n| object(n, n, "x")).collect();
        let (sites, truncated) = archived_sites(&objects, 10);
        assert_eq!(sites.len(), 10);
        assert!(truncated);
        let (sites, truncated) = archived_sites(&objects[..2], 10);
        assert_eq!(sites.len(), 2);
        assert!(!truncated);
    }

    /// The failure list is capped and the summary says how many are hidden — a truncated
    /// list that does not say it is truncated is a report that under-reports a failure.
    #[tokio::test]
    async fn many_failures_are_capped_and_the_cap_is_stated() {
        let objects: Vec<CopiedObject> = (1..=30_u8)
            .map(|n| object(n, 1, &hashed(b"x")))
            .collect();
        let report = restore_objects(
            &objects,
            &|_object| Box::pin(async { Ok(b"x".to_vec()) }) as Boxed<Result<Vec<u8>>>,
            &|_object: CopiedObject, _bytes: Vec<u8>| {
                Box::pin(async { Err(BackupError::Rejected("nope".to_owned())) })
                    as Boxed<Result<()>>
            },
            &|_object| Box::pin(async { true }) as Boxed<bool>,
        )
        .await;
        assert_eq!(report.objects_failed, 30, "the count is never capped");
        assert_eq!(report.failures.len(), MAX_REPORTED_FAILURES);
        assert!(report.summary().contains("and 10 more"));
    }

    /// A complete report reads as a sentence an operator can act on.
    #[test]
    fn a_complete_report_sums_objects_and_rows() {
        let report = MediaRestoreReport {
            objects_restored: 3,
            rows_touched: 3,
            bytes_restored: 30,
            ..MediaRestoreReport::default()
        };
        assert!(report.is_complete());
        assert_eq!(report.summary(), "3 objects restored (3 rows touched)");
    }

    /// `dropped` is zero by construction and a test says so, because the preview already
    /// priced it and a second implementation is how it would get filled in.
    #[test]
    fn a_restore_drops_nothing() {
        assert_eq!(MediaRestoreReport::default().dropped, 0);
    }
}
