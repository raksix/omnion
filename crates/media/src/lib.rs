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

pub mod error;
pub mod library;
pub mod model;
pub mod validation;

pub use error::{MediaError, Result};
pub use library::{delete_media, find_media, insert_media, list_media};
pub use model::{MAX_FILENAME_LENGTH, MAX_UPLOAD_BYTES, Media, NewMedia};
pub use validation::{
    INLINE_CONTENT_TYPES, ServePlan, normalize_content_type, object_key, sanitize_filename,
    serve_plan,
};
