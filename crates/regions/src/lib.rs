//! `omnion-regions` — the edge / multi-region layer (docs/requests/REQ-035, slice 1).
//!
//! The crate holds everything about regions that is a *decision* rather than I/O: what a
//! region's status is when its services disagree, when a measurement is too old to draw,
//! and what a routing default points at. Persistence lives in [`store`] behind the `store`
//! feature, so the rules are testable without a database — which is the reason it is a crate
//! rather than a module in `apps/api`.
//!
//! # The property this crate is built around
//!
//! **A region with no fresh information is never green.** The REQ says it twice, in two
//! different sentences: "a region with no recent checks shows `Unknown` rather than green" and
//! "an unreachable control plane shows `Unknown`, never green". That is not a styling
//! preference — it is the difference between a panel that tells an operator when to act and
//! one that agrees with them about everything until the day it does not. So:
//!
//! * [`ServiceStatus::Unknown`] and the fold in [`rules::aggregate`] both refuse to return
//!   [`RegionStatus::Healthy`] for an empty or entirely stale check set.
//! * [`rules::p95_of`] returns `Option`, never `0`. A zero-millisecond region renders as the
//!   fastest region on the page.
//! * [`ServiceStatus::worst`] orders `unknown` **below** `down`: a region with a known
//!   outage is more actionable than a region nobody can reach, and a fold that ranked them
//!   the other way would make a checker network partition look like an outage everywhere.
//!
//! # The registry describes, it does not provision
//!
//! Nothing in this crate creates infrastructure. `POST`/`PATCH` on a region rename it, change
//! its operator status or move its default flag; a row whose `api_endpoint` points nowhere is
//! a fact the health matrix will report, not something the panel can fix. A registry that
//! could *add* a region would let a mistyped admin action advertise capacity that does not
//! exist, and every routing decision downstream would inherit the mistake.
//!
//! Slice 1 is the registry, the health history, the latency matrix and the read surfaces.
//! Residency policy (slice 2), the routing document and failover (slice 3) and the migration
//! workflow (slice 4) are built on top of these tables — their tables are not created early
//! to be filled in later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod model;
#[cfg(feature = "store")]
pub mod store;

pub use error::{RegionError, Result};
pub use model::{
    HealthCell, HealthMatrix, HealthPolicy, HistoryEntry, LatencyCell, LatencyMatrix, Region,
    RegionOverview, RegionStatus, RegionView, Service, ServiceCheck, ServiceStatus, rules,
};

/// The label the API uses for this surface in the permission catalogue and the event bus.
///
/// One constant, because a QA step, a route guard, an audit entry and an event name all have
/// to spell the prefix the same way, and four hand-typed copies of a prefix is four chances to
/// spell one of them differently.
pub const PERMISSION_PREFIX: &str = "platform.";
