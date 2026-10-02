//! The shapes the region surface reads and writes (REQ-035).
//!
//! Slice 1 is the registry, the health history and the read surfaces, so everything here is a
//! *read* shape or a rule about one. Three decisions are load-bearing and the wrong version of
//! each is a defect rather than a preference:
//!
//! - **`unknown` is a status, not an absence.** [`ServiceStatus::Unknown`] and
//!   [`RegionStatus`]'s vocabulary both carry it because the REQ's own words are "an
//!   unreachable control plane shows `Unknown`, never green" and "a region with no recent
//!   checks shows `Unknown` rather than green". A panel that renders a missing check as
//!   `healthy` because `healthy` is the `Default` of an enum is a green light on a region
//!   nobody has heard from.
//! - **A region's status is *derived*, not stored, wherever it can be.** The stored
//!   `regions.status` is what an operator set (maintenance) or what the checker last concluded;
//!   [`aggregate`] recomputes it from the service checks so a service that stopped answering
//!   moves the region without anybody editing a row. Both are reported, and the panel shows
//!   the worse of the two — a row stuck on `healthy` while its database is `down` is the
//!   failure this exists to prevent.
//! - **Every number on a region card is a `Staleness`-qualified figure.** [`p95_of`] returns
//!   `None` when the newest sample is older than the window, and the panel renders that as
//!   *stale*, not as zero. A zero-millisecond region is the fastest region on the panel and
//!   the least likely to be true.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{RegionError, Result};

/// The seven services a region runs. Closed on purpose: the panel's badge row renders one
/// badge per service and the health matrix is a region × service grid, so a new service has to
/// be added here *and* in the database check constraint rather than appearing as a row nothing
/// else knows how to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    /// The public API.
    Api,
    /// The admin panel.
    Admin,
    /// The public renderer.
    Web,
    /// Background workers.
    Worker,
    /// The tenant's database.
    Database,
    /// Object storage.
    Storage,
    /// The cache tier.
    Cache,
}

impl Service {
    /// Every service, in the order the badge row renders them.
    ///
    /// The order is the request's own list, not alphabetical: `api`, `admin`, `web` are what a
    /// human sees first and `cache` is the last thing anyone thinks to ask about. A sorted
    /// list would put `admin` before `api` and `cache` first, which reads as a different
    /// system than the one the spec describes.
    pub const ALL: [Service; 7] = [
        Service::Api,
        Service::Admin,
        Service::Web,
        Service::Worker,
        Service::Database,
        Service::Storage,
        Service::Cache,
    ];

    /// The stored and wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Admin => "admin",
            Self::Web => "web",
            Self::Worker => "worker",
            Self::Database => "database",
            Self::Storage => "storage",
            Self::Cache => "cache",
        }
    }

    /// Parse a stored or submitted value, refusing anything else rather than defaulting.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "api" => Ok(Self::Api),
            "admin" => Ok(Self::Admin),
            "web" => Ok(Self::Web),
            "worker" => Ok(Self::Worker),
            "database" => Ok(Self::Database),
            "storage" => Ok(Self::Storage),
            "cache" => Ok(Self::Cache),
            other => Err(RegionError::UnknownService(other.to_owned())),
        }
    }

    /// A human label for the badge row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "API",
            Self::Admin => "Admin",
            Self::Web => "Web",
            Self::Worker => "Worker",
            Self::Database => "Database",
            Self::Storage => "Storage",
            Self::Cache => "Cache",
        }
    }
}

/// How a single service answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceStatus {
    /// Answering within its budget.
    Healthy,
    /// Answering, but late.
    Degraded,
    /// Not answering.
    Down,
    /// The checker could not reach the service at all.
    ///
    /// Distinct from [`ServiceStatus::Down`] and the distinction is the whole reason this
    /// enum is not two states: "the service answered with a failure" and "we could not ask"
    /// call for opposite responses, and collapsing them means a firewall between the checker
    /// and a healthy region renders the region as broken.
    Unknown,
}

