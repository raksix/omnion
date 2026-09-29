//! Error type of the content store.
//!
//! Like the rest of the crates, this one never decides HTTP status codes: it returns
//! [`ContentError`] and the API layer maps it onto the HTTP surface (`apps/api/src/error.rs`).

use uuid::Uuid;

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
    /// A block payload is not a block tree this platform can read (REQ-063).
    #[error("invalid blocks: {0}")]
    InvalidBlock(String),
    /// A page type key is not usable.
    #[error("invalid page type: {0}")]
    InvalidPageType(String),
    /// A pattern or template key is not usable (REQ-063 slice 3).
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// A pattern or template name is blank or too long (REQ-063 slice 3).
    #[error("invalid name: {0}")]
    InvalidName(String),
    /// A free-text field is too long (REQ-063 slice 3).
    #[error("invalid text: {0}")]
    InvalidText(String),
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
    /// No pattern carries this identifier (REQ-063 slice 3).
    #[error("no such pattern")]
    PatternNotFound,
    /// The organization already has a pattern with this key.
    #[error("this organization already has a pattern with this key")]
    PatternKeyTaken(String),
    /// No page template carries this identifier (REQ-063 slice 3).
    #[error("no such page template")]
    TemplateNotFound,
    /// The organization already has a page template with this key.
    #[error("this organization already has a page template with this key")]
    TemplateKeyTaken(String),
    /// A template the platform ships with cannot be deleted.
    #[error("this template ships with the platform and cannot be deleted")]
    TemplateIsSystem,
    /// Publication was requested but the page holds no draft revision.
    #[error("this page has no draft revision to publish")]
    NoDraftRevision,
    /// A theme location is not one the renderer reads (REQ-064 slice 1).
    #[error("invalid location: {0}")]
    InvalidLocation(String),
    /// An item's visibility rule is not one of the documented values (REQ-064 slice 1).
    #[error("invalid visibility: {0}")]
    InvalidVisibility(String),
    /// A menu item is unusable — no label, no target, a page link with no page, or a tree that
    /// names a parent which is not part of the submission.
    #[error("invalid menu item: {0}")]
    InvalidMenuItem(String),
    /// A menu branch nests deeper than the renderer draws.
    #[error("menu nesting is too deep: {0}")]
    TooDeep(String),
    /// No menu carries this identifier.
    #[error("no such menu")]
    MenuNotFound,
    /// The site already has a menu with this key.
    #[error("this site already has a menu with this key")]
    MenuKeyTaken(String),
    /// A location is already held by another menu. The holder is named so the editor can be
    /// pointed at the menu to move, rather than guessing which of the site's menus it was.
    ///
    /// `holder` is the menu's UUID and `holder_key` is the key an editor actually recognises —
    /// a UUID in a message is a name nobody can paste into the menu list, so the two travel
    /// together and the API's message quotes the key.
    #[error(
        "the {location} location is already held by the {holder_key:?} menu; move it there first"
    )]
    MenuLocationTaken {
        /// The contested location.
        location: String,
        /// The menu that holds it today.
        holder: Uuid,
        /// That menu's key — what a reader can act on.
        holder_key: String,
    },
    /// A publishing queue entry does not carry this identifier.
    #[error("no such publishing entry")]
    PublishingEntryNotFound,
    /// A scheduling action is not `publish` or `unpublish`.
    #[error("invalid publishing action: {0}")]
    InvalidPublishAction(String),
    /// A schedule is not a usable instant (before now, or unparseable as a timestamp).
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    /// A form definition is unusable (no fields, a duplicate or malformed key, a choice field
    /// with no options, a submit behaviour with nothing to do).
    #[error("invalid form: {0}")]
    InvalidFormField(String),
    /// No form carries this identifier (REQ-064 slice 2).
    #[error("no such form")]
    FormNotFound,
    /// The site already has a form with this key. It is the form's public address, so two forms
    /// cannot share it — a visitor has no way to choose between them.
    #[error("this site already has a form with this key")]
    FormKeyTaken(String),
    /// No submission carries this identifier, or it belongs to another form.
    #[error("no such submission in this form")]
    SubmissionNotFound,
    /// A redirect rule is unusable (a path that is not site-relative, a code or pattern the
    /// manager does not offer, or a rule that would send a visitor in a circle).
    #[error("invalid redirect: {0}")]
    InvalidRedirect(String),
    /// Two rules would send a visitor back and forth. Raised before the rule is written, with
    /// the path the cycle closes on, because the alternative is discovering it from a browser.
    #[error("redirect loop: {0}")]
    RedirectLoop(String),
    /// No redirect rule carries this identifier, or it belongs to another site.
    #[error("no such redirect rule")]
    RedirectNotFound,
    /// A page's SEO payload is unusable (a relative canonical, a schema type this generator
    /// does not build, a `robots` list that says both `index` and `noindex`).
    #[error("invalid SEO settings: {0}")]
    InvalidSeo(String),
    /// A broken-link row does not carry this identifier, or it belongs to another site.
    #[error("no such broken link")]
    BrokenLinkNotFound,
    /// The site these SEO rows would belong to does not exist. Separate from a generic 404 on
    /// purpose: a redirect for a site that was deleted is a caller bug, not a missing resource.
    #[error("no such site")]
    SiteNotFound,
    /// No comment carries this identifier, or it belongs to another site (REQ-064 slice 4a).
    #[error("no such comment")]
    CommentNotFound,
    /// The comment is already in the state the request asked for. A separate variant from
    /// `CommentNotFound` because the two mean opposite things: one is "there is no such
    /// comment", the other is "that comment is already approved" — and a bulk action that hit
    /// this is a no-op, not a failure.
    #[error("{0}")]
    CommentAlreadyInState(String),
    /// A reply named a reply. The platform's two-level rule, told apart from a missing comment
    /// because the two mean opposite things for the caller: one is "I have the wrong id", the
    /// other is "this platform does not do that, and no retry will change it".
    #[error("a reply cannot answer another reply")]
    CommentThreadTooDeep,
    /// A banned address tried to comment. The message names the reason a moderator recorded,
    /// because "you are banned" with no reason is the one answer a person will argue with.
    #[error("comment refused: {0}")]
    CommentBanned(String),
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
            Self::InvalidBlock(_) => "invalid_block",
            Self::InvalidPageType(_) => "invalid_page_type",
            Self::InvalidKey(_) => "invalid_key",
            Self::InvalidName(_) => "invalid_name",
            Self::InvalidText(_) => "invalid_text",
            Self::InvalidStatus(_) => "invalid_status",
            Self::InvalidLanguage(_) => "invalid_language",
            Self::InvalidField(_) => "invalid_field",
            Self::InvalidValue(_) => "invalid_value",
            Self::PageNotFound => "page_not_found",
            Self::RevisionNotFound => "revision_not_found",
            Self::SlugTaken => "slug_taken",
            Self::PatternNotFound => "pattern_not_found",
            Self::PatternKeyTaken(_) => "pattern_key_taken",
            Self::TemplateNotFound => "template_not_found",
            Self::TemplateKeyTaken(_) => "template_key_taken",
            Self::TemplateIsSystem => "template_is_system",
            Self::NoDraftRevision => "no_draft_revision",
            Self::InvalidLocation(_) => "invalid_location",
            Self::InvalidVisibility(_) => "invalid_visibility",
            Self::InvalidMenuItem(_) => "invalid_menu_item",
            Self::TooDeep(_) => "menu_too_deep",
            Self::MenuNotFound => "menu_not_found",
            Self::MenuKeyTaken(_) => "menu_key_taken",
            Self::MenuLocationTaken { .. } => "menu_location_taken",
            Self::PublishingEntryNotFound => "publishing_entry_not_found",
            Self::InvalidPublishAction(_) => "invalid_publish_action",
            Self::InvalidSchedule(_) => "invalid_schedule",
            Self::InvalidFormField(_) => "invalid_form",
            Self::FormNotFound => "form_not_found",
            Self::FormKeyTaken(_) => "form_key_taken",
            Self::SubmissionNotFound => "submission_not_found",
            Self::InvalidRedirect(_) => "invalid_redirect",
            Self::RedirectLoop(_) => "redirect_loop",
            Self::RedirectNotFound => "redirect_not_found",
            Self::InvalidSeo(_) => "invalid_seo",
            Self::BrokenLinkNotFound => "broken_link_not_found",
            Self::CommentNotFound => "comment_not_found",
            Self::CommentAlreadyInState(_) => "comment_already_in_state",
            Self::CommentBanned(_) => "comment_banned",
            Self::CommentThreadTooDeep => "comment_thread_too_deep",
            Self::SiteNotFound => "site_not_found",
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
