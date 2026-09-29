//! What went wrong in the backup centre.

use thiserror::Error;

/// Result alias of the backup centre.
pub type Result<T> = std::result::Result<T, BackupError>;

/// What went wrong in the backup centre.
#[derive(Debug, Error)]
pub enum BackupError {
    /// The database refused or could not answer.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// No backup row with that id.
    #[error("no backup with this id")]
    NotFound,
    /// No schedule row with that id.
    #[error("no backup schedule with this id")]
    ScheduleNotFound,
    /// A field the caller sent is not acceptable.
    ///
    /// The message names the field, because the screen renders it next to the input and a
    /// message that says only "invalid backup" sends the operator looking at the wrong form.
    #[error("{0}")]
    Invalid(String),
    /// A run is asked for something it cannot do — an unknown part, an unwritable
    /// destination, a manifest whose checksums do not match its artifacts.
    #[error("{0}")]
    Rejected(String),
    /// A run finished with at least one part that failed.
    ///
    /// This is a distinct variant from [`BackupError::Rejected`] on purpose: a partial run
    /// is not a refusal, it is a result, and the screen says "four parts succeeded, one
    /// did not" rather than "the backup failed".
    #[error("backup finished with {failed} of {total} parts failed: {message}")]
    Partial {
        /// How many parts failed.
        failed: i32,
        /// How many parts the run had.
        total: i32,
        /// The first failure message, so the error line is not empty.
        message: String,
    },
}
