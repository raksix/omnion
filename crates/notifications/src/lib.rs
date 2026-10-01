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

pub mod audience;
pub mod delivery;
pub mod error;
pub mod model;
pub mod preference_store;
pub mod preferences;
pub mod push;
pub mod router;
pub mod store;
pub mod vocabulary;

pub use audience::{addressable_recipients, may_address, refused_recipients};
pub use delivery::{
    DeliveryConfig, DeliveryJob, EnqueueReport, InAppTransport, NO_TRANSPORT_YET,
    READER_SWITCHED_IT_OFF, RunReport as DeliveryRunReport, Transport, TransportOutcome, claim_due,
    enqueue, mark_failed, mark_retry, mark_sent, remote_channels, retry_delay, run_due,
    settle_not_ready,
};
pub use error::{NotificationError, Result};
pub use model::{
    CategoryCount, DeliveryRow, ListQuery, NewNotification, Notification, NotificationPage, Summary,
};
pub use preference_store::{
    allowed_channels, disabled_channels, read_preferences, read_settings, reset_preferences,
    write_preferences, write_settings,
};
pub use preferences::{
    DIGEST_CADENCES, IN_APP, PreferenceCell, Preferences, Settings, StatedPreference,
    default_quiet_hours, format_clock, validate_quiet_hours, validate_settings, validate_stated,
};
pub use push::{
    ChannelReadiness, MAX_OUTBOX_PAGE, OUTBOX_RETENTION_DAYS, OutboxCounts, OutboxQuery, OutboxRow,
    PrunedSubscription, PushSubscription, RegisterOutcome, RegisterReport, RetryOutcome,
    SUBSCRIPTION_STALE_DAYS, channel_readiness, list_outbox, list_subscriptions, outbox_counts,
    prune_deliveries, prune_endpoints, prune_stale, register, remove, retry_delivery,
};
pub use router::{
    RecipientRule, RouteReport, RouteRule, RoutedEvent, create_rule, dedupe_key, delete_rule,
    list_rules, render, resolve_recipients, route, rules_for_event,
};
pub use store::{
    BulkRecord, archive, delete, find, list, mark_all_read, recipient_organizations, record,
    record_many, record_many_with_deliveries, record_with_deliveries, set_read, set_read_many,
    summary, validate_categories, validate_channel, within_emit_budget,
};
pub use vocabulary::{
    CATEGORIES, CHANNELS, EMIT_BUDGET_PER_MINUTE, MAX_BULK_IDS, MAX_PAGE, PRIORITIES, is_category,
    is_channel, is_priority,
};
