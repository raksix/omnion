//! Error type of the content store.
//!
//! Like the rest of the crates, this one never decides HTTP status codes: it returns
//! [`ContentError`] and the API layer maps it onto the HTTP surface (`apps/api/src/error.rs`).

/// Errors returned by the content store.
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// A slug is not usable (shape or case).
    #[error("invalid slug: {0}")]
    InvalidSlug(String),
    /// A title is not usable (blank or too long).
    #[error("invalid title: {0}")]
    InvalidTitle(String),
    /// A body is not usable (too large).
    #[error("invalid body: {0}")]
    InvalidBody(String),
    /// A comment is not usable (blank, too long, or an author that does not match its source).
    #[error("invalid comment: {0}")]
    InvalidComment(String),
    /// A summary is not usable (too long).
    #[error("invalid summary: {0}")]
    InvalidSummary(String),
    /// A page type key is not usable.
    #[error("invalid page type: {0}")]
    InvalidPageType(String),
    /// A lifecycle state is not one of the documented values.
    #[error("invalid status: {0}")]
    InvalidStatus(String),
    /// A language tag is not usable (shape or case).
    #[error("invalid language: {0}")]
    InvalidLanguage(String),
    /// A translation field name is not usable.
    #[error("invalid translation field: {0}")]
    InvalidField(String),
    /// A translation value is not usable (too large).
    #[error("invalid translation value: {0}")]
    InvalidValue(String),
    /// No page carries this identifier.
    #[error("no such page")]
    PageNotFound,
    /// The revision does not belong to the page it was requested through.
    #[error("no such revision on this page")]
    RevisionNotFound,
    /// The site already carries a page with this slug.
    #[error("this site already has a page with this slug")]
    SlugTaken,
    /// Publication was requested but the page holds no draft revision.
    #[error("this page has no draft revision to publish")]
    NoDraftRevision,
}

/// Result alias used across the content crate.
pub type Result<T, E = ContentError> = std::result::Result<T, E>;

impl ContentError {
    /// Stable machine-readable code, for tests and for callers that branch on the failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "content_store_error",
            Self::InvalidSlug(_) => "invalid_slug",
            Self::InvalidTitle(_) => "invalid_title",
            Self::InvalidBody(_) => "invalid_body",
            Self::InvalidComment(_) => "invalid_comment",
            Self::InvalidSummary(_) => "invalid_summary",
            Self::InvalidPageType(_) => "invalid_page_type",
            Self::InvalidStatus(_) => "invalid_status",
            Self::InvalidLanguage(_) => "invalid_language",
            Self::InvalidField(_) => "invalid_field",
            Self::InvalidValue(_) => "invalid_value",
            Self::PageNotFound => "page_not_found",
            Self::RevisionNotFound => "revision_not_found",
            Self::SlugTaken => "slug_taken",
            Self::NoDraftRevision => "no_draft_revision",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_without_a_draft_explains_itself() {
        assert_eq!(
            ContentError::NoDraftRevision.to_string(),
            "this page has no draft revision to publish"
        );
    }

    #[test]
    fn a_taken_slug_is_explicit() {
        assert_eq!(
            ContentError::SlugTaken.to_string(),
            "this site already has a page with this slug"
        );
    }

    #[test]
    fn a_missing_revision_names_the_page() {
        assert_eq!(
            ContentError::RevisionNotFound.to_string(),
            "no such revision on this page"
        );
    }
}
