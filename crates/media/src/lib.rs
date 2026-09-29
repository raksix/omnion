//! Omnion media — the media library of the platform.
//!
//! The library is the row half of the media system: what was uploaded, by whom, for which site,
//! how big it is and where its bytes live (docs/01-VISION.md §5, docs/04-MONOREPO.md `media/`).
//! The bytes themselves are moved by `omnion-storage`, so an installation can keep its library
//! on MinIO, on any S3-compatible endpoint or on a local directory without changing a row.
//!
//! The validation rules that guard the outside world — file names, content types, object keys,
//! what may be rendered inline — live in [`validation`] so the API and any future worker share
//! them (docs/requests/REQ-010 extends this module into the enterprise file manager).

#![forbid(unsafe_code)]

pub mod browser;
pub mod duplicates;
pub mod error;
pub mod exif;
pub mod folder_store;
pub mod folders;
pub mod grants;
pub mod library;
pub mod model;
pub mod pixels;
pub mod preset_store;
pub mod probe;
pub mod ranges;
pub mod retention;
pub mod scanning;
pub mod shares;
pub mod storage_settings;
pub mod transform;
pub mod usage;
pub mod validation;
pub mod versions;

pub use browser::{
    FilePage, ListQuery, MetadataPatch, Sort, TrashEntry, assert_same_site, count_files,
    count_in_folder, files_in_folder, find_file, find_file_any_state, list_files, list_trash,
    purge_files, restore_files, storage_keys, trash_files, trash_summary, trashed_ids, update_file,
};
pub use duplicates::{
    CrossSiteCopy, CrossSiteGroup, DuplicateGroup, DuplicateMember, MAX_CROSS_SITE_SITES,
    MergeOutcome, NewReference, Reference, SiteLabel, clear_references, count_live_shares,
    count_references, duplicate_groups, duplicate_groups_across, group_members, list_references,
    merge_group, reclaimable_total, record_reference, repoint_references, site_labels,
};
pub use error::{MediaError, Result};
pub use exif::{EXIF_HEADER_BYTES, Exif, ORIENTATION_TAG, oriented_size, read as read_exif};
pub use folder_store::{
    count_files_in_folder, delete_empty_folder, find_folder, insert_folder, list_folders,
    move_folder, root_folder,
};
pub use folders::{
    Folder, FolderMove, MAX_FOLDER_NAME_LENGTH, MAX_FOLDER_PATH_LENGTH, NewFolder,
    ROOT_FOLDER_NAME, child_path, sanitize_folder_name, subtree_pattern, subtree_predicate,
    validate_folder_name,
};
pub use grants::{
    Capabilities, Chain, ChainNode, Decision, Grant, GrantTarget, MAX_CHAIN_DEPTH, NewGrant,
    SUBJECT_KINDS, delete_grant, group_ids_of, list_grants, load_chain, put_grant, resolve,
};
pub use library::{delete_media, find_media, insert_media, list_media};
pub use model::{MAX_FILENAME_LENGTH, MAX_UPLOAD_BYTES, Media, MediaFile, NewMedia};
pub use pixels::{Box2, Transformed, apply, decode, target_box, transform_bytes};
pub use preset_store::{
    NewDerivative, STANDARD_PRESET, Served, clear_derivatives, create_preset, delete_preset,
    derivative_filename, derivative_keys, derivative_totals, ensure_default_presets,
    find_derivative, find_preset, find_preset_by_name, insert_derivative, list_derivatives,
    list_presets, require_preset, require_preset_by_id, served_for, update_preset,
};
pub use probe::{HEADER_BYTES, MediaProbe, probe};
pub use ranges::{ByteWindow, RangePlan};
pub use retention::{
    DanglingReference, MAX_POLICY_NAME_LENGTH, MAX_WINDOW_DAYS, MIN_WINDOW_DAYS,
    NewRetentionPolicy, PolicyChanges, PurgeOutcome, PurgeRefusal, RetentionPolicy, RetentionRun,
    RunTotals, SWEEP_BATCH as RETENTION_SWEEP_BATCH, VersionSweep, Window, all_keys_of,
    begin_run as begin_retention_run, create_policy, dangling_references, delete_policy,
    enabled_policies, find_policy, finish_run as finish_retention_run, governing_window, last_run,
    list_policies, list_runs as list_retention_runs, past_restore_window, policy_scope_paths,
    purge_candidates, purge_eligible, repair_references, set_hold, site_policy, sites_with_media,
    sweep_versions, update_policy, validate_new as validate_retention,
};
pub use scanning::{
    MAX_SCAN_MB, MAX_TIMEOUT_SECONDS, MIN_SCAN_MB, MIN_TIMEOUT_SECONDS, NewSiteScan, PendingScan,
    Quarantine, ScanRequest, ScanResponse, ScanRun, ServeRefusal, SiteScan, SweepCounts, Verdict,
    apply_verdict, begin_run, claim_pending, close_quarantine, finish_run, interpret,
    is_quarantined, list_quarantines, list_runs, may_serve, parse_response, quarantine_totals,
    read_scan_settings, scan_identity, write_scan_settings,
};
pub use shares::{
    CreatedShare, MAX_EXPIRY_DAYS, MIN_EXPIRY_MINUTES, NewShare, Share, ShareRefusal, TOKEN_BYTES,
    count_download, create_share, find_by_token, find_share, hash_token, is_password_protected,
    list_shares, mint_token, revoke_for_media, revoke_share, servable,
};
pub use storage_settings::{
    ConnectionProbe, MAX_SIGNED_URL_TTL, MAX_UPLOAD_MB, MIN_SIGNED_URL_TTL, MIN_UPLOAD_MB,
    NewSiteStorage, SiteStorage, describe_public_base, describe_target, effective_max_upload_bytes,
    probe_key, read_storage_settings, validate_new as validate_storage, write_storage_settings,
};
pub use transform::{
    Derivative, Fit, ImageFormat, MAX_PRESET_DIMENSION, MAX_PRESET_NAME_LENGTH, NewPreset, Preset,
    Recipe, derivative_prefix, validate_dimensions, validate_new, validate_preset_name,
    validate_quality,
};
pub use usage::{
    MAX_USAGE_ROWS, RESOLVABLE_KINDS, UsageCounts, UsageEntry, count_usage, list_usage,
};
pub use validation::{
    INLINE_CONTENT_TYPES, ServePlan, normalize_content_type, object_key, sanitize_filename,
    serve_plan,
};
pub use versions::{
    MAX_NOTE_LENGTH, MediaVersion, NewVersion, VersionTransaction, all_storage_keys,
    append_version, begin_version, commit_version, count_versions, ensure_version_one,
    fill_dimensions, fill_exif, find_version, list_versions, next_version, normalize_note,
    probe_of,
};
