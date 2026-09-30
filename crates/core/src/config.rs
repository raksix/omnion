//! Typed runtime configuration.
//!
//! Every Omnion service reads its settings from the process environment and validates them
//! once at boot, so a misconfigured deployment fails fast instead of failing mid-request
//! (docs/02-ARCHITECTURE.md, "Reliability"). Keys are namespaced with `OMNION_`; the single
//! exception is `PORT`, honoured as a fallback because PaaS platforms set it directly.

use std::net::SocketAddr;

use crate::error::ConfigError;

/// HTTP port used when neither `OMNION_PORT` nor `PORT` is set.
pub const DEFAULT_PORT: u16 = 8080;

/// Bind host used when `OMNION_HOST` is not set — all interfaces, so containers and
/// readiness probes can reach the API.
pub const DEFAULT_HOST: &str = "0.0.0.0";

/// Database URL matching `infra/compose/postgres.yml`.
pub const DEFAULT_DATABASE_URL: &str = "postgres://omnion:omnion@127.0.0.1:5433/omnion";

/// Redis URL matching `infra/compose/redis.yml`.
pub const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6380";

/// Default connection pool size for a single API instance.
pub const DEFAULT_DB_MAX_CONNECTIONS: u32 = 10;

/// Default tracing filter (`OMNION_LOG`).
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Default delay between two workflow-runner ticks (`OMNION_WORKFLOW_TICK_MS`).
pub const DEFAULT_WORKFLOW_TICK_MS: u64 = 1_000;

/// Default workflow wait-sweeper cadence (`OMNION_WORKFLOW_SWEEP_SECONDS`).
pub const DEFAULT_WORKFLOW_SWEEP_SECONDS: u64 = 30;

/// Default number of steps one tick may advance (`OMNION_WORKFLOW_BATCH`).
pub const DEFAULT_WORKFLOW_BATCH: usize = 16;

/// Default number of schedules one tick may start (`OMNION_WORKFLOW_SCHEDULER_BATCH`).
pub const DEFAULT_WORKFLOW_SCHEDULER_BATCH: usize = 8;

/// Default first retry backoff in milliseconds (`OMNION_WORKFLOW_RETRY_BASE_MS`).
pub const DEFAULT_WORKFLOW_RETRY_BASE_MS: u64 = 5_000;

/// Default ceiling of the retry backoff in milliseconds (`OMNION_WORKFLOW_RETRY_MAX_MS`).
pub const DEFAULT_WORKFLOW_RETRY_MAX_MS: u64 = 300_000;

/// Default delay between two webhook-delivery ticks (`OMNION_EVENTS_POLL_MS`).
pub const DEFAULT_EVENTS_POLL_MS: u64 = 5_000;

/// Default number of deliveries one tick may claim (`OMNION_EVENTS_BATCH`).
pub const DEFAULT_EVENTS_BATCH: usize = 20;

/// Default claim lease in seconds (`OMNION_EVENTS_LEASE_SECONDS`).
pub const DEFAULT_EVENTS_LEASE_SECONDS: u64 = 120;

/// Default per-delivery request timeout in milliseconds (`OMNION_EVENTS_REQUEST_TIMEOUT_MS`).
pub const DEFAULT_EVENTS_REQUEST_TIMEOUT_MS: u64 = 10_000;

/// Default first retry backoff in milliseconds (`OMNION_EVENTS_RETRY_BASE_MS`).
pub const DEFAULT_EVENTS_RETRY_BASE_MS: u64 = 15_000;

/// Default ceiling of the retry backoff in milliseconds (`OMNION_EVENTS_RETRY_MAX_MS`).
pub const DEFAULT_EVENTS_RETRY_MAX_MS: u64 = 900_000;

/// Default delay between two event-retention sweeps (`OMNION_EVENT_RETENTION_POLL_MS`).
///
/// A **week** is the tick, and the number needs a reason. The sweep deletes nothing on almost
/// every tick by construction — the predicate requires an event to be past *its
/// organization's* window — so a frequent tick would be a frequent no-op. A week is longer
/// than the shortest window the API accepts (one day) by a margin that makes the commonest
/// sweep — an organization that set a one-day window — happen on a predictable cadence, and
/// the run log records each of them, so "when did this last happen" is answerable.
pub const DEFAULT_EVENT_RETENTION_POLL_MS: u64 = 604_800_000;

/// How many organizations one retention sweep walks (`OMNION_EVENT_RETENTION_MAX_ORGS`).
///
/// The bound exists because the sweeper holds a transaction per organization: a thousand
/// organizations in one tick is a thousand transactions competing with the delivery runner for
/// the same connections, and the tail of the list would wait. Bounded, the rest is picked up by
/// the next tick rather than lost.
pub const DEFAULT_EVENT_RETENTION_MAX_ORGS: i64 = 50;

/// The same bound as a `usize`, because the environment is read through `read_count`, which
/// parses into a `usize` and refuses a negative value — a signed default there is a type error
/// at the call site, and the fix is made in one place instead of at every read.
const DEFAULT_EVENT_RETENTION_MAX_ORGS_US: usize = 50;

/// Default delay between two automation-matcher ticks (`OMNION_AUTOMATION_POLL_MS`).
pub const DEFAULT_AUTOMATION_POLL_MS: u64 = 2_000;

/// Default number of events one matcher tick evaluates (`OMNION_AUTOMATION_BATCH`).
pub const DEFAULT_AUTOMATION_BATCH: usize = 100;

/// Default delay between two search-indexer ticks (`OMNION_SEARCH_POLL_MS`).
pub const DEFAULT_SEARCH_POLL_MS: u64 = 2_000;

/// Default number of events one search-indexer tick applies (`OMNION_SEARCH_BATCH`).
pub const DEFAULT_SEARCH_BATCH: usize = 200;

/// Default delay between two analytics-rollup ticks (`OMNION_ANALYTICS_POLL_MS`).
pub const DEFAULT_ANALYTICS_POLL_MS: u64 = 15_000;

/// Default beacon budget of one site and caller per minute (`OMNION_ANALYTICS_COLLECT_PER_MINUTE`).
pub const DEFAULT_ANALYTICS_COLLECT_PER_MINUTE: u64 = 300;

/// How often the retention worker sweeps (REQ-010, slice 4).
pub const DEFAULT_RETENTION_POLL_MS: u64 = 900_000;

/// Sites one retention tick walks before it yields to the next tick.
pub const DEFAULT_RETENTION_MAX_SITES: i64 = 50;

/// The same bound as a `u64`, because the environment is read through `read_positive`, which
/// parses into a `u64` and refuses a negative or zero value.
const DEFAULT_RETENTION_MAX_SITES_U64: u64 = 50;

/// How often the CRM autoresponder worker sends what has come due (REQ-117, slice 3).
///
/// A send delay is a promise measured in minutes, so the tick is measured in minutes: a
/// source configured to answer after ten minutes must not be waiting for a nightly sweep.
pub const DEFAULT_CRM_AUTORESPONDER_POLL_MS: u64 = 60_000;

/// How often the CRM SLA worker escalates breached leads and reminds owners (REQ-117,
/// slice 3).
///
/// **The same reasoning as the autoresponder, and for the same reason.** A first-response
/// target is a promise measured in minutes — a 60-minute policy that escalates on a nightly
/// sweep is a policy nobody believes after the first overnight lead. A tick that finds nothing
/// is one indexed read against a partial index (`crm_leads_sla_idx`, which holds only live
/// clocks), so a broken worker shows up within the tick rather than the next morning.
pub const DEFAULT_CRM_SLA_POLL_MS: u64 = 60_000;

/// How many organizations the CRM SLA worker walks in one pass. Ten thousand tenants at a few
/// statements each is a maintenance window, and the organizations that wait for the next tick
/// are the ones whose deadlines are oldest — so the bound is a real cost, not a formality, which
/// is why the read is ordered rather than arbitrary.
pub const DEFAULT_CRM_SLA_MAX_ORGANIZATIONS: i64 = 100;

/// [`DEFAULT_CRM_SLA_MAX_ORGANIZATIONS`] as the unsigned value the env reader hands back.
///
/// The environment gives strings and the reader returns `u64`, so a signed constant needs a
/// twin — the same pairing `DEFAULT_RETENTION_MAX_SITES_U64` already exists for, and the
/// reason it is a `const` rather than a literal `100` at the call site is that the two numbers
/// must not drift: a default of 100 in one place and 10 in the other is a worker that reads
/// ten times slower than its own documentation says.
const DEFAULT_CRM_SLA_MAX_ORGANIZATIONS_U64: u64 = 100;

