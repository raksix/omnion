//! Errors of the search layer.

use uuid::Uuid;

/// Anything that can go wrong while indexing or searching.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The database refused or could not run a query.
    #[error("search store: {0}")]
    Store(#[from] sqlx::Error),
    /// A reindex asked for a provider the registry does not know.
    #[error("unknown search provider: {0}")]
    UnknownProvider(String),
    /// The index cursor row is missing; the migration seeds it, so this means a broken schema.
    #[error("the search cursor row is missing (id = 1)")]
    CursorMissing,
    /// An entity the indexer was asked for no longer exists in its source table.
    #[error("entity {entity} of provider {provider} no longer exists")]
    EntityMissing {
        /// Provider key the entity belongs to.
        provider: &'static str,
        /// Entity id as text.
        entity: String,
    },
    /// A payload an event carried could not be used (a missing or malformed id).
    #[error("event {event} (id {event_id}) cannot be applied: {reason}")]
    UnusableEvent {
        /// Event id on the bus.
        event_id: i64,
        /// Event name.
        event: String,
        /// Why it was skipped.
        reason: &'static str,
    },
    /// An account id that had to exist was not found.
    #[error("no such account: {0}")]
    UnknownUser(Uuid),
}

/// Result alias of the search crate.
pub type Result<T> = std::result::Result<T, SearchError>;
