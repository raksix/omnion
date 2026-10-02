//! The one error this crate raises.

use thiserror::Error;

/// What can go wrong while a credential is issued, presented or retired.
#[derive(Debug, Error)]
pub enum DeveloperError {
    /// The request named something the platform cannot resolve: an unknown scope, an
    /// environment outside the closed list, a name the screen cannot render.
    #[error("{0}")]
    Invalid(String),

    /// The caller asked for something an existing row already owns — a live key's name in the
    /// same organization and environment. Distinct from [`DeveloperError::Invalid`] because
    /// the API answers `409` for this and `400` for the other, and the panel shows a different
    /// message for each.
    #[error("{0}")]
    Conflict(String),

    /// No key, app or log row carries that id **for this organization**. Answered `404`, and
    /// deliberately not distinguished from "exists in another tenant" — the route filters by
    /// organization before it reports, so a probe cannot learn that an id is real elsewhere.
    #[error("{0}")]
    NotFound(String),

    /// The database refused or lost the connection. Never carries a key value: the queries in
    /// this crate never bind one, so a bind failure cannot echo one into a log line.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

/// The crate's result.
pub type Result<T> = std::result::Result<T, DeveloperError>;