/// How often the project limit notice worker runs (REQ-133, slice 4).
///
/// A limit crossing is not a deadline, so a minute is generosity rather than a requirement: the
/// run counters move on a run, and a subscriber to the warning is an operations team rather than
/// an on-call pager. A tick that finds nothing is one partial-index read over the projects that
/// have a cap, so a broken worker shows up within the tick.
pub const DEFAULT_PROJECT_LIMIT_POLL_MS: u64 = 60_000;

/// How many capped projects one pass walks. A pass is four counts and up to four claims per
/// project, so a thousand projects is a maintenance window; the read is ordered by id so the
/// projects that wait for the next tick are the same ones every time rather than a rotating
/// subset that can starve a project for ever.
pub const DEFAULT_PROJECT_LIMIT_MAX_PROJECTS: i64 = 50;

/// [`DEFAULT_PROJECT_LIMIT_MAX_PROJECTS`] as the unsigned value the env reader hands back.
///
/// The environment gives strings and the reader returns `u64`, so a signed constant needs a twin
/// — the same pairing as the CRM SLA pair above, for the same reason: a default of 50 in one
/// place and 5 in the other is a worker that reads ten times slower than it says.
const DEFAULT_PROJECT_LIMIT_MAX_PROJECTS_U64: u64 = 50;
/// How often the backup retention sweep runs (REQ-013, slice 3).
///
/// Six hours, and the number is chosen from the feature rather than from taste: the sweep
/// only removes runs whose own `retain_until` has passed, the shortest window the panel
/// allows is a day, and the *newest successful* run is exempt whatever the window. So an
/// hourly sweep would find the same set as a six-hourly one almost every time — six times
/// the statements for an identical answer — and a nightly sweep would leave a run whose day
/// ended at 04:00 sitting on the destination for twenty hours. Six hours sits between the two
/// and keeps the unattended deletes to one a few hours per site.
pub const DEFAULT_BACKUP_SWEEP_POLL_MS: u64 = 21_600_000;

/// Tenants one backup sweep walks before the rest waits for the next tick.
pub const DEFAULT_BACKUP_SWEEP_MAX_TENANTS: i64 = 50;

/// The same bound as a `u64`, for the same reason as [`DEFAULT_RETENTION_MAX_SITES_U64`].
const DEFAULT_BACKUP_SWEEP_MAX_TENANTS_U64: u64 = 50;

/// Default SMTP host the email action sends through (`OMNION_SMTP_HOST`): Mailpit in the
/// development stack, which is where `infra/compose/mailpit.yml` publishes it.
pub const DEFAULT_SMTP_HOST: &str = "127.0.0.1";

/// Default SMTP port (`OMNION_SMTP_PORT`): Mailpit's plain SMTP port.
pub const DEFAULT_SMTP_PORT: u16 = 1025;

/// Default sender address of platform email (`OMNION_MAIL_FROM`).
pub const DEFAULT_MAIL_FROM: &str = "omnion@localhost";

/// Default per-message SMTP timeout in milliseconds (`OMNION_SMTP_TIMEOUT_MS`).
pub const DEFAULT_SMTP_TIMEOUT_MS: u64 = 10_000;

/// Deployment environment the process runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    /// Local development and staging: extra diagnostics (for example dependency error
    /// details in `/readyz`) and human-readable logs.
    Development,
    /// Production: minimal diagnostics and JSON logs ready for a log collector.
    Production,
}

impl Environment {
    /// Canonical lowercase name, used in logs and payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }

    /// `true` when running in development (extra diagnostics are allowed).
    #[must_use]
    pub const fn is_development(self) -> bool {
        matches!(self, Self::Development)
    }

    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw {
            "development" | "dev" | "local" => Ok(Self::Development),
            "production" | "prod" => Ok(Self::Production),
            other => Err(ConfigError::invalid(
                "OMNION_ENV",
                format!("expected `development` or `production`, got {other:?}"),
            )),
        }
    }
}

/// Shape of the log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable single lines for terminals.
    Pretty,
    /// One JSON object per event, for collectors and log platforms.
    Json,
}

impl LogFormat {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pretty => "pretty",
            Self::Json => "json",
        }
    }

    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw {
            "pretty" | "text" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            other => Err(ConfigError::invalid(
                "OMNION_LOG_FORMAT",
                format!("expected `pretty` or `json`, got {other:?}"),
            )),
        }
    }
}

/// Logging configuration consumed by [`crate::telemetry::init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// `tracing` filter expression, e.g. `info,omnion_api=debug`.
    pub filter: String,
    /// Output format.
    pub format: LogFormat,
}

impl LogConfig {
    /// Build a log configuration.
    #[must_use]
    pub fn new(filter: impl Into<String>, format: LogFormat) -> Self {
        Self {
            filter: filter.into(),
            format,
        }
    }
}

/// First-administrator bootstrap (docs/07-IAM.md).
///
/// Both values must come from the environment together. The API creates the account only
/// when the `users` table is still empty, so this is a one-shot seed for a fresh install —
/// the password is hashed at boot and never stored or logged in plain text.
#[derive(Clone, PartialEq, Eq)]
pub struct AdminBootstrap {
    /// Email address of the first administrator (`OMNION_ADMIN_EMAIL`).
    pub email: String,
    /// Plaintext password (`OMNION_ADMIN_PASSWORD`), hashed before it reaches the database.
    pub password: String,
}

impl std::fmt::Debug for AdminBootstrap {
    /// Never renders the password — configuration is logged at boot.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminBootstrap")
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// HTTP listener configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    /// Bind host (`OMNION_HOST`).
    pub host: String,
    /// Bind port (`OMNION_PORT`, falling back to `PORT`).
    pub port: u16,
}

impl HttpConfig {
    /// Resolve the bind address the server will listen on.
    pub fn bind_address(&self) -> Result<SocketAddr, ConfigError> {
        format!("{}:{}", self.host, self.port)
            .parse::<SocketAddr>()
            .map_err(|_| {
                ConfigError::invalid(
                    "OMNION_HOST",
                    format!(
                        "expected an IP address, got {:?} (use OMNION_HOST=0.0.0.0 for all interfaces)",
                        self.host
                    ),
                )
            })
    }
}

/// PostgreSQL configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// Connection string (`OMNION_DATABASE_URL`).
    pub url: String,
    /// Maximum pooled connections per instance (`OMNION_DB_MAX_CONNECTIONS`).
    pub max_connections: u32,
}

impl DatabaseConfig {
    /// Validate the URL scheme without connecting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.url.starts_with("postgres://") || self.url.starts_with("postgresql://") {
            return Ok(());
        }
        Err(ConfigError::invalid(
            "OMNION_DATABASE_URL",
            "expected a `postgres://` or `postgresql://` connection string",
        ))
    }
}

/// Redis configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisConfig {
    /// Connection string (`OMNION_REDIS_URL`).
    pub url: String,
}

impl RedisConfig {
    /// Validate the URL scheme without connecting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.url.starts_with("redis://") || self.url.starts_with("rediss://") {
            return Ok(());
        }
        Err(ConfigError::invalid(
            "OMNION_REDIS_URL",
            "expected a `redis://` or `rediss://` connection string",
        ))
    }
}

/// Workflow engine knobs (`OMNION_WORKFLOW_*`, phase P09).
///
/// The background runner of `apps/api` reads these; an installation that does not want the
/// engine ticking in-process turns it off with `OMNION_WORKFLOW_RUNNER=false` and runs the
/// steps from wherever it prefers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConfig {
    /// Whether this process runs the engine (`OMNION_WORKFLOW_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two ticks (`OMNION_WORKFLOW_TICK_MS`).
    pub tick_ms: u64,
    /// Delay between two sweeps (`OMNION_WORKFLOW_SWEEP_SECONDS`).
    pub sweep_seconds: u64,
    /// Steps one tick may advance (`OMNION_WORKFLOW_BATCH`).
    pub batch: usize,
    /// Schedules one tick may start (`OMNION_WORKFLOW_SCHEDULER_BATCH`).
    pub scheduler_batch: usize,
    /// First retry backoff (`OMNION_WORKFLOW_RETRY_BASE_MS`).
    pub retry_base_ms: u64,
    /// Ceiling of the retry backoff (`OMNION_WORKFLOW_RETRY_MAX_MS`).
    pub retry_max_ms: u64,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            tick_ms: DEFAULT_WORKFLOW_TICK_MS,
            sweep_seconds: DEFAULT_WORKFLOW_SWEEP_SECONDS,
            batch: DEFAULT_WORKFLOW_BATCH,
            scheduler_batch: DEFAULT_WORKFLOW_SCHEDULER_BATCH,
            retry_base_ms: DEFAULT_WORKFLOW_RETRY_BASE_MS,
            retry_max_ms: DEFAULT_WORKFLOW_RETRY_MAX_MS,
        }
    }
}

