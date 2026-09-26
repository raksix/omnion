//! Errors the media library reports.

use thiserror::Error;

/// Result alias of the media library.
pub type Result<T> = std::result::Result<T, MediaError>;

/// What went wrong in the media library.
#[derive(Debug, Error)]
pub enum MediaError {
    /// The database refused or could not answer.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// No media row with that id.
    #[error("no media row with this id")]
    NotFound,
    /// The upload is larger than the library accepts.
    #[error("the file is larger than the {limit} byte limit")]
    SizeTooLarge {
        /// Largest accepted size in bytes.
        limit: u64,
    },
    /// The upload carried no bytes.
    #[error("the uploaded file is empty")]
    EmptyFile,
    /// The file name cannot be used.
    #[error("{0}")]
    InvalidFilename(String),
    /// The content type cannot be used.
    #[error("{0}")]
    InvalidContentType(String),
    /// The storage key cannot be used.
    #[error("{0}")]
    InvalidKey(String),
    /// The row is already there (two uploads raced for the same object key).
    #[error("this storage key is already in the media library")]
    KeyTaken,
}