impl ServiceStatus {
    /// The stored and wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored value, refusing anything else rather than defaulting.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            "down" => Ok(Self::Down),
            "unknown" => Ok(Self::Unknown),
            other => Err(RegionError::UnknownServiceStatus(other.to_owned())),
        }
    }

    /// The worst of two statuses, used when several checks describe one thing.
    ///
    /// `Unknown` is *not* the worst, and that is deliberate. A region with one `down` service
    /// and six `unknown` ones is a region with a known problem; a region with seven `unknown`
    /// ones is a region nobody can say anything about. The first is actionable and the second
    /// is not, so `unknown` sorts above `degraded` but below `down` — the order here is
    /// `healthy < degraded < unknown < down`, and it is the reason [`aggregate`] can treat a
    /// region with no reachable checker as `unknown` rather than as broken.
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    /// The ordering [`ServiceStatus::worst`] uses. Higher is worse.
    #[must_use]
    pub fn severity(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Unknown => 2,
            Self::Down => 3,
        }
    }

    /// Whether this status means an operator must do something.
    #[must_use]
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Down | Self::Unknown)
    }
}

/// How a whole region is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegionStatus {
    /// Every service that answered is healthy.
    Healthy,
    /// At least one is late, or at least one could not be reached.
    Degraded,
    /// At least one is down.
    Down,
    /// An operator put it here; the checker does not overwrite it.
    Maintenance,
}

impl RegionStatus {
    /// The stored and wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Maintenance => "maintenance",
        }
    }

    /// Parse a stored or submitted value, refusing anything else rather than defaulting.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            "down" => Ok(Self::Down),
            "maintenance" => Ok(Self::Maintenance),
            other => Err(RegionError::UnknownRegionStatus(other.to_owned())),
        }
    }
}

/// One service's latest check, as the matrix renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceCheck {
    /// Which service.
    pub service: Service,
    /// How it answered.
    pub status: ServiceStatus,
    /// Round-trip time, absent when the check could not time one.
    pub latency_ms: Option<i32>,
    /// When the check ran.
    pub checked_at: OffsetDateTime,
    /// Whether this check is older than the freshness window.
    ///
    /// A separate field rather than something the panel recomputes: the window is a
    /// *policy* the read endpoint answers, and two screens with two copies of the constant
    /// would disagree about which figure is stale.
    pub stale: bool,
}

/// The policy that decides when a check is too old to draw as current.
///
/// Three numbers, and each exists because the REQ asks for it: the window is what makes
/// "a region with no recent checks shows `Unknown`" decidable, the threshold is the
/// de-bounce the risks section asks for ("threshold and de-bounce so routing does not flap on
/// one missed check"), and the count is how many consecutive failures are needed before a
/// region is called degraded rather than merely flapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    /// A check older than this is stale and does not count towards the aggregate.
    pub fresh_window: time::Duration,
    /// A latency above this is `degraded` rather than `healthy`.
    pub latency_ceiling_ms: i32,
    /// Consecutive non-healthy checks before the region moves.
    pub failure_threshold: i32,
}

impl Default for HealthPolicy {
    /// The defaults the REQ's data model names: a 30-second read cache, a 2-second ceiling
    /// and three consecutive failures.
    fn default() -> Self {
        Self {
            fresh_window: time::Duration::seconds(300),
            latency_ceiling_ms: 2000,
            failure_threshold: 3,
        }
    }
}