/// Event bus and webhook delivery knobs (`OMNION_EVENTS_*`, phase P12).
///
/// The background runner of `apps/api` reads these: it drains the delivery queue every
/// `poll_ms` and gives each receiver `request_timeout_ms` to answer. An installation that
/// delivers from somewhere else turns the runner off with `OMNION_EVENTS_RUNNER=false` — the
/// queue is durable, so another worker picks the work up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsConfig {
    /// Whether this process drains the webhook delivery queue (`OMNION_EVENTS_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two delivery ticks (`OMNION_EVENTS_POLL_MS`).
    pub poll_ms: u64,
    /// Deliveries one tick may claim (`OMNION_EVENTS_BATCH`).
    pub batch: usize,
    /// How long a claim is exclusive, in seconds (`OMNION_EVENTS_LEASE_SECONDS`).
    pub lease_seconds: u64,
    /// How long one delivery may take, in milliseconds (`OMNION_EVENTS_REQUEST_TIMEOUT_MS`).
    pub request_timeout_ms: u64,
    /// First retry backoff (`OMNION_EVENTS_RETRY_BASE_MS`).
    pub retry_base_ms: u64,
    /// Ceiling of the retry backoff (`OMNION_EVENTS_RETRY_MAX_MS`).
    pub retry_max_ms: u64,
    /// Whether this process sweeps the event bus (`OMNION_EVENT_RETENTION_RUNNER`).
    pub retention_enabled: bool,
    /// Delay between two retention sweeps (`OMNION_EVENT_RETENTION_POLL_MS`).
    pub retention_poll_ms: u64,
    /// How many organizations one retention sweep walks (`OMNION_EVENT_RETENTION_MAX_ORGS`).
    pub retention_max_orgs: i64,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_EVENTS_POLL_MS,
            batch: DEFAULT_EVENTS_BATCH,
            lease_seconds: DEFAULT_EVENTS_LEASE_SECONDS,
            request_timeout_ms: DEFAULT_EVENTS_REQUEST_TIMEOUT_MS,
            retry_base_ms: DEFAULT_EVENTS_RETRY_BASE_MS,
            retry_max_ms: DEFAULT_EVENTS_RETRY_MAX_MS,
            retention_enabled: true,
            retention_poll_ms: DEFAULT_EVENT_RETENTION_POLL_MS,
            retention_max_orgs: DEFAULT_EVENT_RETENTION_MAX_ORGS,
        }
    }
}

/// Automation matcher knobs (`OMNION_AUTOMATION_*`, phase P13).
///
/// The background matcher of `apps/api` reads the event bus every `poll_ms` and evaluates up to
/// `batch` recorded events per tick: which armed rules listen for their name, whether the
/// payload satisfies their conditions, and — when it does — the run it starts. Turning the
/// runner off (`OMNION_AUTOMATION_RUNNER=false`) leaves the events in the bus: the cursor is
/// durable, so another worker (or the next boot) picks the work up where this process left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationConfig {
    /// Whether this process matches events (`OMNION_AUTOMATION_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two matcher ticks (`OMNION_AUTOMATION_POLL_MS`).
    pub poll_ms: u64,
    /// Events one tick may evaluate (`OMNION_AUTOMATION_BATCH`).
    pub batch: usize,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_AUTOMATION_POLL_MS,
            batch: DEFAULT_AUTOMATION_BATCH,
        }
    }
}

/// Search indexer knobs (docs/requests/REQ-002): the background task that applies the event
/// bus to the search index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchConfig {
    /// Whether this process keeps the index fresh (`OMNION_SEARCH_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two indexer ticks (`OMNION_SEARCH_POLL_MS`).
    pub poll_ms: u64,
    /// Events one tick may apply (`OMNION_SEARCH_BATCH`).
    pub batch: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_SEARCH_POLL_MS,
            batch: DEFAULT_SEARCH_BATCH,
        }
    }
}

/// Analytics collection and rollup knobs (docs/requests/REQ-007).
///
/// The rollup worker of `apps/api` reads these: it rebuilds the recent hourly and daily buckets
/// every `poll_ms` (a bucket run is idempotent, so a slow or duplicated tick changes nothing).
/// `collect_per_minute` is the beacon budget of one site and caller per minute on the public
/// collection endpoint — exceeding it answers `429` and is counted, never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyticsConfig {
    /// Whether this process rolls up analytics buckets (`OMNION_ANALYTICS_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two rollup ticks (`OMNION_ANALYTICS_POLL_MS`).
    pub poll_ms: u64,
    /// Beacons one site and caller may send per minute (`OMNION_ANALYTICS_COLLECT_PER_MINUTE`).
    pub collect_per_minute: u64,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_ANALYTICS_POLL_MS,
            collect_per_minute: DEFAULT_ANALYTICS_COLLECT_PER_MINUTE,
        }
    }
}

/// The retention worker of `apps/api` reads these: each tick sweeps the superseded versions and
/// the trash of every site that has a library, and repairs the reference rows whose referent is
/// gone.
///
/// `poll_ms` is the *tick*, not the day. A sweep is idempotent — it claims rows, removes them
/// and writes what it computed — so a tick that finds nothing is a no-op, and the daily
/// character of the work comes from the windows in the policies rather than from the timer.
/// That is deliberate: a worker that only ran at 02:00 and did nothing on the rest of the day
/// is a worker whose single failure is invisible until the next morning, and a `poll_ms` of a
/// few minutes costs a handful of empty statements per site per tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Whether this process sweeps libraries (`OMNION_RETENTION_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two sweeps (`OMNION_RETENTION_POLL_MS`).
    pub poll_ms: u64,
    /// How many sites one tick may walk (`OMNION_RETENTION_MAX_SITES`).
    pub max_sites: i64,
    /// Whether this process sweeps expired backups off the destination (`OMNION_BACKUP_SWEEP`).
    ///
    /// A **separate** flag from `runner_enabled` on purpose. The media sweeper removes
    /// library files and the backup sweeper removes restore points, and an installation
    /// that wants to keep every backup for ever — an air-gapped archive, a compliance
    /// deployment that manages retention itself — must be able to stop the second without
    /// stopping the first. One flag for both would make "never delete my backups" mean "never
    /// purge my trash" as well, and the only way out would be to turn the whole worker off.
    pub backup_sweep_enabled: bool,
    /// Delay between two backup sweeps (`OMNION_BACKUP_SWEEP_POLL_MS`).
    ///
    /// The default is long on purpose and it is a **conservative** one: the sweep is
    /// unattended and its deletes are the only ones in this feature no operator asked for.
    /// A tick that finds nothing costs one grouped query, so the interval can be hours
    /// without cost — and an installation that has just restored something and wants the
    /// space back does not have to wait for a manual sweep to be offered in the panel.
    pub backup_sweep_poll_ms: u64,
    /// How many tenants one backup sweep may walk (`OMNION_BACKUP_SWEEP_MAX_TENANTS`).
    pub backup_sweep_max_tenants: i64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_RETENTION_POLL_MS,
            max_sites: DEFAULT_RETENTION_MAX_SITES,
            backup_sweep_enabled: true,
            backup_sweep_poll_ms: DEFAULT_BACKUP_SWEEP_POLL_MS,
            backup_sweep_max_tenants: DEFAULT_BACKUP_SWEEP_MAX_TENANTS,
        }
    }
}

/// The CRM autoresponder worker of `apps/api` reads these: each tick sends the reservations
/// whose configured send delay has elapsed (REQ-117, slice 3).
///
/// The worker exists because a send delay is a promise about *when*, and a promise with no
/// clock behind it is a promise nobody keeps. A source configured to answer after an hour
/// reserves the slot at capture and relies on this worker to complete it, so the tick is
/// minutes rather than days: the worst case an operator can observe is one tick of lateness
/// on a delay they chose, never a reply that never arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrmAutoresponderConfig {
    /// Whether this process sends due autoresponders (`OMNION_CRM_AUTORESPONDER_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two passes (`OMNION_CRM_AUTORESPONDER_POLL_MS`).
    pub poll_ms: u64,
}

impl Default for CrmAutoresponderConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_CRM_AUTORESPONDER_POLL_MS,
        }
    }
}

