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
pub mod error;
pub mod folder_store;
pub mod folders;
pub mod library;
pub mod model;
pub mod probe;
pub mod validation;
pub mod versions;

pub use browser::{
    FilePage, ListQuery, MetadataPatch, Sort, TrashEntry, assert_same_site, count_files,
    count_in_folder, files_in_folder, find_file, find_file_any_state, list_files, list_trash,
    purge_files, restore_files, storage_keys, trash_files, trash_summary, trashed_ids, update_file,
};
pub use error::{MediaError, Result};
pub use folder_store::{
    count_files_in_folder, delete_empty_folder, find_folder, insert_folder, list_folders,
    move_folder, root_folder,
};
pub use folders::{
    Folder, FolderMove, MAX_FOLDER_NAME_LENGTH, MAX_FOLDER_PATH_LENGTH, NewFolder,
    ROOT_FOLDER_NAME, child_path, sanitize_folder_name, subtree_pattern, subtree_predicate,
    validate_folder_name,
};
pub use library::{delete_media, find_media, insert_media, list_media};
pub use model::{MAX_FILENAME_LENGTH, MAX_UPLOAD_BYTES, Media, MediaFile, NewMedia};
pub use probe::{HEADER_BYTES, MediaProbe, probe};
pub use validation::{
    INLINE_CONTENT_TYPES, ServePlan, normalize_content_type, object_key, sanitize_filename,
    serve_plan,
};
pub use versions::{
    MAX_NOTE_LENGTH, MediaVersion, NewVersion, VersionTransaction, all_storage_keys,
    append_version, begin_version, commit_version, count_versions, ensure_version_one,
    fill_dimensions, find_version, list_versions, next_version, normalize_note, probe_of,
};
