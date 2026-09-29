//! Omnion content.
//!
//! The content store of the platform (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7):
//! pages, their append-only revision history and the translation rows of a revision. The
//! content type builder, block editor and translation memory build on these primitives; v0
//! keeps the model small but complete — a page has a slug, a lifecycle and a history that can
//! be compared and restored without ever rewriting it.

#![forbid(unsafe_code)]

pub mod blockdiff;
pub mod blocks;
pub mod comments;
pub mod error;
pub mod featured;
pub mod forms;
pub mod forms_notify;
pub mod members;
pub mod menus;
pub mod model;
pub mod newsletter;
pub mod page_comments;
pub mod pages;
pub mod patterns;
pub mod publishing;
pub mod sanitize;
pub mod seo;
pub mod templates;
pub mod translations;
pub mod validation;

pub use blockdiff::{BlockChange, BlockDiff, BlockDiffEntry, PropChange, diff_blocks, headline};
pub use blocks::{
    Block, BlockDefinition, BlockIssue, BlockValidationReport, CATEGORIES, HIDE_ON_VALUES,
    MAX_BLOCKS, MAX_DEPTH, PropDef, PropDefault, PropKind, REGISTRY, REGISTRY_VERSION, ReadOn,
    TreeSanitizeReport, Viewport, block_hide_on, block_is_hidden, blocks_to_value, default_props,
    definition, filter_for_viewport, is_known, meta_is_meaningful, parse_blocks, registry_document,
    sanitize_tree, validate,
};
pub use comments::{
    COMMENT_COLUMNS, CommentSource, MAX_COMMENT_BODY, NewRevisionComment, RevisionComment,
};
pub use error::{ContentError, Result};
// `MAX_NAME_LENGTH` is deliberately *not* re-exported from here: `patterns` already exports one
// with the same meaning, and two public names for two different bounds is a call site that
// guesses. A menu name and a pattern name are both 120 characters, so the value agrees — but
// the modules keep their own constant so a bound may move without editing the other.
pub use featured::{
    Availability, FeaturedChanges, FeaturedImage, FeaturedMedia, FeaturedStore, MAX_ALT,
    MAX_LEGEND, PickableMedia, is_renderable, object_position, renderable, validate_changes,
};
pub use forms::{
    FIELD_TYPES, FORM_STATUSES, Form, FormChanges, FormField, MAX_ANSWER_LENGTH, MAX_ANSWERS,
    MAX_FIELD_TEXT_LENGTH, MAX_FORM_NAME_LENGTH, MAX_OPTIONS, NewFormField, NewSubmission,
    SUBMISSIONS_STATUSES, SUBMIT_ACTIONS, Submission, SubmissionOutcome, SubmissionQuery,
    answers_summary, bulk_submission_status, consent_text, count_submissions, create_form,
    delete_form, delete_submission, find_form, find_form_by_key, find_submission, list_fields,
    list_forms, list_submissions, matches_pattern, option_labels, read_form, save_fields,
    set_form_status, set_submission_status, spam_score, submissions_in_last_hour,
    submissions_to_csv, submit_public, update_form, validate_answer, validate_submission,
};
pub use menus::{
    Audience, ITEM_TYPES, LOCATIONS, MAX_ITEMS, MAX_LABEL_LENGTH, Menu, MenuChanges, MenuItem,
    MenuSave, NewMenuItem, RenderedItem, RenderedMenu, VISIBILITIES, create_menu, delete_menu,
    find_menu, find_menu_by_key, find_menu_by_location, list_items, list_menus, read_menu,
    rendered_menu, save_menu, update_menu,
};
pub use model::{
    DEFAULT_PAGE_TYPE, NewPage, NewRevisionTranslation, Page, PageChanges, PageRevision,
    REVISION_RESOURCE, Translation,
};
pub use pages::{
    create_page, current_draft, delete_page, find_page, find_page_by_slug, find_revision,
    latest_revision, list_pages, list_revisions, publish_page, restore_revision, unpublish_page,
    update_page,
};
pub use patterns::{
    MAX_DESCRIPTION_LENGTH, MAX_NAME_LENGTH, NewPattern, NewTemplate, PageFromTemplate,
    PageTemplate, Pattern, PatternChanges, delete_pattern, delete_template, find_pattern,
    find_pattern_by_key, find_template, find_template_by_key, instance_blocks, list_patterns,
    list_templates, save_pattern, save_template, update_pattern,
};
pub use publishing::{
    ACTIONS, NewSchedule, PublishingEntry, QueueQuery, cancel, claim_due, claim_entry, find_entry,
    find_pending, finish, list_queue, publish_now, reschedule, retry, run_entry, schedule,
};
pub use sanitize::{
    ALLOWED_ATTRIBUTES, ALLOWED_TAGS, ALLOWED_URL_SCHEMES, SanitizeReport, allowed_embed_hosts,
    embed_host_is_allowed, sanitize_html,
};
pub use templates::{
    ABOUT, BLOG_POST, CONTACT, LANDING, PRICING, SYSTEM_TEMPLATES, SystemTemplate,
};
pub use translations::{revision_translations, set_revision_translation};
