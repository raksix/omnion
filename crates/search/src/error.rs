//! Errors of the search layer.

/// Anything that can go wrong while running a search.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The database refused or could not run a query.
    #[error("search store: {0}")]
    Store(#[from] sqlx::Error),
}

/// Result alias of the search crate.
pub type Result<T> = std::result::Result<T, SearchError>;
