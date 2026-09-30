//! Omnion reliability primitives (docs/requests/REQ-127-reliability-primitives.md).
//!
//! Five guarantees that look like five features and are actually one: **the platform must not
//! do a thing twice, must not give up too early, must not let one caller spend everyone's
//! budget, must not keep calling something that is already down, and must not accept a payload
//! it has not checked.** Each is a small, boring state machine that has to behave the same in a
//! request, a worker and the panel — so each lives here, next to its pure decision function and
//! away from the HTTP layer.
//!
//! * [`limits`] — the platform-wide budgets (user, organization, ip, route) and the pure
//!   [`limits::decide`] the middleware and the panel's dry-run both call.
//! * [`idempotency`] — the keyed write contract: fingerprint, replay, conflict, in-progress.
//! * [`retry`] — per-subsystem policies, the delay curve with its jitter modes, and the
//!   persisted next-attempt time that makes a restart resume rather than replay.
//! * [`breaker`] — the outbound state machine, half-open probing, and a state that survives a
//!   restart so a restart never pretends a broken provider is healthy.
//! * [`intake`] — the inbound guard: HMAC in constant time, a replay window, a size cap applied
//!   *before* authentication, and a narrow sanitisation pass.
//!
//! ## Why a crate and not five modules in `omnion-api`
//!
//! Every one of these is needed by something that does not link the API binary: a worker
//! retries, a queue consumer checks a breaker, the intake guard runs in front of
//! authentication. A second implementation of a backoff curve or an HMAC comparison is not a
//! refactor, it is a divergence — and the divergences here are the expensive kind, because
//! they only show up in production under load.
//!
//! ## The one rule every module obeys
//!
//! **The decision is pure and its inputs are visible in its answer.** `decide` never counts,
//! `evaluate` never sleeps, `verify` never reaches for a clock. Counters, clocks and secrets
//! arrive as arguments. That is what lets the panel's tester predict the middleware instead of
//! agreeing with it today, and it is why every module here is testable without a database.

pub mod breaker;
pub mod breaker_store;
pub mod error;
pub mod idempotency;
pub mod idem_store;
pub mod intake;
pub mod limiter_redis;
pub mod limits;
pub mod retry;
pub mod retry_store;
pub mod scheduler;
pub mod store;
pub mod vocabulary;

pub use error::{ReliabilityError, Result};
pub use limits::{LimitPolicy, Subject, Verdict};
pub use vocabulary::{
    BREAKER_STATES, IDEMPOTENCY_STATES, INTAKE_REASONS, INTAKE_SCHEMES, JITTER_MODES,
    MAX_PAGE, RATE_SCOPES, RETRY_SUBSYSTEMS, RETRY_OUTCOMES, SANITIZE_PROFILES,
};