/// A stored region, as the `regions` table holds it.
///
/// `Serialize` because [`RegionView`] flattens it: the panel's row is *the region plus three
/// derived figures*, and flattening means the JSON the list screen renders has the region's
/// own field names at the top level rather than nested under a `region` key that no other
/// screen in the platform uses. `sqlx::FromRow` is behind the feature so the rules stay
/// testable on the build that has no database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "store", derive(sqlx::FromRow))]
pub struct Region {
    /// Stable identifier, e.g. `tr-ankara`.
    pub code: String,
    /// Operator-facing label.
    pub display_name: String,
    /// Grouping for routing defaults: `tr`, `eu`, `us`.
    pub country_group: String,
    /// What the last check concluded, or what an operator set.
    pub status: String,
    /// Public API host.
    pub api_endpoint: String,
    /// Public admin host, absent on a region that serves only the API.
    pub admin_endpoint: Option<String>,
    /// Public web host, absent on a region that serves only the API.
    pub web_endpoint: Option<String>,
    /// Bucket label for this region's objects.
    pub storage_bucket: String,
    /// Cache key prefix.
    pub cache_namespace: String,
    /// The one region routing falls back to.
    pub is_default: bool,
    /// Inactive keeps history but stops routing.
    pub is_active: bool,
    /// Share of traffic, cached from the routing policy.
    pub traffic_share: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl Region {
    /// The stored status, parsed.
    ///
    /// A row whose status the enum does not know is reported as `degraded` rather than
    /// refused: this is a *read* of a descriptive table, and a status a future build added
    /// must not blank the whole panel for an older one. The parse is exercised on the write
    /// path, so an unknown status can only exist if it came from outside.
    #[must_use]
    pub fn status(&self) -> RegionStatus {
        RegionStatus::parse(&self.status).unwrap_or(RegionStatus::Degraded)
    }

    /// The country group, uppercased, as a routing rule's `country` match is written.
    #[must_use]
    pub fn group(&self) -> String {
        self.country_group.to_uppercase()
    }
}

/// One region × service cell of the health matrix.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthCell {
    /// The region.
    pub region_code: String,
    /// The service.
    pub service: Service,
    /// How it answered, or `unknown` when there is no fresh check at all.
    pub status: ServiceStatus,
    /// Round-trip time when one was measured.
    pub latency_ms: Option<i32>,
    /// When the check ran, absent when there is none.
    pub checked_at: Option<OffsetDateTime>,
    /// Whether the check is older than the freshness window.
    pub stale: bool,
}

/// The health matrix and its history, as `GET /regions/health` returns it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthMatrix {
    /// One cell per region × service, in the registry's order.
    pub cells: Vec<HealthCell>,
    /// The most recent status change per region × service, newest first, capped at 50.
    pub history: Vec<HistoryEntry>,
    /// When the newest check in this response was taken.
    pub newest_check: Option<OffsetDateTime>,
    /// Regions whose checks are entirely outside the freshness window.
    pub stale_region_codes: Vec<String>,
}

/// One recorded change, for the region's history table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryEntry {
    /// The region.
    pub region_code: String,
    /// The service.
    pub service: Service,
    /// What it changed to.
    pub status: ServiceStatus,
    /// When it changed.
    pub changed_at: OffsetDateTime,
}

/// One region-to-region latency figure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencyCell {
    /// Where the probe started.
    pub from_region: String,
    /// Where it went.
    pub to_region: String,
    /// 95th percentile round trip, absent when nothing has been measured.
    pub p95_ms: Option<i32>,
    /// How many samples the figure came from.
    pub sample_count: i32,
    /// When it was measured.
    pub measured_at: Option<OffsetDateTime>,
    /// Whether the figure is older than the matrix's refresh window.
    pub stale: bool,
}

/// The latency matrix, as `GET /regions/latency-matrix` returns it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencyMatrix {
    /// Cells in registry order, one row per source region.
    pub cells: Vec<LatencyCell>,
    /// When the newest sample in this response was taken.
    pub measured_at: Option<OffsetDateTime>,
    /// The window beyond which a figure is stale. The panel states it rather than colouring
    /// the cell, so "old" is legible without a legend.
    pub stale_after_seconds: i32,
}

/// Everything the list screen and the detail screen render, in one read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegionOverview {
    /// The registry.
    pub regions: Vec<RegionView>,
    /// The matrix.
    pub health: HealthMatrix,
    /// The region-to-region figures.
    pub latency: LatencyMatrix,
    /// `false` on a single-region deployment, which is what disables the multi-region
    /// actions with an explanation rather than hiding them.
    pub multi_region_active: bool,
    /// The sentence the panel shows when `multi_region_active` is `false`.
    pub inactive_reason: Option<String>,
}

/// A region as the panel renders it: the row plus what the health says about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegionView {
    /// The stored row.
    #[serde(flatten)]
    pub region: Region,
    /// The status recomputed from the service checks.
    pub derived_status: RegionStatus,
    /// The status actually shown: the worse of stored and derived.
    pub effective_status: RegionStatus,
    /// How many organizations have this region as their home.
    pub home_for_organizations: i64,
    /// The newest check in this region, absent when there is none.
    pub last_check_at: Option<OffsetDateTime>,
    /// The worst p95 across the region's services, absent when nothing is fresh.
    pub p95_ms: Option<i32>,
    /// The service checks, in [`Service::ALL`] order.
    pub services: Vec<ServiceCheck>,
}