/// Knobs of the CRM SLA worker (REQ-117, slice 3).
///
/// The SLA worker is a *separate* process-level switch from the autoresponder's rather than a
/// second field on it. They answer two different promises — "somebody answers the visitor" and
/// "somebody answers the lead" — and an installation that wants to stop emailing acknowledgements
/// while keeping its escalation timer running is a configuration mistake, not a reason to couple
/// the two flags. (The same reasoning already separates the retention, search and analytics
/// workers, each of which has its own `runner_enabled`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrmSlaConfig {
    /// Whether this process escalates breaches and sends reminders
    /// (`OMNION_CRM_SLA_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two passes (`OMNION_CRM_SLA_POLL_MS`).
    pub poll_ms: u64,
    /// How many organizations one pass walks (`OMNION_CRM_SLA_MAX_ORGANIZATIONS`).
    ///
    /// Bounded because a pass is several statements per organization: an installation with
    /// thousands of tenants must not hold thousands of them open in one tick, and the bound is
    /// on the *query* so the truncation is visible in the log as a count rather than as a
    /// silent slice.
    pub max_organizations: i64,
}

impl Default for CrmSlaConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_CRM_SLA_POLL_MS,
            max_organizations: DEFAULT_CRM_SLA_MAX_ORGANIZATIONS,
        }
    }
}

/// The automation project limit notice worker (REQ-133, slice 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLimitConfig {
    /// Whether this process emits the project's limit crossings
    /// (`OMNION_PROJECT_LIMIT_RUNNER`).
    ///
    /// **On by default, and a second process is safe.** The once-ness is a claim in the
    /// database, not a property of one worker, so two API nodes both sweeping cannot both
    /// notify — which is why this is a switch rather than a singleton decision.
    pub runner_enabled: bool,
    /// Delay between two passes (`OMNION_PROJECT_LIMIT_POLL_MS`).
    pub poll_ms: u64,
    /// How many capped projects one pass walks (`OMNION_PROJECT_LIMIT_MAX_PROJECTS`).
    pub max_projects: i64,
}

impl Default for ProjectLimitConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_PROJECT_LIMIT_POLL_MS,
            max_projects: DEFAULT_PROJECT_LIMIT_MAX_PROJECTS,
        }
    }
}

/// Email settings of the `send_email` action (`OMNION_SMTP_*`, `OMNION_MAIL_*`).
///
/// Development defaults point at Mailpit, which the compose stack publishes on `1025`; a
/// production installation points the same settings at its own relay. The password is
/// write-only like every other secret in the configuration: it is read from the environment and
/// never rendered back (see the manual `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub struct MailConfig {
    /// Whether the email action may send (`OMNION_MAIL_ENABLED`).
    pub enabled: bool,
    /// SMTP server host (`OMNION_SMTP_HOST`).
    pub host: String,
    /// SMTP server port (`OMNION_SMTP_PORT`).
    pub port: u16,
    /// Sender address of platform email (`OMNION_MAIL_FROM`).
    pub from: String,
    /// Username for `AUTH PLAIN`, when the server wants one (`OMNION_SMTP_USERNAME`).
    pub username: Option<String>,
    /// Password for `AUTH PLAIN`, when the server wants one (`OMNION_SMTP_PASSWORD`).
    pub password: Option<String>,
    /// How long one message may take (`OMNION_SMTP_TIMEOUT_MS`).
    pub timeout_ms: u64,
}

impl MailConfig {
    /// `true` when a username and a password are both configured.
    #[must_use]
    pub fn authenticates(&self) -> bool {
        self.username.is_some() && self.password.is_some()
    }

    /// `true` when the email action should send: switched on and given a server and a sender.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.enabled && !self.host.trim().is_empty() && !self.from.trim().is_empty()
    }
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: DEFAULT_SMTP_HOST.to_owned(),
            port: DEFAULT_SMTP_PORT,
            from: DEFAULT_MAIL_FROM.to_owned(),
            username: None,
            password: None,
            timeout_ms: DEFAULT_SMTP_TIMEOUT_MS,
        }
    }
}

/// Render the mail settings without the credentials.
///
/// A password in a log line is a leaked password: the Debug output names the host, the sender
/// and whether credentials are configured, and never the secret itself.
impl std::fmt::Debug for MailConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MailConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("from", &self.from)
            .field("username", &self.username.as_deref().unwrap_or("<none>"))
            .field(
                "password",
                &if self.password.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// The secret a CSRF token is derived from (REQ-012, slice 2).
///
/// The token itself is `HMAC(this, session id)`, so this is the one value that decides whether a
/// browser may change something. Two consequences shape the struct:
///
/// * **It is not in the database.** A secret in a row lands in every backup and every replica; an
///   environment variable is what a deployment already knows how to keep out of both.
/// * **It is write-only.** `Debug` renders `<redacted>` and never the bytes, exactly like the
///   SMTP password above — the log line at boot says "a secret is configured", not what it is.
///
/// `is_usable` is the question that matters at runtime: with no secret the platform must **refuse
/// cookie-authenticated mutations**, not skip the check. Failing open here would turn a missing
/// configuration into a silent loss of a control nobody would notice was gone.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CsrfSecret {
    /// The raw key material, if one was configured.
    secret: Option<String>,
}

impl CsrfSecret {
    /// Build from a configured value; a blank string counts as unset.
    #[must_use]
    pub fn new(secret: Option<String>) -> Self {
        Self {
            secret: secret
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
        }
    }

    /// The key material, when one is configured.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.secret.as_ref().map(String::as_bytes)
    }

    /// `true` when tokens can be derived at all.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.secret.is_some()
    }
}

/// Render the CSRF settings without the secret.
impl std::fmt::Debug for CsrfSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CsrfSecret")
            .field(
                "secret",
                &if self.is_usable() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .finish()
    }
}

/// Fully validated runtime configuration of one Omnion service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Deployment environment.
    pub env: Environment,
    /// HTTP listener.
    pub http: HttpConfig,
    /// PostgreSQL.
    pub database: DatabaseConfig,
    /// Redis.
    pub redis: RedisConfig,
    /// Optional first-administrator bootstrap.
    pub admin: Option<AdminBootstrap>,
    /// Workflow engine knobs (P09).
    pub workflows: WorkflowConfig,
    /// Event bus and webhook delivery knobs (P12).
    pub events: EventsConfig,
    /// Automation matcher knobs (P13).
    pub automation: AutomationConfig,
    /// Search indexer knobs (REQ-002).
    pub search: SearchConfig,
    /// Analytics collection and rollup knobs (REQ-007).
    pub analytics: AnalyticsConfig,
    /// Retention worker knobs (REQ-010, slice 4).
    pub retention: RetentionConfig,
    /// CRM autoresponder worker knobs (REQ-117, slice 3).
    pub crm_autoresponder: CrmAutoresponderConfig,
    /// CRM SLA worker knobs (REQ-117, slice 3).
    pub crm_sla: CrmSlaConfig,
    /// The project limit notice worker (REQ-133, slice 4).
    pub project_limit: ProjectLimitConfig,
    /// Email settings of the `send_email` action (P13).
    pub mail: MailConfig,
    /// The secret CSRF tokens are derived from (REQ-012, slice 2).
    pub csrf: CsrfSecret,
    /// Logging.
    pub log: LogConfig,
}

