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
    /// A content API token name is already taken in this organization (REQ-019).
    #[error("a token named \"{0}\" already exists")]
    TokenNameTaken(String),
    /// A content read query parameter is not one this surface accepts (REQ-019 slice 2).
    #[error("invalid query: {0}")]
    InvalidQuery(String),
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
    /// A page cannot feature this file: it is another site's, already gone, in the trash, or not
    /// an image at all.
    ///
    /// One variant for the four cases on purpose. The caller — a panel operator choosing a file —
    /// has exactly one thing to do about any of them (pick another one), and four error codes
    /// would tell them four different things about a library that is not the problem. The
    /// *message* still names which of the four it was, because the four have different fixes.
    #[error("{0}")]
    FeaturedMediaUnavailable(String),
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
    /// No live theme carries this key for this organization (REQ-062 slice 1).
    #[error("no installed theme has the key '{0}'")]
    ThemeNotFound(String),
    /// The site has no activation row, so there is nothing to roll back to (REQ-062 slice 1).
    #[error("this site has no theme activation to roll back from")]
    RollbackUnavailable,
    /// No settings revision carries this number for this site (REQ-062 slice 2).
    ///
    /// The number is in the message rather than a bare "not found" because the caller passed
    /// one and needs to know which one was wrong: the history screen asks for revision 4 and a
    /// 404 that only says "no such revision" sends the operator looking for a missing row
    /// instead of a stale link.
    #[error("this site has no settings revision numbered {0}")]
    ThemeSettingsRevisionNotFound(i32),
    /// Publish was asked for with no draft saved yet.
    ///
    /// A separate variant from a generic bad request, because it is the one state where the
    /// panel's Publish button is simply the wrong button: the answer names the action that
    /// would work instead.
    #[error("this site has no saved draft to publish — save one first")]
    ThemeSettingsNothingToPublish,
    /// The draft is older than what is live, so publishing would move the site backwards.
    ///
    /// Both numbers are carried so the screen can say which is which rather than refusing
    /// without telling the operator which of their two tabs is stale.
    #[error(
        "the draft is revision {draft_no} but revision {published_no} is the one that is live; \
         reload before publishing"
    )]
    ThemeSettingsDraftStale {
        /// The draft's number.
        draft_no: i32,
        /// The live revision's number.
        published_no: i32,
    },
    /// The draft's tokens fail the WCAG AA contrast check and the caller did not acknowledge.
    ///
    /// Kept distinct from a validation error because the payload was *legal* — a colour is a
    /// colour. It is a warning the product makes the operator look at, which is why publishing
    /// it needs an explicit acknowledgement rather than a correction.
    #[error("the settings fail the contrast check: {0}")]
    ThemeSettingsContrastRefused(String),
    // --- Theme layouts and packages (REQ-062 slice 3) ---------------------------------------
    // Each of these is a sentence an operator acts on, and each is a *different* next step,
    // which is why they are separate variants rather than one `ThemeError(String)`: a screen
    // that receives "unknown slot 'foot'" needs to say "that is not a slot — here are the
    // eight we render", while a screen that receives "the tree cannot be stored" needs to
    // point the author at the block that caused it.
    /// A slot name that is not one the platform renders.
    #[error("'{0}' is not a slot this platform renders")]
    ThemeUnknownSlot(String),
    /// A slot name longer than the column allows.
    #[error("the slot name must stay under {1} characters ('{0}')")]
    ThemeSlotTooLong(String, usize),
    /// A slot tree carrying more top-level blocks than a slot may hold.
    #[error("a slot may hold at most {0} blocks — this one is larger")]
    ThemeSlotTooManyBlocks(usize),
    /// A slot tree the store cannot hold, with the validator's own reason.
    #[error("this slot cannot be stored: {0}")]
    ThemeSlotInvalid(String),
    /// Reset was asked for a slot the theme never shipped a default for.
    #[error("this theme ships no default layout for '{0}'")]
    ThemeSlotNoDefault(String),
    /// A package with errors in it, with the count so the screen does not recount the list.
    #[error("the package has {0} error(s) and was not installed")]
    ThemePackageInvalid(usize),
    /// A bundled theme was asked to be deleted; it is a file, not a row.
    #[error("'{0}' is a bundled theme and cannot be removed")]
    ThemeBundledCannotBeRemoved(String),
    /// An uploaded theme is still the active one, or a site still holds its layouts.
    #[error("'{0}' is still in use and cannot be removed")]
    ThemeInUse(String),
    /// An uploaded package was larger than the platform accepts.
    #[error("the package is larger than the {0} byte limit")]
    ThemePackageTooLarge(usize),
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
    /// Newsletter and membership validation (REQ-064 slice 4b/4c). One variant rather than one
    /// per field, because every caller renders the same thing from it — a message a person can
    /// act on — and a variant per field is a variant nobody ever adds an arm for.
    #[error("{0}")]
    InvalidNewsletter(String),
    /// No list carries this identifier, or it belongs to another site.
    #[error("no such newsletter list")]
    NewsletterListNotFound,
    /// No subscriber carries this identifier, or it belongs to another site.
    #[error("no such subscriber")]
    SubscriberNotFound,
    /// The address is already confirmed on this list. Deliberately NOT a 404: the caller is
    /// told the truth because this is a panel write, and a panel that cannot say "already
    /// subscribed" makes an owner guess whether their button worked.
    #[error("{0} is already subscribed to this list")]
    SubscriberAlreadyConfirmed(String),
    /// A confirmation or unsubscribe token that matches no row, or is not a token at all.
    ///
    /// The three refusals a token can earn — unknown, expired, already used — are ONE error to
    /// the caller and THREE reasons in [`crate::newsletter::TokenOutcome`]. A visitor who
    /// clicked a link from a forwarded mail must not be able to learn whether an address is
    /// on a list, so the error says nothing and the outcome says everything.
    #[error("that link is not valid")]
    InvalidToken,
    /// No issue carries this archive slug, or it belongs to another site.
    #[error("no such newsletter issue")]
    IssueNotFound,

    // -----------------------------------------------------------------------------------------
    // Memberships (REQ-064, slice 4c)
    // -----------------------------------------------------------------------------------------

    /// A member field is not usable (blank, too long, an unknown role, an unknown state).
    #[error("invalid member: {0}")]
    InvalidMember(String),
    /// No member carries this identifier, or it belongs to another site.
    #[error("no such member")]
    MemberNotFound,
    /// The address already has an account on this site.
    ///
    /// Deliberately a 409 and not a 404, on a *public* route. The tension is real and it is
    /// resolved in favour of the visitor here rather than in favour of concealment: this error
    /// is only ever returned by the **panel**'s "add a member" action, never by the public
    /// signup. The public signup answers 202 with the same body whether the address was new,
    /// already known or just re-invited, so it discloses nothing; the panel is a signed-in
    /// operator who needs to be told the add did not work and why.
    #[error("{0} already has an account on this site")]
    MemberEmailTaken(String),
    /// A sign-in was refused.
    ///
    /// One error for four refusals — unknown address, wrong password, not yet verified, and
    /// blocked — because the two that matter to an attacker are indistinguishable to a
    /// visitor, and a sign-in form that answers "this address is not verified yet" hands out
    /// the membership of every address on the site.
    #[error("that e-mail and password do not match an account here")]
    InvalidCredentials,
    /// The member's own site has not verification on, so no link was ever sent.
    ///
    /// The panel's "send verification" button reaching this means the site's own policy
    /// contradicts the button, which is a configuration mistake the operator has to see.
    #[error("this site does not require verification — turn it on in membership settings first")]
    VerificationNotRequired,
    /// The password is too short to be stored.
    #[error("password: {0}")]
    WeakPassword(String),
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
            Self::FeaturedMediaUnavailable(_) => "featured_media_unavailable",
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
            Self::InvalidNewsletter(_) => "invalid_newsletter",
            Self::NewsletterListNotFound => "newsletter_list_not_found",
            Self::SubscriberNotFound => "subscriber_not_found",
            Self::SubscriberAlreadyConfirmed(_) => "subscriber_already_confirmed",
            Self::InvalidToken => "invalid_token",
            Self::IssueNotFound => "newsletter_issue_not_found",
            Self::InvalidMember(_) => "invalid_member",
            Self::MemberNotFound => "member_not_found",
            Self::MemberEmailTaken(_) => "member_email_taken",
            Self::InvalidCredentials => "invalid_credentials",
            Self::VerificationNotRequired => "verification_not_required",
            Self::WeakPassword(_) => "weak_password",
            Self::SiteNotFound => "site_not_found",
            Self::ThemeNotFound(_) => "theme_not_found",
            Self::RollbackUnavailable => "theme_rollback_unavailable",
            Self::ThemeSettingsRevisionNotFound(_) => "theme_settings_revision_not_found",
            Self::ThemeSettingsNothingToPublish => "theme_settings_nothing_to_publish",
            Self::ThemeSettingsDraftStale { .. } => "theme_settings_draft_stale",
            Self::ThemeSettingsContrastRefused(_) => "theme_settings_contrast_required",
            // Slot and package failures (REQ-062 slice 3). Each one is its own code because the
            // upload screen branches on them: an unknown slot and an unknown block type are two
            // different sentences about two different parts of the same file, and a client that
            // only had "theme_package_invalid" could not point at the line that was wrong.
            Self::ThemeUnknownSlot(_) => "theme_unknown_slot",
            Self::ThemeSlotTooLong(_, _) => "theme_slot_too_long",
            Self::ThemeSlotTooManyBlocks(_) => "theme_slot_too_many_blocks",
            Self::ThemeSlotInvalid(_) => "theme_slot_invalid",
            Self::ThemeSlotNoDefault(_) => "theme_slot_no_default",
            Self::ThemePackageInvalid(_) => "theme_package_invalid",
            Self::ThemeBundledCannotBeRemoved(_) => "theme_bundled_cannot_be_removed",
            Self::ThemeInUse(_) => "theme_in_use",
            Self::ThemePackageTooLarge(_) => "theme_package_too_large",
            // The content API token store (REQ-019 slice 1). Its own code rather than a generic
            // "invalid", because the panel's create dialog branches on it: a name that is taken
            // is a field the person typed, and a name that is malformed is the same field with a
            // different mistake. Both answer 409/400 with `name_taken` / `invalid_parameter`.
            Self::TokenNameTaken(_) => "name_taken",
            // A read query that names a parameter the surface does not accept. Its own code so
            // the API layer can answer `400 invalid_parameter` with `details.field` pointing at
            // the offending input — the Explorer highlights that field rather than printing a
            // sentence the caller has to parse.
            Self::InvalidQuery(_) => "invalid_parameter",
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