/// Validation and aggregation rules, kept beside the shapes they govern.
pub mod rules {
    use super::{HealthPolicy, RegionStatus, ServiceCheck, ServiceStatus, Service};
    use crate::error::{RegionError, Result};

    /// Shortest accepted display name.
    pub const MIN_DISPLAY_NAME: usize = 2;
    /// Longest accepted display name.
    pub const MAX_DISPLAY_NAME: usize = 80;
    /// The longest a code may be. The regex caps the place name at 31 characters and the
    /// prefix at 3, so 34 is the ceiling and not an estimate.
    pub const MAX_CODE: usize = 34;

    /// Validate a submitted display name.
    pub fn validate_display_name(name: &str) -> Result<()> {
        let length = name.trim().chars().count();
        if !(MIN_DISPLAY_NAME..=MAX_DISPLAY_NAME).contains(&length) {
            return Err(RegionError::InvalidDisplayName {
                min: MIN_DISPLAY_NAME,
                max: MAX_DISPLAY_NAME,
            });
        }
        Ok(())
    }

    /// Validate a submitted code against the shape the database enforces.
    ///
    /// The migration's constraint is `^[a-z]{2}-[a-z][a-z0-9-]{0,29}[a-z0-9]$`, and this
    /// function is a **transcription of that pattern, not a second opinion about it**.
    ///
    /// That sentence is not a formality. The first version of this rule split the code on
    /// every dash and demanded exactly two segments, so it refused `us-virginia-2` -- which
    /// the database's looser `[a-z0-9-]{1,30}` accepted. Two implementations of one rule that
    /// disagree are worse than one rule: the API refuses a code the database would have
    /// stored, the panel reports a validation error for a perfectly valid region code, and the
    /// disagreement stays invisible until somebody types a two-word place. The second version
    /// then asserted a trailing dash was refused while the regex still permitted it, which the
    /// test caught on its first run.
    ///
    /// So the shape is walked exactly as the tightened regex reads it: two lowercase letters,
    /// one dash, a *letter* (the regex anchors the first place character to `[a-z]`, not to
    /// "any word character"), up to 29 further `[a-z0-9-]` characters, and a letter or digit
    /// last.
    pub fn validate_code(code: &str) -> Result<()> {
        let bytes = code.as_bytes();
        let two_letters = bytes.len() > 2
            && bytes[0].is_ascii_lowercase()
            && bytes[1].is_ascii_lowercase()
            && bytes[2] == b'-';
        let rest = &bytes[bytes.len().min(3)..];
        let last = rest.len().checked_sub(1);
        let valid = two_letters
            && rest.len() >= 2
            && rest.len() <= 31
            && rest[0].is_ascii_lowercase()
            && last.is_some_and(|i| rest[i].is_ascii_lowercase() || rest[i].is_ascii_digit())
            && rest[1..rest.len() - 1]
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
        if !valid {
            return Err(RegionError::InvalidCode { max: MAX_CODE });
        }
        Ok(())
    }

    /// Roll a region's service checks up into one status.
    ///
    /// The three rules, in the order they are applied, and each answers a different question:
    ///
    /// 1. **Maintenance wins.** An operator who put a region into maintenance gets `maintenance`
    ///    back whatever the checker says, because a region being drained *should* look
    ///    degraded and a panel that overrides it sends the operator hunting.
    /// 2. **No fresh check is `unknown`, not `healthy`.** An empty set does not aggregate to
    ///    the best status in the enum; it aggregates to the one that says "nobody knows".
    /// 3. **De-bounce.** A single `down` does not move the region until
    ///    [`HealthPolicy::failure_threshold`] consecutive checks say so. Without this, one
    ///    missed check flap the routing decision, which the REQ's risks section names as the
    ///    thing the threshold exists to stop.
    #[must_use]
    pub fn aggregate(stored: RegionStatus, checks: &[ServiceCheck], policy: &HealthPolicy) -> RegionStatus {
        if stored == RegionStatus::Maintenance {
            return RegionStatus::Maintenance;
        }
        let fresh: Vec<&ServiceCheck> = checks.iter().filter(|c| !c.stale).collect();
        if fresh.is_empty() {
            return RegionStatus::Degraded;
        }

        // The de-bounce reads a *streak*, so the checks must be ordered newest-first. The
        // store guarantees that order; reversing here would make a single `down` at the END
        // of a passing series look like three consecutive failures.
        let consecutive_failures = fresh
            .iter()
            .take_while(|c| c.status.is_actionable())
            .count() as i32;

        if consecutive_failures >= policy.failure_threshold.max(1) {
            return RegionStatus::Down;
        }
        if consecutive_failures > 0 {
            return RegionStatus::Degraded;
        }

        // With no actionable service left, the worst *latency* decides. A service that
        // answered slowly is degraded even when every other check is green, which is the
        // case a "worst status" roll-up alone would silently discard.
        let slowest = fresh
            .iter()
            .filter_map(|c| c.latency_ms)
            .max()
            .unwrap_or_default();
        if slowest > policy.latency_ceiling_ms {
            return RegionStatus::Degraded;
        }
        RegionStatus::Healthy
    }