impl Config {
    /// Load and validate the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|key| std::env::var(key).ok())
    }

    /// Load and validate the configuration from an arbitrary key/value source.
    ///
    /// Blank values are treated as unset, which is how empty compose variables behave.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let read = |key: &str| -> Option<String> {
            get(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let env = match read("OMNION_ENV") {
            Some(raw) => Environment::parse(&raw)?,
            None => Environment::Development,
        };

        let host = read("OMNION_HOST").unwrap_or_else(|| DEFAULT_HOST.to_owned());
        let port = match read("OMNION_PORT").or_else(|| read("PORT")) {
            Some(raw) => raw.parse::<u16>().map_err(|_| {
                ConfigError::invalid(
                    "OMNION_PORT",
                    format!("expected a TCP port number in 0-65535, got {raw:?}"),
                )
            })?,
            None => DEFAULT_PORT,
        };

        let database = DatabaseConfig {
            url: read("OMNION_DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned()),
            max_connections: match read("OMNION_DB_MAX_CONNECTIONS") {
                Some(raw) => raw
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        ConfigError::invalid(
                            "OMNION_DB_MAX_CONNECTIONS",
                            format!("expected a positive integer, got {raw:?}"),
                        )
                    })?,
                None => DEFAULT_DB_MAX_CONNECTIONS,
            },
        };

        let redis = RedisConfig {
            url: read("OMNION_REDIS_URL").unwrap_or_else(|| DEFAULT_REDIS_URL.to_owned()),
        };

        let admin = match (read("OMNION_ADMIN_EMAIL"), read("OMNION_ADMIN_PASSWORD")) {
            (Some(email), Some(password)) => Some(AdminBootstrap { email, password }),
            (None, None) => None,
            (Some(_), None) => {
                return Err(ConfigError::invalid(
                    "OMNION_ADMIN_PASSWORD",
                    "set it together with OMNION_ADMIN_EMAIL (both or neither)",
                ));
            }
            (None, Some(_)) => {
                return Err(ConfigError::invalid(
                    "OMNION_ADMIN_EMAIL",
                    "set it together with OMNION_ADMIN_PASSWORD (both or neither)",
                ));
            }
        };

        let format = match read("OMNION_LOG_FORMAT") {
            Some(raw) => LogFormat::parse(&raw)?,
            None if env.is_development() => LogFormat::Pretty,
            None => LogFormat::Json,
        };
        let log = LogConfig::new(
            read("OMNION_LOG").unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned()),
            format,
        );

        let workflows = WorkflowConfig {
            runner_enabled: read_flag(&read, "OMNION_WORKFLOW_RUNNER", true)?,
            tick_ms: read_positive(&read, "OMNION_WORKFLOW_TICK_MS", DEFAULT_WORKFLOW_TICK_MS)?,
            sweep_seconds: read_positive(
                &read,
                "OMNION_WORKFLOW_SWEEP_SECONDS",
                DEFAULT_WORKFLOW_SWEEP_SECONDS,
            )?,
            batch: read_count(&read, "OMNION_WORKFLOW_BATCH", DEFAULT_WORKFLOW_BATCH)?,
            scheduler_batch: read_count(
                &read,
                "OMNION_WORKFLOW_SCHEDULER_BATCH",
                DEFAULT_WORKFLOW_SCHEDULER_BATCH,
            )?,
            retry_base_ms: read_positive(
                &read,
                "OMNION_WORKFLOW_RETRY_BASE_MS",
                DEFAULT_WORKFLOW_RETRY_BASE_MS,
            )?,
            retry_max_ms: read_positive(
                &read,
                "OMNION_WORKFLOW_RETRY_MAX_MS",
                DEFAULT_WORKFLOW_RETRY_MAX_MS,
            )?,
        };

        let events = EventsConfig {
            runner_enabled: read_flag(&read, "OMNION_EVENTS_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_EVENTS_POLL_MS", DEFAULT_EVENTS_POLL_MS)?,
            batch: read_count(&read, "OMNION_EVENTS_BATCH", DEFAULT_EVENTS_BATCH)?,
            lease_seconds: read_positive(
                &read,
                "OMNION_EVENTS_LEASE_SECONDS",
                DEFAULT_EVENTS_LEASE_SECONDS,
            )?,
            request_timeout_ms: read_positive(
                &read,
                "OMNION_EVENTS_REQUEST_TIMEOUT_MS",
                DEFAULT_EVENTS_REQUEST_TIMEOUT_MS,
            )?,
            retry_base_ms: read_positive(
                &read,
                "OMNION_EVENTS_RETRY_BASE_MS",
                DEFAULT_EVENTS_RETRY_BASE_MS,
            )?,
            retry_max_ms: read_positive(
                &read,
                "OMNION_EVENTS_RETRY_MAX_MS",
                DEFAULT_EVENTS_RETRY_MAX_MS,
            )?,
            // The sweeper is a **separate flag from the delivery runner**, deliberately: an
            // installation that drains the queue from a dedicated process (and not at all from
            // the web nodes) must still be able to switch the sweeper off, and one flag for
            // both would force it to turn the delivery runner on to get retention.
            retention_enabled: read_flag(&read, "OMNION_EVENT_RETENTION_RUNNER", true)?,
            retention_poll_ms: read_positive(
                &read,
                "OMNION_EVENT_RETENTION_POLL_MS",
                DEFAULT_EVENT_RETENTION_POLL_MS,
            )?,
            retention_max_orgs: i64::try_from(read_count(
                &read,
                "OMNION_EVENT_RETENTION_MAX_ORGS",
                DEFAULT_EVENT_RETENTION_MAX_ORGS_US,
            )?)
            .unwrap_or(DEFAULT_EVENT_RETENTION_MAX_ORGS),
        };

        let automation = AutomationConfig {
            runner_enabled: read_flag(&read, "OMNION_AUTOMATION_RUNNER", true)?,
            poll_ms: read_positive(
                &read,
                "OMNION_AUTOMATION_POLL_MS",
                DEFAULT_AUTOMATION_POLL_MS,
            )?,
            batch: read_count(&read, "OMNION_AUTOMATION_BATCH", DEFAULT_AUTOMATION_BATCH)?,
        };

        let search = SearchConfig {
            runner_enabled: read_flag(&read, "OMNION_SEARCH_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_SEARCH_POLL_MS", DEFAULT_SEARCH_POLL_MS)?,
            batch: read_count(&read, "OMNION_SEARCH_BATCH", DEFAULT_SEARCH_BATCH)?,
        };

        // Read here so a malformed value is a configuration error at boot rather than a
        // worker that silently keeps its default — the same treatment every other knob gets.
        let retention = RetentionConfig {
            runner_enabled: read_flag(&read, "OMNION_RETENTION_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_RETENTION_POLL_MS", DEFAULT_RETENTION_POLL_MS)?,
            max_sites: i64::try_from(read_positive(
                &read,
                "OMNION_RETENTION_MAX_SITES",
                DEFAULT_RETENTION_MAX_SITES_U64,
            )?)
            .unwrap_or(DEFAULT_RETENTION_MAX_SITES),
            // The backup sweep reads its own root out of `backup_settings` every tick, so a
            // malformed value here would be a worker that kept its default and a destination
            // that quietly changed. Both knobs get the boot-time treatment every other one gets.
            backup_sweep_enabled: read_flag(&read, "OMNION_BACKUP_SWEEP", true)?,
            backup_sweep_poll_ms: read_positive(
                &read,
                "OMNION_BACKUP_SWEEP_POLL_MS",
                DEFAULT_BACKUP_SWEEP_POLL_MS,
            )?,
            backup_sweep_max_tenants: i64::try_from(read_positive(
                &read,
                "OMNION_BACKUP_SWEEP_MAX_TENANTS",
                DEFAULT_BACKUP_SWEEP_MAX_TENANTS_U64,
            )?)
            .unwrap_or(DEFAULT_BACKUP_SWEEP_MAX_TENANTS),
        };

        let crm_autoresponder = CrmAutoresponderConfig {
            runner_enabled: read_flag(&read, "OMNION_CRM_AUTORESPONDER_RUNNER", true)?,
            poll_ms: read_positive(
                &read,
                "OMNION_CRM_AUTORESPONDER_POLL_MS",
                DEFAULT_CRM_AUTORESPONDER_POLL_MS,
            )?,
        };

        let crm_sla = CrmSlaConfig {
            runner_enabled: read_flag(&read, "OMNION_CRM_SLA_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_CRM_SLA_POLL_MS", DEFAULT_CRM_SLA_POLL_MS)?,
            max_organizations: i64::try_from(read_positive(
                &read,
                "OMNION_CRM_SLA_MAX_ORGANIZATIONS",
                DEFAULT_CRM_SLA_MAX_ORGANIZATIONS_U64,
            )?)
            .unwrap_or(DEFAULT_CRM_SLA_MAX_ORGANIZATIONS),
        };

        let project_limit = ProjectLimitConfig {
            runner_enabled: read_flag(&read, "OMNION_PROJECT_LIMIT_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_PROJECT_LIMIT_POLL_MS", DEFAULT_PROJECT_LIMIT_POLL_MS)?,
            max_projects: i64::try_from(read_positive(
                &read,
                "OMNION_PROJECT_LIMIT_MAX_PROJECTS",
                DEFAULT_PROJECT_LIMIT_MAX_PROJECTS_U64,
            )?)
            .unwrap_or(DEFAULT_PROJECT_LIMIT_MAX_PROJECTS),
        };


        let analytics = AnalyticsConfig {
            runner_enabled: read_flag(&read, "OMNION_ANALYTICS_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_ANALYTICS_POLL_MS", DEFAULT_ANALYTICS_POLL_MS)?,
            collect_per_minute: read_positive(
                &read,
                "OMNION_ANALYTICS_COLLECT_PER_MINUTE",
                DEFAULT_ANALYTICS_COLLECT_PER_MINUTE,
            )?,
        };

        let mail = MailConfig {
            enabled: read_flag(&read, "OMNION_MAIL_ENABLED", true)?,
            host: read("OMNION_SMTP_HOST").unwrap_or_else(|| DEFAULT_SMTP_HOST.to_owned()),
            port: read_port(&read, "OMNION_SMTP_PORT", DEFAULT_SMTP_PORT)?,
            from: read("OMNION_MAIL_FROM").unwrap_or_else(|| DEFAULT_MAIL_FROM.to_owned()),
            username: read("OMNION_SMTP_USERNAME"),
            password: read("OMNION_SMTP_PASSWORD"),
            timeout_ms: read_positive(&read, "OMNION_SMTP_TIMEOUT_MS", DEFAULT_SMTP_TIMEOUT_MS)?,
        };

        // The CSRF secret is the one piece of configuration the platform refuses to invent: a
        // deployment that sets none still boots, and every cookie-authenticated mutation then
        // answers 403 rather than skipping the check. Failing open would turn a missing key into
        // a silent loss of a control nobody would notice.
        let csrf = CsrfSecret::new(read("OMNION_CSRF_SECRET"));

        let config = Self {
            env,
            http: HttpConfig { host, port },
            database,
            redis,
            admin,
            workflows,
            events,
            automation,
            search,
            analytics,
            retention,
            crm_autoresponder,
            crm_sla,
            project_limit,
            mail,
            csrf,
            log,
        };
        config.validate()?;
        Ok(config)
    }

    /// Re-check the loaded configuration (used by [`Config::from_source`] and by tests).
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.http.bind_address()?;
        self.database.validate()?;
        self.redis.validate()
    }
}

