//! The shapes every HR list shares.
//!
//! A page type rather than four, because the CRM module wrote the first one and every module
//! since has copied it: a list that returns an array has no way to say "there are more" and a
//! screen that guesses grows an endless scroll that skips rows. The cursor is the last id of the
//! page, which is stable under concurrent writes — an offset is not, and an offset-based page
//! over a directory that is being edited will skip or repeat employees.

use serde::{Deserialize, Serialize};

/// Rows a page holds when the caller names no size.
pub const DEFAULT_PER_PAGE: i64 = 50;

/// Hard cap on a page.
pub const MAX_PER_PAGE: i64 = 200;

/// Longest a search term may be before it is refused.
pub const MAX_SEARCH_LENGTH: usize = 120;

/// One page of a list, with the cursor the next one starts from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    /// The rows of this page.
    pub items: Vec<T>,
    /// The cursor to pass as `?cursor=` for the next page, or `None` at the end.
    pub next_cursor: Option<String>,
    /// How many rows the filter matched, when the count is cheap enough to run.
    pub total_estimate: i64,
}

impl<T> Page<T> {
    /// A page assembled from a query.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<String>, total_estimate: i64) -> Self {
        Self {
            items,
            next_cursor,
            total_estimate,
        }
    }

    /// An empty page — what a filter that matched nothing returns, rather than a list screen
    /// having to tell "no results" from "not loaded yet".
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
            total_estimate: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_page_says_zero_rather_than_nothing() {
        // A list screen distinguishes "no results" from "still loading" by the length of the
        // array plus the total, so the empty page has to carry a total rather than leaving it to
        // the caller.
        let page = Page::<u8>::empty();
        assert!(page.items.is_empty());
        assert_eq!(page.total_estimate, 0);
        assert!(page.next_cursor.is_none());
    }
}