    /// A region's p95, or `None` when nothing fresh has been measured.
    ///
    /// Returning `Option` rather than `0` is the point: the panel renders the absent case as
    /// "no recent measurements", and a `0` would render as the fastest region on the page.
    #[must_use]
    pub fn p95_of(checks: &[ServiceCheck]) -> Option<i32> {
        checks
            .iter()
            .filter(|c| !c.stale)
            .filter_map(|c| c.latency_ms)
            .max()
    }

    /// Turn a latency measurement into a status.
    ///
    /// A measurement over the ceiling is `degraded`; a measurement with no number at all is
    /// `unknown`, because a check that could not time the service is not a passing one.
    #[must_use]
    pub fn status_for_latency(latency_ms: Option<i32>, policy: &HealthPolicy) -> ServiceStatus {
        match latency_ms {
            None => ServiceStatus::Unknown,
            Some(ms) if ms > policy.latency_ceiling_ms => ServiceStatus::Degraded,
            Some(_) => ServiceStatus::Healthy,
        }
    }

    /// The service order the badge row renders, re-exported so callers do not import
    /// [`Service`] just to get it.
    pub fn service_order() -> [Service; 7] {
        Service::ALL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn check(service: Service, status: ServiceStatus, latency: Option<i32>, age: i64) -> ServiceCheck {
        ServiceCheck {
            service,
            status,
            latency_ms: latency,
            checked_at: OffsetDateTime::now_utc() - time::Duration::seconds(age),
            stale: age > 300,
        }
    }

    #[test]
    fn an_empty_check_set_is_degraded_not_healthy() {
        // The defect this prevents: `RegionStatus::Healthy` is the natural `Default` of any
        // fold over statuses, so "no data" silently reads as "everything is fine" and a
        // region nobody has ever checked renders green.
        let policy = HealthPolicy::default();
        assert_eq!(
            rules::aggregate(RegionStatus::Healthy, &[], &policy),
            RegionStatus::Degraded
        );
    }

    #[test]
    fn a_single_missed_check_flaps_nothing() {
        // The de-bounce, in the only direction that matters: one `down` followed by greens
        // is a missed check, not an outage.
        let policy = HealthPolicy::default();
        let checks = vec![
            check(Service::Api, ServiceStatus::Down, None, 5),
            check(Service::Admin, ServiceStatus::Healthy, Some(20), 5),
        ];
        assert_eq!(
            rules::aggregate(RegionStatus::Healthy, &checks, &policy),
            RegionStatus::Degraded
        );
    }

    #[test]
    fn three_consecutive_failures_move_the_region_to_down() {
        let policy = HealthPolicy::default();
        let checks = vec![
            check(Service::Api, ServiceStatus::Down, None, 5),
            check(Service::Admin, ServiceStatus::Down, None, 6),
            check(Service::Web, ServiceStatus::Down, None, 7),
            check(Service::Cache, ServiceStatus::Healthy, Some(10), 8),
        ];
        assert_eq!(
            rules::aggregate(RegionStatus::Healthy, &checks, &policy),
            RegionStatus::Down
        );
    }

    #[test]
    fn a_slow_service_degrades_an_otherwise_green_region() {
        // Every status is `healthy`, so a fold over statuses alone returns `healthy` and the
        // only signal that anything is wrong is the latency. This is the case the risk
        // section calls "routing should not flap" *without* the panel hiding it.
        let policy = HealthPolicy::default();
        let checks = vec![check(Service::Api, ServiceStatus::Healthy, Some(2500), 5)];
        assert_eq!(
            rules::aggregate(RegionStatus::Healthy, &checks, &policy),
            RegionStatus::Degraded
        );
    }

    #[test]
    fn maintenance_survives_a_healthy_checker() {
        // A region being drained *should* look degraded, and a panel that overrides the
        // operator's own decision sends them looking for a fault that does not exist.
        let policy = HealthPolicy::default();
        let checks = vec![check(Service::Api, ServiceStatus::Healthy, Some(10), 5)];
        assert_eq!(
            rules::aggregate(RegionStatus::Maintenance, &checks, &policy),
            RegionStatus::Maintenance
        );
    }

    #[test]
    fn a_stale_check_does_not_count_towards_the_aggregate() {
        // Every check is 10 minutes old, so the region is one nobody has heard from. The
        // stored status is `healthy` and each check says `healthy`; without the freshness
        // filter this returns `healthy`.
        let policy = HealthPolicy::default();
        let checks = vec![
            check(Service::Api, ServiceStatus::Healthy, Some(10), 600),
            check(Service::Admin, ServiceStatus::Healthy, Some(12), 601),
        ];
        assert_eq!(
            rules::aggregate(RegionStatus::Healthy, &checks, &policy),
            RegionStatus::Degraded
        );
    }

    #[test]
    fn no_measurement_is_never_a_zero_millisecond_region() {
        assert_eq!(rules::p95_of(&[]), None);
        assert_eq!(
            rules::p95_of(&[check(Service::Api, ServiceStatus::Down, None, 5)]),
            None
        );
        assert_eq!(
            rules::p95_of(&[check(Service::Api, ServiceStatus::Healthy, Some(42), 5)]),
            Some(42)
        );
    }

    #[test]
    fn a_check_that_could_not_be_timed_is_unknown_not_healthy() {
        let policy = HealthPolicy::default();
        assert_eq!(
            rules::status_for_latency(None, &policy),
            ServiceStatus::Unknown
        );
        assert_eq!(
            rules::status_for_latency(Some(10), &policy),
            ServiceStatus::Healthy
        );
        assert_eq!(
            rules::status_for_latency(Some(2001), &policy),
            ServiceStatus::Degraded
        );
    }

    #[test]
    fn unknown_sits_below_down_and_above_degraded() {
        // The order is the reason an empty check set can be `unknown` without making every
        // unreachable region look broken: a known `down` outranks "we could not ask".
        assert!(ServiceStatus::Down.severity() > ServiceStatus::Unknown.severity());
        assert!(ServiceStatus::Unknown.severity() > ServiceStatus::Degraded.severity());
        assert_eq!(
            ServiceStatus::Healthy.worst(ServiceStatus::Down),
            ServiceStatus::Down
        );
        assert_eq!(
            ServiceStatus::Down.worst(ServiceStatus::Healthy),
            ServiceStatus::Down
        );
    }

    #[test]
    fn a_code_is_refused_in_every_shape_the_database_would_refuse() {
        assert!(rules::validate_code("tr-ankara").is_ok());
        assert!(rules::validate_code("eu-frankfurt").is_ok());
        assert!(rules::validate_code("us-virginia-2").is_ok());
        // The migration's check is `^[a-z]{2}-[a-z][a-z0-9-]{1,30}$`, so every one of these
        // would be 23514 at insert time. Refusing them here means the panel says so at the
        // field instead of the API answering 500 from a constraint.
        for bad in ["tr", "tr-", "TR-ANKARA", "tr-ankara-", "t-ankara", "-ankara", "tr_a", ""] {
            assert!(
                rules::validate_code(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_display_name_is_refused_outside_its_length() {
        assert!(rules::validate_display_name("a").is_err());
        assert!(rules::validate_display_name("ab").is_ok());
        assert!(rules::validate_display_name(&"x".repeat(80)).is_ok());
        assert!(rules::validate_display_name(&"x".repeat(81)).is_err());
    }

    #[test]
    fn an_unknown_status_is_refused_rather_than_defaulted() {
        // Both parsers: defaulting an unknown status is how a region renders green because
        // a future build added a state an older one has never heard of.
        assert!(ServiceStatus::parse("flapping").is_err());
        assert!(RegionStatus::parse("flapping").is_err());
        assert!(Service::parse("cdn").is_err());
    }
}
