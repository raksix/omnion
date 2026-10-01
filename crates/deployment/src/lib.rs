//! `omnion-deployment` — the deployment centre (docs/requests/REQ-024).
//!
//! The screen in the brief is four lines, and the dangerous part is which of them is a lie.
//! `Version 2.4.1` / `Available 2.5.0` reads as a lookup, so the temptation is to store a
//! version string and compare it — and every shortcut at that comparison ships: text ordering
//! offers a downgrade from `1.10.0` to `1.9.0`, a nightly build is offered to a stable
//! installation as though it were the next release, and a release that needs a newer core
//! appears on the card and fails halfway through a deploy.
//!
//! So the decisions live here, as named types with their own tests, rather than as expressions at
//! the route handler or the card. Persistence lives in [`store`] on top of this crate; the
//! routes and the update-check runner are in `apps/api`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod job;
pub mod manifest;
pub mod preflight;
pub mod version;

pub use error::{FeedContext, StoreError};
pub use job::{Job, JobKind, JobStatus, Step, StepStatus, may_cancel, plan_steps};
pub use manifest::{
    CheckResult, Manifest, RejectedEntry, SeenSet, parse_manifest, run_check, stale_banner,
};
pub use preflight::{
    CheckId, CheckOutcome, CheckState, Confirmation, PreflightReport, confirmation_for,
    confirmation_matches,
};
pub use version::{Availability, Channel, Release, Version, VersionError, availability};

/// Database access, behind the `store` feature.
///
/// Gated because the decision types above are worth unit-testing with no pool and no database,
/// and because a crate whose *first* module is SQL cannot be tested at all on a machine whose
/// PostgreSQL is in recovery — which is exactly the situation this crate was written in.
#[cfg(feature = "store")]
pub mod store;

#[cfg(feature = "store")]
// `UpdateCheck`, not `CheckState`: `preflight::CheckState` is the pass/warn/fail/unknown of a
// wizard row, and a crate that exports two things called `CheckState` forces every reader to
// write the module path to say which one they mean.
pub use store::{
    DeploymentRow, HealthRow, HistoryFilter, ReleaseRow, StepRow, UpdateCheck,
};
