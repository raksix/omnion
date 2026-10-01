//! Storage failures of the deployment centre, as one type (REQ-024).
//!
//! The split that matters is [`StoreError::NotFound`] against everything else. A request for a
//! release the cache has never seen is a `404` — the operator followed a link to something that
//! does not exist, and a message telling them so is the whole answer. A database that refused a
//! write is a `500` and a request id, because the operator did nothing wrong and telling them
//! to "check the version and try again" sends them to fix the thing that is not broken.
//!
//! The third variant, [`StoreError::Feed`], exists for the *update check's* failure rather than
//! the store's: the manifest feed is an external thing that is regularly unreachable, and folding
//! it into `Store` would turn a normal offline condition into an error page instead of the
//! cached-data banner the spec requires.

use std::fmt;

/// What can go wrong while loading or storing deployment state.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The row does not exist. A `404` to the caller.
    #[error("not found")]
    NotFound,
    /// The event bus refused an announcement.
    ///
    /// Its own variant because the *recovery* is specific: the caller has already claimed the
    /// (channel, version) pairs as seen, so this failure has to release the claim or the
    /// announcement is lost for good. Folding it into `Database` would make a `503`-worthy bus
    /// problem read as a storage fault and send an operator to restart something that is fine.
    #[error("the update could not be announced: {0}")]
    Event(String),
    /// The release feed could not be read, or answered with nothing usable.
    ///
    /// Kept separate from the database's own failures because the *response* is different: the
    /// check records this as a failed run and the screen shows the cached-data banner, while a
    /// broken database still bubbles up as a `500`. Folding the two together would send an
    /// operator to look for a database problem that does not exist.
    #[error("release feed unreachable: {0}")]
    Feed(String),
    /// The database refused the statement.
    ///
    /// Declared only with the `store` feature because the `sqlx::Error` it wraps is the
    /// feature's dependency — the variant is unreachable without it, and a variant that cannot
    /// exist is a variant the compiler can no longer check for you.
    #[cfg(feature = "store")]
    #[error("the deployment store failed: {0}")]
    Database(#[from] sqlx::Error),
}

impl StoreError {
    /// The code the API hands to the panel.
    ///
    /// The panel switches on this name, so it is a wire contract: `not_found` sends the
    /// operator back to the list and `feed_unreachable` keeps them on the card with the stale
    /// banner, and one shared string would send both to the same place.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::NotFound => "not_found",
            StoreError::Feed(_) => "feed_unreachable",
            StoreError::Event(_) => "announce_failed",
            #[cfg(feature = "store")]
            StoreError::Database(_) => "internal_error",
        }
    }

    /// Is this a `404`?
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::NotFound)
    }
}

/// A storage error and the release feed's failure, rendered the same way for the log.
impl fmt::Display for FeedContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (last good cache: {})",
            self.reason, self.cached_at
        )
    }
}

/// The pair the stale banner needs: why the feed failed, and how old the data on screen is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedContext<'a> {
    /// What to tell the operator, verbatim from the failure.
    pub reason: &'a str,
    /// When the cache was last refreshed, as the feed's own string.
    pub cached_at: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_failures_carry_three_codes() {
        // Three codes rather than one, because each sends the operator somewhere different. A
        // shared `internal_error` would render the stale banner as an error page and tell an
        // operator to retry a request that will never succeed while the feed is down.
        assert_eq!(StoreError::NotFound.code(), "not_found");
        assert_eq!(
            StoreError::Feed("timeout".into()).code(),
            "feed_unreachable"
        );
    }

    #[test]
    fn only_a_missing_row_is_a_404() {
        assert!(StoreError::NotFound.is_not_found());
        assert!(!StoreError::Feed("timeout".into()).is_not_found());
    }

    #[test]
    fn a_failed_announcement_is_its_own_code() {
        // The recovery differs from every other failure — the claim is released and the next
        // pass re-announces — so it must be distinguishable from a dead feed, which retries
        // nothing and a broken store, which the operator has to look at.
        assert_eq!(
            StoreError::Event("the bus is down".into()).code(),
            "announce_failed"
        );
    }

    #[test]
    fn a_feed_failure_never_reads_as_a_store_failure() {
        // The whole reason the variant exists. Folded into `Database`, the update check would
        // record a network timeout as a broken database and the operator would go looking for a
        // database problem that does not exist.
        let feed = StoreError::Feed("the feed timed out".into());
        let text = feed.to_string();
        assert!(text.contains("release feed unreachable"), "{text}");
        assert!(!text.contains("deployment store failed"), "{text}");
    }

    #[test]
    fn the_banner_context_keeps_both_halves() {
        // A banner that loses the timestamp is the empty-field bug the spec names: the reader
        // cannot tell whether the data is a minute or a month old, so they cannot decide
        // whether to trust it.
        let context = FeedContext {
            reason: "connection refused",
            cached_at: "2026-10-01 07:31",
        };
        let text = context.to_string();
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("2026-10-01 07:31"), "{text}");
    }
}