impl Default for Config {
    /// Development-friendly defaults that match the compose stack, without reading the
    /// environment. Handy for tests and local harnesses.
    fn default() -> Self {
        Self {
            env: Environment::Development,
            http: HttpConfig {
                host: DEFAULT_HOST.to_owned(),
                port: DEFAULT_PORT,
            },
            database: DatabaseConfig {
                url: DEFAULT_DATABASE_URL.to_owned(),
                max_connections: DEFAULT_DB_MAX_CONNECTIONS,
            },
            redis: RedisConfig {
                url: DEFAULT_REDIS_URL.to_owned(),
            },
            admin: None,
            workflows: WorkflowConfig::default(),
            events: EventsConfig::default(),
            automation: AutomationConfig::default(),
            search: SearchConfig::default(),
            analytics: AnalyticsConfig::default(),
            retention: RetentionConfig::default(),
            crm_autoresponder: CrmAutoresponderConfig::default(),
            crm_sla: CrmSlaConfig::default(),
            project_limit: ProjectLimitConfig::default(),
            mail: MailConfig::default(),
            // No secret by default, which is the honest default: a default key would be a key
            // every deployment shares, and a shared CSRF secret is no CSRF secret.
            csrf: CsrfSecret::default(),
            log: LogConfig::new(DEFAULT_LOG_FILTER, LogFormat::Pretty),
        }
    }
}

/// Read a boolean flag; anything else is a configuration error rather than a silent default.
fn read_flag<F>(read: &F, key: &str, default: bool) -> Result<bool, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match read(key) {
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(ConfigError::invalid(
                key,
                format!("expected a boolean (true/false), got {other:?}"),
            )),
        },
        None => Ok(default),
    }
}

/// Read an optional positive integer.
fn read_positive<F>(read: &F, key: &str, default: u64) -> Result<u64, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match read(key) {
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                ConfigError::invalid(key, format!("expected a positive integer, got {raw:?}"))
            }),
        None => Ok(default),
    }
}

/// Read an optional count (a positive integer that fits a `usize`).
fn read_count<F>(read: &F, key: &str, default: usize) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let value = read_positive(read, key, default as u64)?;
    usize::try_from(value).map_err(|_| {
        ConfigError::invalid(
            key,
            format!("expected a count that fits this platform, got {value}"),
        )
    })
}

