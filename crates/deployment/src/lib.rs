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

pub mod cluster;
pub mod error;
pub mod job;
pub mod log_cursor;
pub mod maintenance;
pub mod manifest;
pub mod preflight;
pub mod version;

/// The job write side, behind the `store` feature.
///
/// Ungated until tick 100, and that is a real defect rather than a style note: `jobs.rs` is 692
/// lines of `sqlx` and `crate::store`, both of which only exist when the feature is on, so
/// `cargo test -p omnion-deployment` — the exact command the crate's own documentation and this
/// loop's gate both name — failed to *compile* with 34 errors. Only the feature-enabled form was
/// ever green, because `apps/api` is what turns the feature on, so the API built and the crate's
/// own default test command did not. The two database-free cursor helpers moved to
/// [`log_cursor`] so the log-stream edge cases keep their tests on the build with no database.
#[cfg(feature = "store")]
pub mod jobs;

pub use cluster::{
    MAX_WORKLOAD_NAME, Metric, Point, Process, RestartEdit, RestartRefusal, Runtime,
    SAMPLE_INTERVAL_SECONDS, SAMPLE_RETENTION_MINUTES, SAMPLE_WINDOW_MINUTES, Sparkline, Unit,
    Usage, Workload, check_restart, format_bytes, format_duration, prune_before, sample_bucket,
    sparkline,
};
pub use error::{FeedContext, StoreError};
pub use job::{Job, JobKind, JobStatus, Step, StepStatus, cancel_refusal, may_cancel, plan_steps};
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

/// The cluster panel's persistence, behind the same `store` feature.
///
/// A module of its own rather than more of [`store`], for the reason the slice's storage is not
/// the centre's storage: `store` answers "what does the deployment centre know" and this answers
/// "what did the cluster report a minute ago", with a different lifetime and a different pruning
/// rule. Folding it in would put a bounded, self-pruning history table behind the same
/// documentation as the release cache that must never be pruned.
#[cfg(feature = "store")]
pub mod cluster_store;

#[cfg(feature = "store")]
pub use cluster_store::{Recorded, SampleRow};

/// The maintenance window's storage, behind the same `store` feature.
///
/// Four queries and a `Window` type's worth of decisions, separated so the twenty decision tests
/// in [`maintenance`] run on a build with no database — the same split as [`log_cursor`] and
/// [`jobs`], and for the same reason: the crate's default test command could not compile while
/// the two halves shared a module.
#[cfg(feature = "store")]
pub mod maintenance_store;

#[cfg(feature = "store")]
// `UpdateCheck`, not `CheckState`: `preflight::CheckState` is the pass/warn/fail/unknown of a
// wizard row, and a crate that exports two things called `CheckState` forces every reader to
// write the module path to say which one they mean.
pub use store::{DeploymentRow, HealthRow, HistoryFilter, ReleaseRow, StepRow, UpdateCheck};
