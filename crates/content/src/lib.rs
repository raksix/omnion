//! Omnion content.
//!
//! The content store of the platform (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7):
//! pages, their append-only revision history and the translation rows of a revision. The
//! content type builder, block editor and translation memory build on these primitives; v0
//! keeps the model small but complete — a page has a slug, a lifecycle and a history that can
//! be compared and restored without ever rewriting it.

#![forbid(unsafe_code)]

pub mod comments;
pub mod error;
pub mod model;
pub mod pages;
pub mod translations;
pub mod validation;

pub use comments::{
    COMMENT_COLUMNS, CommentSource, MAX_COMMENT_BODY, NewRevisionComment, RevisionComment,
};
pub use error::{ContentError, Result};
pub use model::{
    DEFAULT_PAGE_TYPE, NewPage, NewRevisionTranslation, Page, PageChanges, PageRevision,
    REVISION_RESOURCE, Translation,
};
pub use pages::{
    create_page, current_draft, delete_page, find_page, find_page_by_slug, find_revision,
    latest_revision, list_pages, list_revisions, publish_page, restore_revision, update_page,
};
pub use translations::{revision_translations, set_revision_translation};
