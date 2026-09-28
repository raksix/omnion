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
    /// No folder row with that id.
    #[error("no media folder with this id")]
    FolderNotFound,
    /// The folder name cannot be used.
    #[error("{0}")]
    InvalidFolderName(String),
    /// A sibling folder of that name already exists.
    #[error("a folder with this name already exists here")]
    FolderNameTaken,
    /// The move would put a folder inside its own subtree.
    #[error("a folder cannot be moved inside itself (target path `{path}`)")]
    FolderCycle {
        /// The path the move tried to create.
        path: String,
    },
    /// The folder still holds something.
    #[error("the folder still holds {count} {what}")]
    FolderNotEmpty {
        /// What is in the way (`folders` or `files`).
        what: &'static str,
        /// How many.
        count: i64,
    },
    /// The library root is structural: it cannot be renamed, moved or removed.
    #[error("the library root cannot be renamed, moved or deleted")]
    RootFolderProtected,
    /// A file cannot be moved into a folder of another site.
    #[error("the folder belongs to another site")]
    FolderSiteMismatch,
    /// The file is in the trash and the action needs it live (or the other way round).
    #[error("the file is in the trash")]
    FileTrashed,
    /// A transformation preset is not usable as written.
    #[error("{0}")]
    InvalidPreset(String),
    /// No preset with that name (or that id) on this site.
    #[error("no transformation preset named `{name}` on this site")]
    PresetNotFound {
        /// The name the caller asked for.
        name: String,
    },
    /// A sibling preset of that name already exists.
    #[error("a preset named `{name}` already exists on this site")]
    PresetNameTaken {
        /// The name that is taken.
        name: String,
    },
    /// The bytes could not be decoded as an image.
    #[error("the file could not be decoded as an image: {reason}")]
    Undecodable {
        /// What the decoder said.
        reason: String,
    },
    /// The bytes decode, but not as a raster image (an SVG, a PDF, a video).
    #[error("`{content_type}` is not a raster image, so it cannot be transformed")]
    NotAnImage {
        /// The content type the file carries.
        content_type: String,
    },
    /// The transformation itself failed after a successful decode.
    #[error("the image could not be transformed: {reason}")]
    TransformFailed {
        /// What the encoder said.
        reason: String,
    },
}
