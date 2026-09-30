//! Omnion system health — the probe registry, the samples and the honest history
//! (docs/requests/REQ-014, slice 1).
//!
//! A system health screen has one job no other screen has: **not to reassure the
//! operator when it has nothing to go on.** Every other list in this panel can
//! fall back to a plausible default; this one cannot, because a green row that
//! was never checked is the single most expensive thing the product can render.
//! So the rules are written into the design rather than left to each probe:
//!
//! * **A service that has never been probed is a row.** [`model::ServiceReport::overview`]
//!   emits one row per registered service always, including the ones with no
//!   sample. A client that renders only the services it received would make an
//!   unprobed platform look like a small one — and would report "all systems
//!   operational" for a platform nothing has ever looked at.
//! * **A probe that cannot reach is `down`, not absent and not an error.** A
//!   missing row is the dangerous failure: it reads as "nothing to report here".
//!   [`probes::Observation`] has no unreachable-error path for exactly that
//!   reason, and [`registry::run_all`] never removes a result.
//! * **A number that could not be read is no number.** Host metrics fall back to
//!   `unknown` with a sentence, and never divide by a zero total. A `0%` on a
//!   disk nobody managed to stat is a number nobody will question.
//! * **Fresh beats stored.** The runner's report always wins over the last
//!   sample, so a dependency that has just stopped answering cannot keep showing
//!   the green it earned when it was answering.
//!
//! It is infrastructure, like `omnion-audit` and `omnion-events`: this crate knows
//! what a probe *is*, not what Redis's or PostgreSQL's version history means. The
//! modules are split by the question each one answers — [`vocabulary`] what the
//! screen can say, [`probes`] what it can read, [`model`] what it stores, and
//! [`store`] the SQL.
//!
//! ## The one `unsafe` block in the workspace
//!
//! Every other crate here is `#![forbid(unsafe_code)]`, and this one is
//! `#![deny(unsafe_code)]` with a single `#[allow]` on one function. That
//! difference is deliberate and the reason is worth writing down, because a
//! reviewer should be able to check this decision rather than rediscover it:
//!
//! * **The disk row needs `statvfs`.** The kernel publishes CPU times, memory
//!   and load average in `/proc`, and this crate reads all three with nothing
//!   but `std::fs`. It publishes **no** block counts anywhere — there is no
//!   `/proc` file, no sysfs file and no environment variable that says how full
//!   a filesystem is. The only ways to ask are a syscall or a subprocess.
//! * **The subprocess was rejected, and this box is why.** Shelling out to `df`
//!   spawns a process on every probe (the default interval is 15 s) and makes the
//!   screen's most-watched number depend on a binary being intact. This host has
//!   already had a `coreutils` corruption incident where `/usr/bin/ls` silently
//!   returned exit 255 with no output; a status screen that reports `unknown`
//!   because `df` went missing is a screen that is wrong exactly when the
//!   machine is already wrong.
//! * **The alternative — reporting no disk number at all — is not honest
//!   either.** A metric card that exists and always says "unknown" is a dead
//!   affordance, which this project's definition of done forbids.
//!
//! So the call is made, and made small: one function, one C string, one
//! `SAFETY` comment, no pointer stored, no reference outliving the frame. If
//! `rustix` or `sysinfo` ever enters the dependency tree, that function becomes
//! the only thing to delete.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod history;
pub mod incidents;
pub mod model;
pub mod probes;
pub mod registry;
pub mod store;
pub mod vocabulary;

pub use error::{HealthError, Result};
pub use history::{
    CSV_HEADER, DEFAULT_RANGE, MetricSummary, RANGE_KEYS, Range, RollupDay, daily_rollup,
    metric_summaries, sparkline_values, summaries_to_csv,
};
pub use incidents::{
    BreachCheck, BREACH_WINDOW_SECONDS, HealthSettings, Incident, IncidentFilter, IncidentOutcome,
    IncidentPage, MAX_CHECK_INTERVAL_SECONDS, MAX_WORKER_STALE_SECONDS,
    MIN_CHECK_INTERVAL_SECONDS, MIN_WORKER_STALE_SECONDS, SettingsUpdate, THRESHOLD_METRICS,
    Threshold, Thresholds, Transition, acknowledge, apply, breach_count, breach_window, clear_breach,
    detect, incident, is_suppressed, list_incidents, load_settings, metric_unit, open_incident,
    parse_thresholds, record_breach, resolve, save_settings, suggested_thresholds, thresholds,
};
pub use model::{
    CheckDescription, HealthBanner, HealthOverview, HostMetric, NewSample, Sample, ServiceReport,
};
pub use probes::{
    DEGRADED_LATENCY_MS, DEGRADED_QUEUE_DEPTH, MetricReading, Observation, PROBE_TIMEOUT_MS,
    ProbeContext, ProbeResult, probe_api, probe_host, probe_postgres, probe_queue, probe_redis,
    probe_search, probe_storage, probe_workers,
};
pub use registry::{
    DESCRIPTIONS, build_overview, described_services, describe, registry_size, run_all,
    run_and_record, unprobed_overview,
};
pub use store::{
    SAMPLE_RETENTION_DAYS, last_sample_at, latest_sample, latest_samples, prune_old_samples,
    record, record_run, recorded_metrics, sample_count, samples_in_window,
};
pub use vocabulary::{
    HOST_METRICS, HOST_SERVICE, SERVICES, STATE_WHEN_UNPROBED, STATES, all_services,
    canonical_state, is_finite, is_state, rank, worst,
};
