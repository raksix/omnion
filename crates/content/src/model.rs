//! Rows and request shapes of the content store.

use time::OffsetDateTime;
use uuid::Uuid;

/// The page type a page gets when the caller does not pick one (docs/01-VISION.md §7 — the
/// content type builder extends this set later).
pub const DEFAULT_PAGE_TYPE: &str = "page";

/// Resource type translation rows carry for page revisions.
pub const REVISION_RESOURCE: &str = "page_revision";

/// A page: one addressable piece of content of one site.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Page {
    /// Primary key.
    pub id: Uuid,
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Address of the page inside its site, unique per site.
    pub slug: String,
    /// Content type key, `page` by default.
    pub page_type: String,
    /// `draft` (never published), `published` or `archived`.
    pub status: String,
    /// Revision visitors currently see; `None` until the first publish.
    pub published_revision_id: Option<Uuid>,
    /// Account that created the page, when a person did.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl Page {
    /// `true` when the page has a published revision.
    #[must_use]
    pub fn is_published(&self) -> bool {
        self.status == "published"
    }
}

/// One content version of one page. Rows are append-only: edits write the next revision and
/// publishing/restoring changes a revision's state, never its content.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PageRevision {
    /// Primary key.
    pub id: Uuid,
    /// The page this revision belongs to.
    pub page_id: Uuid,
    /// Monotonic revision number inside the page, starting at 1.
    pub revision_no: i32,
    /// `draft` (the working head), `published` (what visitors see) or `archived`.
    pub state: String,
    /// Revision title.
    pub title: String,
    /// Revision body.
    pub body: String,
    /// Short summary, when the author wrote one.
    pub summary: Option<String>,
    /// Revision this one was copied from by a restore, when it was.
    pub restored_from_id: Option<Uuid>,
    /// Account that authored the revision, when a person did.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// When the revision was published, when it ever was.
    pub published_at: Option<OffsetDateTime>,
}

impl PageRevision {
    /// `true` when this revision is the working draft.
    #[must_use]
    pub fn is_draft(&self) -> bool {
        self.state == "draft"
    }

    /// `true` when this revision is the one visitors see.
    #[must_use]
    pub fn is_published(&self) -> bool {
        self.state == "published"
    }
}

/// A page to create together with its first revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPage {
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Desired slug (normalized before the insert).
    pub slug: String,
    /// Content type key; defaults to [`DEFAULT_PAGE_TYPE`].
    pub page_type: Option<String>,
    /// Title of the first revision.
    pub title: String,
    /// Body of the first revision; empty when omitted.
    pub body: Option<String>,
    /// Summary of the first revision; none when omitted.
    pub summary: Option<String>,
    /// Author of the page and its first revision.
    pub created_by: Option<Uuid>,
}

/// Fields [`crate::pages::update_page`] may change. `None` leaves a field untouched; an empty
/// summary clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageChanges {
    /// New slug (a rename; no revision is written for it).
    pub slug: Option<String>,
    /// New title (appends a revision).
    pub title: Option<String>,
    /// New body (appends a revision).
    pub body: Option<String>,
    /// New summary; `Some("")` clears it (appends a revision).
    pub summary: Option<String>,
}

impl PageChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slug.is_none() && !self.touches_content()
    }

    /// `true` when the change set needs a new revision.
    #[must_use]
    pub fn touches_content(&self) -> bool {
        self.title.is_some() || self.body.is_some() || self.summary.is_some()
    }
}

/// A translation row to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRevisionTranslation {
    /// Revision the translated value belongs to.
    pub revision_id: Uuid,
    /// Language tag (`tr`, `en`, `pt-br`).
    pub language: String,
    /// Field name (`title`, `body`, `summary`).
    pub field: String,
    /// Translated value.
    pub value: String,
    /// Account that wrote the value, when a person did.
    pub created_by: Option<Uuid>,
}

/// A stored translation row: one value of one field of one resource in one language.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Translation {
    /// Primary key.
    pub id: Uuid,
    /// Organization the resource belongs to (derived from page → site).
    pub organization_id: Uuid,
    /// Kind of resource (`page_revision`).
    pub resource_type: String,
    /// Identifier of the resource.
    pub resource_id: Uuid,
    /// Language tag, lowercase.
    pub language: String,
    /// Field name.
    pub field: String,
    /// Translated value.
    pub value: String,
    /// Account that last wrote the value.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_only_change_is_not_a_content_change() {
        let rename = PageChanges {
            slug: Some("about".to_owned()),
            ..PageChanges::default()
        };
        assert!(!rename.is_empty());
        assert!(!rename.touches_content(), "a rename writes no revision");

        let edit = PageChanges {
            title: Some("About us".to_owned()),
            ..PageChanges::default()
        };
        assert!(edit.touches_content());

        assert!(PageChanges::default().is_empty());
    }

    #[test]
    fn clearing_a_summary_still_writes_a_revision() {
        let clear = PageChanges {
            summary: Some(String::new()),
            ..PageChanges::default()
        };
        assert!(clear.touches_content(), "an empty summary clears the field");
    }
}
