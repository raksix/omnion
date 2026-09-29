//! Omnion backup centre — parts, manifests, verification and retention
//! (docs/requests/REQ-013).
//!
//! A backup screen has one job that is unlike every other screen in the panel: **it must not
//! claim a restore point exists when it cannot prove one does.** Every other list here can
//! fall back to something plausible; this one cannot, because a green row that was never
//! written to a destination is the most expensive thing the product can render — the
//! operator finds out on the day the volume is gone.
//!
//! The rule is written into the shape of the code rather than left to each caller's
//! judgement:
//!
//! * **A part is a value, not a file.** `part` is pure: no database, no storage, no clock.
//!   A run reaches `succeeded` because [`summarise`] read its parts, never because a handler
//!   wrote the word — so the status of a run cannot be asserted by whoever ran it.
//! * **A part that produced nothing is still a part.** `plugins` is an honest empty result
//!   until a package installer exists, and a manifest that lists only what worked cannot
//!   answer the question the restore wizard asks first: *was media part of this run at
//!   all?*
//! * **Verification is a result, not an exception.** [`verify_manifest`] returns what it
//!   found. A mismatch is the answer the operator asked for, and raising it as an error
//!   would make the screen say "verification failed" without saying *what* failed.
//! * **A run is a snapshot of itself.** The manifest records what the parts were when they
//!   were produced, so reading it back after a restore describes the artifact rather than
//!   the platform.
//!
//! [`part`] is the model and the rules; [`store`] is the four tables; [`destination`] is
//! where bytes go and what "writable" means. It is infrastructure in the shape of
//! `omnion-audit` and `omnion-events`: this crate knows what a backup *is*, not how one
//! particular deployment takes its database dump.

#![forbid(unsafe_code)]

pub mod apply;
pub mod destination;
pub mod error;
pub mod media;
pub mod part;
pub mod preview;
pub mod purge;
pub mod restore;
pub mod restore_objects;
pub mod store;
pub mod sweep;

pub use apply::{
    ArchiveFacts, MAX_MEDIA_OBJECTS, MAX_SELECTED_PARTS, PlanError, PlanRefusal, RestorePlan,
    RestoreRequest, build_plan,
};
pub use destination::{
    DestinationReport, PROBE_FILENAME, local_path_for, local_root_for, probe_local, storage_key,
    storage_prefix,
};
pub use error::{BackupError, Result};
pub use media::{
    CopiedObject, INDEX_FILENAME, INDEX_VERSION, MAX_OBJECT_BYTES, MAX_REPORTED_FAILURES,
    MediaCopyReport, MediaIndex, MediaObject, OBJECTS_DIR, ObjectFailure, SiteCount, build_index,
 copy_objects, index_key, object_key, pending_objects, pending_objects_for_organization, roll_up_sites,
    safe_filename,
};
pub use part::{
    MANIFEST_VERSION, MAX_ERROR_LENGTH, MAX_LABEL_LENGTH, Manifest, ObservedPart, PARTS, Part,
    PartStatus, RunStatus, Verification, build_manifest, bytes_checksum, canonical_json,
    manifest_checksum, normalise_scopes, summarise, truncate_error, validate_label,
    verify_manifest,
};
pub use purge::{
    MAX_REPORTED_PURGE_FAILURES, PROBE_MARKER, PurgeFailure, PurgeReport, remove_run_artifacts,
    run_directory,
};
pub use preview::{LiveComparison, MAX_MATCHED_KEYS, compare_database, compare_media};
pub use restore_objects::{
    ArchiveReader, Boxed, LibraryWriter, MediaRestoreReport, RestoreFailure, RowToucher,
    MAX_REPORTED_FAILURES as MAX_REPORTED_RESTORE_FAILURES, archived_sites, index_objects, read_index,
    restore_objects,
};
pub use restore::{
    LiveCounts, MAX_REPORTED_SITES, PartEvidence, RestoreMode, RestorePreview, RestoreWarning,
    RestoreWarningCode, STALE_AFTER_DAYS, WarningSeverity, build_preview, confirm_phrase,
    reported_sites,
};
pub use store::{
    Backup, BackupPage, BackupQuery, BackupSchedule, BackupSettings, NewBackup, NewPart,
    NewSchedule, NewSettings, PartTotals, StatusTotals, count_by_status, delete_backup,
    delete_schedule, find_backup, finish_run, insert_backup, insert_part, list_backups, list_parts,
    list_schedules, load_settings, manifest_of, next_due_schedules, organizations_with_backups,
    protected_backup_count, prune_candidates, record_schedule_run, save_part, save_settings,
    schedule_appears_due, set_prefix, set_protected, start_run, totals, upsert_schedule,
};
pub use sweep::{
    MAX_REPORTED_STRANDED, StrandedArtifact, SweepReport, sweep_all, sweep_organization,
};
