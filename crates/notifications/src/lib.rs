//! Omnion notifications — the inbox a person actually looks at (docs/requests/REQ-021).
//!
//! The platform has three feedback loops today and none of them reach a person: the event bus
//! records facts, the audit trail records privileged work, and the search index records
//! documents. This crate is the fourth — the one that says *you*, and is the only one a
//! reader can act on.
//!
//! It is infrastructure, like `omnion-events` and `omnion-audit`: it knows what a notification
//! *is*, not what an approval or a security alert means. The router that turns a bus event
//! into a notification is data (slice 3), and a module emits through [`store::record`] without
//! this crate ever naming the module.
//!
//! Slice 1 is the in-app half — the record, the list, the summary and the bulk actions. The
//! vocabulary in [`vocabulary`] is deliberately compile-time: it is the same list the panel's
//! category filter, the preference matrix's rows and the SQL check constraints are all built
//! from, so a category cannot exist in one and not the others.

#![forbid(unsafe_code)]

pub mod error;
pub mod model;
pub mod preference_store;
pub mod preferences;
pub mod store;
pub mod vocabulary;

pub use error::{NotificationError, Result};
pub use model::{
    CategoryCount, ListQuery, NewNotification, Notification, NotificationPage, Summary,
};
pub use preference_store::{
    allowed_channels, read_preferences, read_settings, reset_preferences, write_preferences,
    write_settings,
};
pub use preferences::{
    DIGEST_CADENCES, IN_APP, PreferenceCell, Preferences, Settings, StatedPreference,
    default_quiet_hours, validate_quiet_hours, validate_settings, validate_stated,
};
pub use store::{
    archive, delete, find, list, mark_all_read, record, record_many, set_read, set_read_many,
    summary, validate_categories, validate_channel, within_emit_budget,
};
pub use vocabulary::{
    CATEGORIES, CHANNELS, EMIT_BUDGET_PER_MINUTE, MAX_BULK_IDS, MAX_PAGE, PRIORITIES, is_category,
    is_channel, is_priority,
};
