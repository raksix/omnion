//! Omnion events: the platform's event bus and its signed webhook deliveries.
//!
//! The CMS is not a closed box: publishing a page is a fact other software needs to hear about
//! (docs/01-VISION.md §13). This crate records those facts and delivers them:
//!
//! * [`bus`] — recording an event. One call writes the event row and queues one delivery per
//!   subscribed, enabled endpoint of the same organization, atomically.
//! * [`store`] — the SQL behind it: events, endpoints, the delivery queue and its leases.
//! * [`signature`] — the HMAC-SHA256 scheme a receiver verifies a delivery with.
//! * [`sender`] — one delivery's HTTP round trip, and the outcome a runner records.
//! * [`engine`] — the delivery runner: claim due rows, send, record, retry with backoff.
//!
//! The crate is infrastructure, like the audit trail: it knows nothing about pages, media or
//! workflows beyond the event names they emit, and the modules that do the work call it — never
//! the other way round. Receivers are documented in `docs/BUILD-LOG.md` (P12) and proven by the
//! development receiver in `infra/mocks/webhook-receiver.mjs`.

#![forbid(unsafe_code)]

pub mod bus;
pub mod engine;
pub mod error;
pub mod model;
pub mod sender;
pub mod signature;
pub mod store;
pub mod validation;

pub use error::{EventsError, Result};
pub use model::{
    DEFAULT_MAX_ATTEMPTS, Delivery, DeliveryStatus, EndpointChanges, Event, NewEndpoint, NewEvent,
    WebhookEndpoint,
};
pub use signature::{SIGNATURE_HEADER, verify};