/// Read an optional TCP port.
fn read_port<F>(read: &F, key: &str, default: u16) -> Result<u16, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let value = read_positive(read, key, u64::from(default))?;
    u16::try_from(value)
        .map_err(|_| ConfigError::invalid(key, format!("expected a port number, got {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        Config::from_source(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_match_the_compose_stack() {
        let config = config_from(&[]).expect("empty source must fall back to defaults");
        assert_eq!(config.env, Environment::Development);
        assert_eq!(config.http.port, DEFAULT_PORT);
        assert_eq!(config.http.host, DEFAULT_HOST);
        assert_eq!(config.database.url, DEFAULT_DATABASE_URL);
        assert_eq!(config.redis.url, DEFAULT_REDIS_URL);
        assert_eq!(config.log.format, LogFormat::Pretty);
        assert_eq!(
            config.http.bind_address().expect("default host must bind"),
            "0.0.0.0:8080".parse().expect("literal address")
        );
    }

    #[test]
    fn the_autoresponder_worker_ticks_in_minutes_not_days() {
        // The worker's whole reason to exist is a promise measured in minutes: a source
        // configured to answer after ten minutes must be inside a tick, not inside a nightly
        // sweep. A default of an hour would make the feature work on paper and never in use.
        let config = config_from(&[]).expect("defaults are valid");
        assert_eq!(
            config.crm_autoresponder.poll_ms,
            DEFAULT_CRM_AUTORESPONDER_POLL_MS
        );
        assert!(
            config.crm_autoresponder.poll_ms <= 5 * 60_000,
            "a send delay of minutes must be honoured within minutes, got {}ms",
            config.crm_autoresponder.poll_ms
        );
        assert!(config.crm_autoresponder.runner_enabled);
    }

    #[test]
    fn a_malformed_autoresponder_tick_is_a_boot_error_not_a_silent_default() {
        // A worker that silently keeps its default after being told to tick every second is
        // a worker that hammers the database for ever, and nothing in the log says so.
        let error = config_from(&[("OMNION_CRM_AUTORESPONDER_POLL_MS", "0")])
            .expect_err("a zero tick must be refused");
        assert_eq!(error.key, "OMNION_CRM_AUTORESPONDER_POLL_MS");
    }

    #[test]
    fn the_autoresponder_worker_can_be_switched_off_without_touching_the_others() {
        let config = config_from(&[
            ("OMNION_CRM_AUTORESPONDER_RUNNER", "false"),
            ("OMNION_RETENTION_RUNNER", "true"),
        ])
        .expect("flags are valid");
        assert!(!config.crm_autoresponder.runner_enabled);
        assert!(config.retention.runner_enabled);
    }

    #[test]
    fn the_sla_worker_ticks_in_minutes_because_a_first_response_target_is_measured_in_them() {
        // The SLA worker's promise is the same *kind* of promise as the autoresponder's, and it
        // fails the same way if the default is wrong: a 60-minute target escalated by a daily
        // sweep is a target that has already been missed by the time anybody hears about it,
        // and the screen keeps saying "on track" the whole time. The assertion is the bound,
        // not the value — a future reader who wants 5 minutes should be able to without
        // having to argue with this test.
        let config = config_from(&[]).expect("defaults are valid");
        assert_eq!(config.crm_sla.poll_ms, DEFAULT_CRM_SLA_POLL_MS);
        assert!(
            config.crm_sla.poll_ms <= 5 * 60_000,
            "an escalation must arrive inside the window it is escalating about, got {}ms",
            config.crm_sla.poll_ms
        );
        assert!(config.crm_sla.runner_enabled);
    }

    #[test]
    fn the_sla_worker_is_a_separate_switch_from_the_autoresponder() {
        // The trap this pins: the two workers look like one feature ("the CRM sends things on a
        // timer"), so a later reader folds them into one flag. An installation that stops
        // emailing acknowledgements and silently stops escalating its own overdue leads is a
        // failure with no log line, and it is exactly what a shared flag would produce.
        let config = config_from(&[
            ("OMNION_CRM_AUTORESPONDER_RUNNER", "false"),
            ("OMNION_CRM_SLA_RUNNER", "true"),
        ])
        .expect("flags are valid");
        assert!(!config.crm_autoresponder.runner_enabled);
        assert!(config.crm_sla.runner_enabled);

        let swapped = config_from(&[
            ("OMNION_CRM_AUTORESPONDER_RUNNER", "true"),
            ("OMNION_CRM_SLA_RUNNER", "false"),
        ])
        .expect("flags are valid");
        assert!(swapped.crm_autoresponder.runner_enabled);
        assert!(!swapped.crm_sla.runner_enabled);
    }

    #[test]
    fn a_malformed_sla_knob_is_a_boot_error_not_a_silent_default() {
        // Same treatment as every other knob: told to tick every millisecond and quietly
        // keeping a default means a worker hammering the database for ever with nothing in the
        // log to say so.
        let zero = config_from(&[("OMNION_CRM_SLA_POLL_MS", "0")])
            .expect_err("a zero tick must be refused");
        assert_eq!(zero.key, "OMNION_CRM_SLA_POLL_MS");

        let orgs = config_from(&[("OMNION_CRM_SLA_MAX_ORGANIZATIONS", "0")])
            .expect_err("walking zero organizations is a worker that never walks");
        assert_eq!(orgs.key, "OMNION_CRM_SLA_MAX_ORGANIZATIONS");

        // And the unsigned/signed twin of the bound must agree, or the documented default is
        // a different number from the one the process runs with.
        assert_eq!(
            u64::try_from(DEFAULT_CRM_SLA_MAX_ORGANIZATIONS).unwrap(),
            DEFAULT_CRM_SLA_MAX_ORGANIZATIONS_U64
        );
    }

    #[test]
    fn omnion_port_wins_over_the_port_fallback() {
        let config = config_from(&[("OMNION_PORT", "9100"), ("PORT", "18080")])
            .expect("both ports are valid");
        assert_eq!(config.http.port, 9100);
    }

    #[test]
    fn port_fallback_is_used_when_omnion_port_is_absent() {
        let config = config_from(&[("PORT", "18080")]).expect("fallback port is valid");
        assert_eq!(config.http.port, 18080);
    }

    #[test]
    fn invalid_port_is_rejected() {
        let error = config_from(&[("OMNION_PORT", "seventy")]).expect_err("port must be numeric");
        assert_eq!(error.key, "OMNION_PORT");
    }

    #[test]
    fn unknown_environment_is_rejected() {
        let error = config_from(&[("OMNION_ENV", "staging-ish")]).expect_err("env must be known");
        assert_eq!(error.key, "OMNION_ENV");
    }

    #[test]
    fn production_defaults_to_json_logs() {
        let config = config_from(&[("OMNION_ENV", "production")]).expect("production is valid");
        assert_eq!(config.env, Environment::Production);
        assert_eq!(config.log.format, LogFormat::Json);
    }

    #[test]
    fn blank_values_fall_back_to_defaults() {
        let config = config_from(&[("OMNION_DATABASE_URL", "   "), ("OMNION_LOG", "")])
            .expect("blank values are treated as unset");
        assert_eq!(config.database.url, DEFAULT_DATABASE_URL);
        assert_eq!(config.log.filter, DEFAULT_LOG_FILTER);
    }

    #[test]
    fn non_postgres_database_url_is_rejected() {
        let error = config_from(&[("OMNION_DATABASE_URL", "mysql://localhost/omnion")])
            .expect_err("only postgres URLs are supported");
        assert_eq!(error.key, "OMNION_DATABASE_URL");
    }

    #[test]
    fn non_redis_url_is_rejected() {
        let error = config_from(&[("OMNION_REDIS_URL", "http://localhost:6379")])
            .expect_err("only redis URLs are supported");
        assert_eq!(error.key, "OMNION_REDIS_URL");
    }

    #[test]
    fn pool_size_must_be_positive() {
        let error = config_from(&[("OMNION_DB_MAX_CONNECTIONS", "0")])
            .expect_err("pool size must be positive");
        assert_eq!(error.key, "OMNION_DB_MAX_CONNECTIONS");
    }

    #[test]
    fn admin_bootstrap_needs_both_values() {
        let error = config_from(&[("OMNION_ADMIN_EMAIL", "admin@example.com")])
            .expect_err("an email without a password must be rejected");
        assert_eq!(error.key, "OMNION_ADMIN_PASSWORD");

        let error = config_from(&[("OMNION_ADMIN_PASSWORD", "change-me-please-123")])
            .expect_err("a password without an email must be rejected");
        assert_eq!(error.key, "OMNION_ADMIN_EMAIL");
    }

    #[test]
    fn admin_bootstrap_loads_and_redacts_the_password() {
        let config = config_from(&[
            ("OMNION_ADMIN_EMAIL", "admin@example.com"),
            ("OMNION_ADMIN_PASSWORD", "change-me-please-123"),
        ])
        .expect("both values together are valid");

        let admin = config.admin.expect("bootstrap must be configured");
        assert_eq!(admin.email, "admin@example.com");
        assert_eq!(admin.password, "change-me-please-123");

        let rendered = format!("{admin:?}");
        assert!(
            !rendered.contains("change-me-please-123"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "rendered: {rendered}");
    }

    #[test]
    fn admin_bootstrap_is_absent_by_default() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.admin.is_none());
    }

    #[test]
    fn the_workflow_runner_has_development_defaults() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.workflows.runner_enabled);
        assert_eq!(config.workflows.tick_ms, DEFAULT_WORKFLOW_TICK_MS);
        assert_eq!(
            config.workflows.sweep_seconds,
            DEFAULT_WORKFLOW_SWEEP_SECONDS
        );
        assert_eq!(config.workflows.batch, DEFAULT_WORKFLOW_BATCH);
        assert_eq!(
            config.workflows.retry_base_ms,
            DEFAULT_WORKFLOW_RETRY_BASE_MS
        );
        assert_eq!(config.workflows.retry_max_ms, DEFAULT_WORKFLOW_RETRY_MAX_MS);
    }

    #[test]
    fn the_workflow_runner_is_configurable() {
        let config = config_from(&[
            ("OMNION_WORKFLOW_RUNNER", "false"),
            ("OMNION_WORKFLOW_TICK_MS", "250"),
            ("OMNION_WORKFLOW_SWEEP_SECONDS", "5"),
            ("OMNION_WORKFLOW_BATCH", "4"),
            ("OMNION_WORKFLOW_SCHEDULER_BATCH", "3"),
            ("OMNION_WORKFLOW_RETRY_BASE_MS", "100"),
            ("OMNION_WORKFLOW_RETRY_MAX_MS", "900"),
        ])
        .expect("the workflow settings are valid");

        assert!(!config.workflows.runner_enabled);
        assert_eq!(config.workflows.tick_ms, 250);
        assert_eq!(config.workflows.sweep_seconds, 5);
        assert_eq!(config.workflows.batch, 4);
        assert_eq!(config.workflows.scheduler_batch, 3);
        assert_eq!(config.workflows.retry_base_ms, 100);
        assert_eq!(config.workflows.retry_max_ms, 900);
    }

    #[test]
    fn broken_workflow_settings_fail_at_boot() {
        let error =
            config_from(&[("OMNION_WORKFLOW_TICK_MS", "0")]).expect_err("a zero tick is refused");
        assert_eq!(error.key, "OMNION_WORKFLOW_TICK_MS");

        let error = config_from(&[("OMNION_WORKFLOW_BATCH", "many")])
            .expect_err("a non-numeric batch is refused");
        assert_eq!(error.key, "OMNION_WORKFLOW_BATCH");

        let error = config_from(&[("OMNION_WORKFLOW_RUNNER", "maybe")])
            .expect_err("an unknown boolean is refused");
        assert_eq!(error.key, "OMNION_WORKFLOW_RUNNER");
    }

    #[test]
    fn the_event_delivery_runner_has_development_defaults() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.events.runner_enabled);
        assert_eq!(config.events.poll_ms, DEFAULT_EVENTS_POLL_MS);
        assert_eq!(config.events.batch, DEFAULT_EVENTS_BATCH);
        assert_eq!(config.events.lease_seconds, DEFAULT_EVENTS_LEASE_SECONDS);
        assert_eq!(
            config.events.request_timeout_ms,
            DEFAULT_EVENTS_REQUEST_TIMEOUT_MS
        );
        assert_eq!(config.events.retry_base_ms, DEFAULT_EVENTS_RETRY_BASE_MS);
        assert_eq!(config.events.retry_max_ms, DEFAULT_EVENTS_RETRY_MAX_MS);
    }

    #[test]
    fn the_event_delivery_runner_is_configurable() {
        let config = config_from(&[
            ("OMNION_EVENTS_RUNNER", "false"),
            ("OMNION_EVENTS_POLL_MS", "250"),
            ("OMNION_EVENTS_BATCH", "4"),
            ("OMNION_EVENTS_LEASE_SECONDS", "30"),
            ("OMNION_EVENTS_REQUEST_TIMEOUT_MS", "1000"),
            ("OMNION_EVENTS_RETRY_BASE_MS", "100"),
            ("OMNION_EVENTS_RETRY_MAX_MS", "900"),
        ])
        .expect("the event settings are valid");

        assert!(!config.events.runner_enabled);
        assert_eq!(config.events.poll_ms, 250);
        assert_eq!(config.events.batch, 4);
        assert_eq!(config.events.lease_seconds, 30);
        assert_eq!(config.events.request_timeout_ms, 1_000);
        assert_eq!(config.events.retry_base_ms, 100);
        assert_eq!(config.events.retry_max_ms, 900);
    }

    #[test]
    fn broken_event_settings_fail_at_boot() {
        let error =
            config_from(&[("OMNION_EVENTS_POLL_MS", "0")]).expect_err("a zero poll is refused");
        assert_eq!(error.key, "OMNION_EVENTS_POLL_MS");

        let error = config_from(&[("OMNION_EVENTS_BATCH", "many")])
            .expect_err("a non-numeric batch is refused");
        assert_eq!(error.key, "OMNION_EVENTS_BATCH");

        let error = config_from(&[("OMNION_EVENTS_RUNNER", "maybe")])
            .expect_err("an unknown boolean is refused");
        assert_eq!(error.key, "OMNION_EVENTS_RUNNER");
    }

    #[test]
    fn the_automation_matcher_and_mail_have_development_defaults() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.automation.runner_enabled);
        assert_eq!(config.automation.poll_ms, DEFAULT_AUTOMATION_POLL_MS);
        assert_eq!(config.automation.batch, DEFAULT_AUTOMATION_BATCH);

        // The defaults point at the compose stack's Mailpit, which is what makes the email
        // action work out of the box in development.
        assert!(config.mail.enabled);
        assert_eq!(config.mail.host, DEFAULT_SMTP_HOST);
        assert_eq!(config.mail.port, DEFAULT_SMTP_PORT);
        assert_eq!(config.mail.from, DEFAULT_MAIL_FROM);
        assert_eq!(config.mail.timeout_ms, DEFAULT_SMTP_TIMEOUT_MS);
        assert!(!config.mail.authenticates(), "no credentials by default");
        assert!(config.mail.is_usable());
    }

    #[test]
    fn the_automation_matcher_and_mail_are_configurable() {
        let config = config_from(&[
            ("OMNION_AUTOMATION_RUNNER", "false"),
            ("OMNION_AUTOMATION_POLL_MS", "250"),
            ("OMNION_AUTOMATION_BATCH", "7"),
            ("OMNION_MAIL_ENABLED", "false"),
            ("OMNION_SMTP_HOST", "mailpit"),
            ("OMNION_SMTP_PORT", "2525"),
            ("OMNION_MAIL_FROM", "platform@example.com"),
            ("OMNION_SMTP_USERNAME", "omnion"),
            ("OMNION_SMTP_PASSWORD", "secret"),
            ("OMNION_CSRF_SECRET", "secret"),
            ("OMNION_SMTP_TIMEOUT_MS", "1500"),
        ])
        .expect("the automation settings are valid");

        assert!(!config.automation.runner_enabled);
        assert_eq!(config.automation.poll_ms, 250);
        assert_eq!(config.automation.batch, 7);
        assert!(!config.mail.enabled);
        assert_eq!(config.mail.host, "mailpit");
        assert_eq!(config.mail.port, 2_525);
        assert_eq!(config.mail.from, "platform@example.com");
        assert_eq!(config.mail.username.as_deref(), Some("omnion"));
        assert!(config.mail.authenticates());
        assert_eq!(config.mail.timeout_ms, 1_500);
        assert!(!config.mail.is_usable(), "switched off is not usable");
    }

    #[test]
    fn a_mail_password_never_reaches_a_log_line() {
        let config = config_from(&[
            ("OMNION_SMTP_USERNAME", "omnion"),
            ("OMNION_SMTP_PASSWORD", "hunter2"),
        ])
        .expect("the mail settings are valid");

        let rendered = format!("{:?}", config.mail);
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        // The whole configuration renders safely too (it derives Debug through this field).
        let whole = format!("{:?}", config);
        assert!(!whole.contains("hunter2"), "{whole}");
    }

    #[test]
    fn the_csrf_secret_is_read_and_never_rendered() {
        let config = config_from(&[("OMNION_CSRF_SECRET", "s3cret-key-material")])
            .expect("a CSRF secret is a valid setting");
        assert!(config.csrf.is_usable());
        assert_eq!(
            config.csrf.as_bytes(),
            Some(b"s3cret-key-material".as_slice())
        );

        let rendered = format!("{:?}", config.csrf);
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("s3cret-key-material"), "{rendered}");
        let whole = format!("{:?}", config);
        assert!(
            !whole.contains("s3cret-key-material"),
            "the whole configuration derives Debug through this field: {whole}"
        );
    }

    #[test]
    fn a_blank_csrf_secret_counts_as_unset_rather_than_as_an_empty_key() {
        // An empty string is a value a compose file produces when a variable is declared and
        // left blank. Signing with an empty key would make every deployment's token identical.
        let config = config_from(&[("OMNION_CSRF_SECRET", "   ")])
            .expect("a blank secret is not a broken configuration");
        assert!(!config.csrf.is_usable());
        assert_eq!(config.csrf.as_bytes(), None);
    }

    #[test]
    fn a_deployment_without_a_csrf_secret_still_boots() {
        // Failing to boot would make the secret a hard deployment requirement; the honest
        // answer is that the platform starts and refuses cookie-authenticated mutations.
        let config = config_from(&[]).expect("the platform boots without a CSRF secret");
        assert!(!config.csrf.is_usable());
    }

    /// The backup sweep is gated by its OWN flag, and that is the property worth pinning: an
    /// installation that wants to keep every backup for ever must be able to stop the sweep
    /// without stopping the media sweeper, and the reverse must hold too. If these two ever
    /// share a flag again, this test is the one that notices.
    #[test]
    fn the_backup_sweep_has_its_own_switch() {
        let on = config_from(&[]).expect("the platform boots");
        assert!(on.retention.backup_sweep_enabled);
        assert!(on.retention.runner_enabled);

        let media_only = config_from(&[("OMNION_BACKUP_SWEEP", "false")])
            .expect("stopping the backup sweep is a valid configuration");
        assert!(!media_only.retention.backup_sweep_enabled);
        assert!(
            media_only.retention.runner_enabled,
            "stopping the backup sweep must not stop the media sweeper"
        );

        let backups_only = config_from(&[("OMNION_RETENTION_RUNNER", "false")])
            .expect("stopping the media sweeper is a valid configuration");
        assert!(!backups_only.retention.runner_enabled);
        assert!(
            backups_only.retention.backup_sweep_enabled,
            "stopping the media sweeper must not stop the backup sweep"
        );
    }

    /// A malformed sweep interval is a boot failure, not a worker that quietly kept its
    /// default — the same treatment every other interval gets, and the reason it matters most
    /// here is that the interval is how often unattended deletes happen.
    #[test]
    fn a_broken_backup_sweep_interval_fails_at_boot() {
        let error = config_from(&[("OMNION_BACKUP_SWEEP_POLL_MS", "0")])
            .expect_err("a zero interval is refused");
        assert_eq!(error.key, "OMNION_BACKUP_SWEEP_POLL_MS");

        let error = config_from(&[("OMNION_BACKUP_SWEEP_MAX_TENANTS", "plenty")])
            .expect_err("a non-numeric bound is refused");
        assert_eq!(error.key, "OMNION_BACKUP_SWEEP_MAX_TENANTS");
    }

    #[test]
    fn broken_automation_settings_fail_at_boot() {
        let error =
            config_from(&[("OMNION_AUTOMATION_POLL_MS", "0")]).expect_err("a zero poll is refused");
        assert_eq!(error.key, "OMNION_AUTOMATION_POLL_MS");

        let error = config_from(&[("OMNION_SMTP_PORT", "70000")])
            .expect_err("a port outside the range is refused");
        assert_eq!(error.key, "OMNION_SMTP_PORT");

        let error = config_from(&[("OMNION_AUTOMATION_BATCH", "many")])
            .expect_err("a non-numeric batch is refused");
        assert_eq!(error.key, "OMNION_AUTOMATION_BATCH");
    }
}
