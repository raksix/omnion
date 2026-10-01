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

/// Default delay between two AI health probe ticks (`OMNION_AI_HEALTH_POLL_MS`).
///
/// 60 s is the interval the request asks for: often enough that three consecutive failures (the
/// window in which a provider reads as `down`) arrive within five minutes, rarely enough that a
/// provider the operator just connected is not dialled five times before they can read the Health
/// tab.
pub const DEFAULT_AI_HEALTH_POLL_MS: u64 = 60_000;

/// How many agent runs one API process executes at a time (`OMNION_AI_RUNNER_CONCURRENCY`).
///
/// 4, not 1 and not 32. One is a runtime that feels broken while a tool takes four seconds; 32
/// is a runtime that can have thirty-two provider calls in flight on a four-core box, which is
/// both a timeout story and a bill. Four is what a small installation actually needs, and the
/// knob exists for the ones that do not.
pub const DEFAULT_AI_RUNNER_CONCURRENCY: usize = 4;

/// How long a run may stay `running` before the eval runner fails it (`OMNION_AI_EVAL_TIMEOUT`).
///
/// 900 s. The arithmetic that produced the number: a suite's cases run one at a time in this
/// slice, a model turn is on the order of 10 s, and a case carrying `rubric` is two of those
/// (the one under test plus the judge). 900 s therefore covers roughly forty such cases with
/// room for a slow provider, which is the largest suite the panel's own import will realistically
/// produce. Below that the reaper would fail healthy runs; above it a run abandoned by a crashed
/// process stays `running` past the point where an operator has given up looking.
pub const DEFAULT_AI_EVAL_TIMEOUT_SECONDS: i64 = 900;

/// How often the eval runner sweeps for suites whose schedule is due (`OMNION_AI_EVAL_SCHEDULER_MS`).
///
/// 60 s. A cron string has a one-minute resolution at best, so a faster sweep can only re-check
/// the same due suites; a slower one makes "hourly" fire up to a minute late, which is inside
/// the tolerance every other scheduled job in the platform already accepts.
pub const DEFAULT_AI_EVAL_SCHEDULER_MS: u64 = 60_000;

/// How often stale runs are failed (`OMNION_AI_EVAL_SWEEP_MS`).
///
/// Ten minutes. The sweep's predicate is "running longer than the timeout", so once a run has
/// been found it is already past that window and no second sweep inside the same window could
/// find anything new — the interval buys recovery speed for the *next* run, nothing else.
pub const DEFAULT_AI_EVAL_SWEEP_MS: u64 = 600_000;

/// How often the retention worker sweeps (REQ-010, slice 4).
pub const DEFAULT_RETENTION_POLL_MS: u64 = 900_000;

/// Sites one retention tick walks before it yields to the next tick.
pub const DEFAULT_RETENTION_MAX_SITES: i64 = 50;

/// The same bound as a `u64`, because the environment is read through `read_positive`, which
/// parses into a `u64` and refuses a negative or zero value.
const DEFAULT_RETENTION_MAX_SITES_U64: u64 = 50;

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

/// How often the schedule worker looks for a backup whose time has come: every minute.
pub const DEFAULT_BACKUP_SCHEDULE_POLL_MS: u64 = 60_000;

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
    /// How many tenants one backup sweep may walk (`OMNION_BACKUP_SWUP_MAX_TENANTS`).
    pub backup_sweep_max_tenants: i64,
    /// Delay between two schedule checks (`OMNION_BACKUP_SCHEDULE_POLL_MS`).
    ///
    /// A minute, and for a different reason than the sweep's six hours: the sweep's interval
    /// comes from the feature (retention is measured in days, so a tick that finds nothing
    /// changes no answer), while a schedule's is measured in minutes — an hourly schedule that
    /// fires at :37 because the worker happened to wake at :37 is a schedule the operator did
    /// not write, and every operator notices.
    pub backup_schedule_poll_ms: u64,
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
            backup_schedule_poll_ms: DEFAULT_BACKUP_SCHEDULE_POLL_MS,
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

/// AI Hub knobs (docs/requests/REQ-097, slice 3).
///
/// The probe runner of `apps/api` reads these: every `poll_ms` it runs the **same** connection
/// test the "Probe now" button runs, once per enabled provider, and prunes the history that fell
/// out of the retention window. Turning the runner off (`OMNION_AI_HEALTH_RUNNER=false`) leaves
/// the samples untouched — the Health tab then shows only what an operator probed by hand, which
/// is a real history, just a sparse one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiHubConfig {
    /// Whether this process samples provider health (`OMNION_AI_HEALTH_RUNNER`).
    pub runner_enabled: bool,
    /// Delay between two probe ticks (`OMNION_AI_HEALTH_POLL_MS`).
    pub poll_ms: u64,
    /// How many days of samples and usage rows are kept (`OMNION_AI_HEALTH_RETENTION_DAYS`).
    ///
    /// Unsigned on purpose: a negative retention is not a shorter history, it is a `make_interval`
    /// that deletes everything, and the reader refuses it rather than trusting the spelling.
    pub retention_days: u64,
    /// Whether this process runs the agent runner (`OMNION_AI_RUNNER`).
    ///
    /// A **third** switch rather than a reading of `runner_enabled`, because the health probe,
    /// the decision pruner and the agent runner are three different kinds of background work with
    /// three different risk profiles: the probe makes outbound network calls, the pruner issues a
    /// bulk delete, and the runner **spends money** — it calls a provider and a tool. An
    /// installation that wants its providers probed but refuses to let an agent act on its own
    /// says `OMNION_AI_RUNNER=false`, and the API answers `503 runner_disabled` on a run start
    /// rather than queueing work nothing will ever pick up. A panel that queued runs forever with
    /// no worker would look like a bug in the agent.
    pub agent_runner_enabled: bool,
    /// How many runs one process executes at a time (`OMNION_AI_RUNNER_CONCURRENCY`).
    ///
    /// Default 4, floor 1. Every slot is one in-flight provider call plus one tool, so this is
    /// the number that decides how many tokens can be in flight at once — it is a money knob and
    /// not only a thread-pool knob. A floor of 1 rather than 0 because a concurrency of 0 is a
    /// runner that never runs anything and reports itself healthy.
    pub runner_concurrency: usize,
    /// Whether this process prunes the route decision log (`OMNION_AI_LOG_RUNNER`).
    ///
    /// A **separate** switch from `runner_enabled`, not a second reading of the same one: the
    /// health probe dials providers on the network, and the decision pruner only issues a bulk
    /// delete. An installation that wants to stop outbound probes (a locked-down network, a
    /// cost policy) still wants its log pruned, and an installation that manages retention
    /// with an external job does not want a second one deleting rows underneath it. One env
    /// var for "do not touch my providers" and another for "do not touch my log" is the only
    /// split that serves both.
    pub log_runner_enabled: bool,
    /// Whether this process executes queued eval runs (`OMNION_AI_EVAL_RUNNER`).
    ///
    /// A **fourth** switch, and the split is the same one the three above already draw: this
    /// runner is the second background task that **spends money** — a rubric case costs a second
    /// model call on top of the one under test, so a suite of twenty rubric cases costs twice a
    /// chat turn twenty times. An installation that lets an agent talk but refuses to let an
    /// eval burn a judge budget says `OMNION_AI_EVAL_RUNNER=false`, and the API answers
    /// `503 runner_disabled` on run start rather than queueing runs that will never be picked
    /// up and that the history would show as permanently `queued`.
    ///
    /// It is not a reading of `agent_runner_enabled` on purpose: an eval run is not an agent
    /// run, it has no identity and may call no tools, and tying the two would let an operator
    /// who switched off agent autonomy also switch off the evidence that their agents work —
    /// which is the switch they most need when they are deciding whether to switch it back on.
    pub eval_runner_enabled: bool,
    /// How long a run may stay `running` before the runner fails it (`OMNION_AI_EVAL_TIMEOUT`).
    ///
    /// 900 s by default: long enough for a twenty-case suite where each case is a slow model
    /// turn, short enough that a run whose process died between claim and settle is `error`
    /// before an operator goes looking for it. The unit is seconds and the floor is 30 —
    /// below that a healthy run on a slow provider would be failed by the reaper while it was
    /// still working, which is the one failure this number exists to prevent.
    pub eval_timeout_seconds: i64,
    /// How often the eval runner sweeps for suites whose schedule is due (`OMNION_AI_EVAL_SCHEDULER_MS`).
    ///
    /// 60 s rather than the agent runner's 250 ms tick: a cron schedule has a resolution of a
    /// minute anyway, and a sweep that reads every enabled suite is a query per suite per tick —
    /// cheap once a minute, not four times a second.
    pub eval_scheduler_ms: u64,
}

impl Default for AiHubConfig {
    fn default() -> Self {
        Self {
            runner_enabled: true,
            poll_ms: DEFAULT_AI_HEALTH_POLL_MS,
            retention_days: 30,
            agent_runner_enabled: true,
            runner_concurrency: DEFAULT_AI_RUNNER_CONCURRENCY,
            log_runner_enabled: true,
            eval_runner_enabled: true,
            eval_timeout_seconds: DEFAULT_AI_EVAL_TIMEOUT_SECONDS,
            eval_scheduler_ms: DEFAULT_AI_EVAL_SCHEDULER_MS,
        }
    }
}

/// The installation's Web Push identity (`OMNION_PUSH_*`, REQ-021 slice 6).
///
/// One P-256 key pair per installation, generated at deploy time. The request says it is
/// "generated at deploy time and kept only in the platform secret store", and this is the
/// honest form of that for a platform whose secret store is REQ-125's and not yet built: the
/// key material is read from the environment, **write-only in every rendering**, and the
/// public half is derived from it so the two can never disagree.
///
/// **The private half is a key that can push to every subscribed browser in this
/// installation.** It is `Debug`-redacted for the same reason the SMTP password is: a
/// process that logs its own VAPID private key is a process whose subscribers can be
/// spammed by anybody who can read the log.
///
/// `is_usable` is the whole point of the struct and it is deliberately strict. A push service
/// checks that the JWT's `aud` names the endpoint host and that the signature verifies against
/// the `public_key` the browser was told to trust; a pair where only one half is present, or
/// where the public half does not match the private one, produces a token that is refused on
/// every send. Reporting such an installation as ready is the same green light wired to nothing
/// that the webhook readiness branch was.
#[derive(Clone, PartialEq, Eq)]
pub struct PushConfig {
    /// The P-256 private key, base64url without padding (`OMNION_PUSH_PRIVATE_KEY`).
    ///
    /// Raw scalar bytes when decoded, so the value an operator pastes is the one the
    /// Web Push specification describes and not a DER wrapper around it.
    private_key: Option<String>,
    /// The contact address a push service uses to reach an operator about a failing
    /// subscription (`OMNION_PUSH_CONTACT`), conventionally `mailto:`.
    ///
    /// Not a secret and not optional in the specification: a token whose `sub` is not a
    /// `mailto:` or `https:` URL is rejected outright by some push services.
    contact: Option<String>,
}

impl PushConfig {
    /// The private key, raw 32 bytes, when one is configured and well-formed.
    ///
    /// `None` for absent, for empty, and for a value that is not base64url — the third case
    /// matters because a truncated or padded paste would otherwise be *some* 32 bytes and
    /// produce a key that signs correctly and matches nobody's expectation.
    #[must_use]
    pub fn private_key_bytes(&self) -> Option<Vec<u8>> {
        let raw = self.private_key.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let bytes = crate::base64url::decode(raw)?;
        (bytes.len() == 32).then_some(bytes)
    }

    /// Whether a send could be attempted at all: a usable key **and** a contact address.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.private_key_bytes().is_some() && self.contact().is_some()
    }

    /// The contact address, when it is one a push service accepts.
    ///
    /// The check is on the *scheme* because the specification requires one and because the
    /// alternative is a token rejected at send time, days after the operator believed push
    /// was configured.
    #[must_use]
    pub fn contact(&self) -> Option<&str> {
        let raw = self.contact.as_deref()?.trim();
        let ok = (raw.starts_with("mailto:") || raw.starts_with("https://"))
            && !raw.contains(char::is_whitespace);
        ok.then_some(raw)
    }

    /// Whether a private key was given at all, well-formed or not.
    ///
    /// Separate from [`Self::is_usable`] so the settings screen can tell "no push key was
    /// configured" from "the push key is configured but unusable", which are different fixes.
    #[must_use]
    pub fn has_private_key(&self) -> bool {
        self.private_key
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
    }

    /// A copy with the private key set, base64url without padding.
    ///
    /// **The one writer, and it exists because the fields are private on purpose.** A private
    /// key is a credential, and a struct whose fields can be built from anywhere is a struct
    /// that gets one assembled in a test, a fixture and a config file without anybody deciding
    /// that. But "generated at deploy time" means something has to hand the platform a key it
    /// did not read from the environment: `vapid::VapidKeys::generate` mints one, and this is
    /// how it is handed over.
    ///
    /// No validation happens here, deliberately. A malformed key is still *present*, and
    /// [`Self::has_private_key`] is exactly the distinction the settings screen renders against
    /// [`Self::private_key_bytes`]. Keeping "what was configured" and "what would a push
    /// service accept" as two separate questions is what lets that screen name which of the two
    /// problems it has.
    #[must_use]
    pub fn with_private_key(mut self, private_key: impl Into<String>) -> Self {
        self.private_key = Some(private_key.into());
        self
    }

    /// A copy with the contact address set.
    ///
    /// Same reasoning as [`Self::with_private_key`]; validated on read in [`Self::contact`].
    #[must_use]
    pub fn with_contact(mut self, contact: impl Into<String>) -> Self {
        self.contact = Some(contact.into());
        self
    }

    /// The base64url public key, derived from the private half.
    ///
    /// Derived rather than configured, because a pair supplied as two strings can disagree and
    /// the failure is invisible until every send is refused.
    #[must_use]
    pub fn public_key(&self) -> Option<String> {
        let bytes = self.private_key_bytes()?;
        Some(crate::vapid::public_key_from_private(&bytes)?)
    }
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            private_key: None,
            contact: None,
        }
    }
}

/// Render the push settings without the private key.
impl std::fmt::Debug for PushConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PushConfig")
            .field(
                "private_key",
                &if self.has_private_key() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("contact", &self.contact.as_deref().unwrap_or("<none>"))
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
    /// AI provider health probe knobs (REQ-097).
    pub ai_hub: AiHubConfig,
    /// Retention worker knobs (REQ-010, slice 4).
    pub retention: RetentionConfig,
    /// Email settings of the `send_email` action (P13).
    pub mail: MailConfig,
    /// The installation's Web Push identity (REQ-021, slice 6).
    pub push: PushConfig,
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
            // No `enabled` flag of its own: the schedule worker is the thing an installation
            // with no schedules wants off, and an installation with no schedules pays one
            // grouped query a minute for it. `OMNION_BACKUP_SCHEDULE_POLL_MS` is the lever —
            // set it to an hour and the worker costs nothing measurable, and every schedule
            // still fires within the hour.
            backup_schedule_poll_ms: read_positive(
                &read,
                "OMNION_BACKUP_SCHEDULE_POLL_MS",
                DEFAULT_BACKUP_SCHEDULE_POLL_MS,
            )?,
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

        let ai_hub = AiHubConfig {
            runner_enabled: read_flag(&read, "OMNION_AI_HEALTH_RUNNER", true)?,
            poll_ms: read_positive(&read, "OMNION_AI_HEALTH_POLL_MS", DEFAULT_AI_HEALTH_POLL_MS)?,
            retention_days: read_positive(
                &read,
                "OMNION_AI_HEALTH_RETENTION_DAYS",
                AiHubConfig::default().retention_days,
            )?,
            log_runner_enabled: read_flag(&read, "OMNION_AI_LOG_RUNNER", true)?,
            agent_runner_enabled: read_flag(&read, "OMNION_AI_RUNNER", true)?,
            runner_concurrency: read_positive(
                &read,
                "OMNION_AI_RUNNER_CONCURRENCY",
                DEFAULT_AI_RUNNER_CONCURRENCY as u64,
            )?
            .max(1) as usize,
            eval_runner_enabled: read_flag(&read, "OMNION_AI_EVAL_RUNNER", true)?,
            // The floor of 30 is enforced here rather than trusted from the operator: the number
            // decides when the reaper fails a run, and a value under thirty would fail healthy
            // runs on any provider slower than half a second per case. Clamping is the honest
            // response — refusing to boot an installation over a too-small timeout is a far
            // worse outcome than a timeout that ignores their number.
            eval_timeout_seconds: i64::try_from(read_positive(
                &read,
                "OMNION_AI_EVAL_TIMEOUT",
                DEFAULT_AI_EVAL_TIMEOUT_SECONDS as u64,
            )?)
            .unwrap_or(DEFAULT_AI_EVAL_TIMEOUT_SECONDS)
            .max(30),
            eval_scheduler_ms: read_positive(
                &read,
                "OMNION_AI_EVAL_SCHEDULER_MS",
                DEFAULT_AI_EVAL_SCHEDULER_MS,
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

        // Web Push (REQ-021 slice 6). Read, never generated: a key the platform invents at
        // boot would change on every restart, and every browser holding a subscription to the
        // previous key would be silently unreachable. `generate` exists for the deploy-time
        // step the request describes, and an operator pastes its output into the environment.
        let push = PushConfig {
            private_key: read("OMNION_PUSH_PRIVATE_KEY"),
            contact: read("OMNION_PUSH_CONTACT"),
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
            ai_hub,
            retention,
            mail,
            push,
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
            ai_hub: AiHubConfig::default(),
            retention: RetentionConfig::default(),
            mail: MailConfig::default(),
            push: PushConfig::default(),
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
    use crate::base64url;

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

        // **Push is off by default, and the reason is that it cannot be on by default.**
        // Readiness has to report "no push key is configured" rather than inventing a key at
        // boot: a key the platform mints per process would invalidate every existing browser
        // subscription on every restart.
        assert!(!config.push.has_private_key());
        assert!(!config.push.is_usable());
        assert!(config.push.public_key().is_none());
    }

    /// A 32-byte private key, base64url — the same 0x01..=0x20 scalar `vapid`'s tests use, so
    /// the fixture is one the signing code has already proved is a valid P-256 key.
    const PUSH_KEY: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    /// A contact address the specification accepts.
    const PUSH_CONTACT: &str = "mailto:push@omnion.invalid";

    #[test]
    fn a_configured_push_key_is_usable_and_derives_its_own_public_half() {
        let config = config_from(&[
            ("OMNION_PUSH_PRIVATE_KEY", PUSH_KEY),
            ("OMNION_PUSH_CONTACT", PUSH_CONTACT),
        ])
        .expect("a valid key must load");

        assert!(config.push.has_private_key());
        assert!(config.push.is_usable());
        assert_eq!(
            config.push.public_key().as_deref(),
            crate::vapid::public_key_from_private(&base64url::decode(PUSH_KEY).expect("b64"))
                .as_deref(),
            "the public half is derived, never configured — the two cannot disagree"
        );
        // And the derived key is one a browser can actually subscribe with.
        let point =
            base64url::decode(config.push.public_key().expect("derived").as_str()).expect("b64");
        assert_eq!(point.len(), 65);
        assert_eq!(point[0], 0x04, "the uncompressed SEC1 point tag");
    }

    #[test]
    fn a_malformed_push_key_is_not_a_usable_key_but_is_still_reported_as_present() {
        // The distinction is the whole point of `has_private_key`: "you pasted something that
        // is not a key" and "you pasted nothing" are different fixes, and a readiness screen
        // that says "no push key configured" for the first one sends the operator looking in
        // the wrong place.
        for bad in ["not-base64!", "AAAA", "c2hvcnQ=", "not-a-key-at-all"] {
            let config = config_from(&[
                ("OMNION_PUSH_PRIVATE_KEY", bad),
                ("OMNION_PUSH_CONTACT", PUSH_CONTACT),
            ])
            .expect("a bad value must not stop the process booting");
            assert!(!config.push.is_usable(), "{bad:?} must not be a usable key");
            assert!(
                config.push.has_private_key(),
                "{bad:?} is present but unusable — the operator needs to be told which"
            );
            assert!(config.push.public_key().is_none());
        }

        // **Whitespace is absence, not malformedness.** An environment variable holding
        // spaces is what a templated deployment file produces when the value was never
        // substituted, and reporting "a push key is configured but broken" for that sends the
        // operator to debug a key that does not exist. It reads as nothing configured, which
        // is the truth.
        for blank in ["", "   ", "\t"] {
            let config = config_from(&[
                ("OMNION_PUSH_PRIVATE_KEY", blank),
                ("OMNION_PUSH_CONTACT", PUSH_CONTACT),
            ])
            .expect("loads");
            assert!(
                !config.push.has_private_key(),
                "{blank:?} is an absent key, not a broken one"
            );
            assert!(!config.push.is_usable());
        }
    }

    #[test]
    fn a_key_without_a_contact_is_present_but_not_usable() {
        // A push service rejects a token whose `sub` is not a mailto: or https: URL, so a
        // half-configured installation must not claim readiness.
        let config = config_from(&[("OMNION_PUSH_PRIVATE_KEY", PUSH_KEY)]).expect("loads");
        assert!(config.push.has_private_key());
        assert!(config.push.contact().is_none());
        assert!(!config.push.is_usable());
    }

    #[test]
    fn a_contact_without_a_scheme_is_refused() {
        for bad in [
            "ops@example.com",
            "mailto:ops@example .com",
            "ftp://ops@x.test",
        ] {
            let config = config_from(&[
                ("OMNION_PUSH_PRIVATE_KEY", PUSH_KEY),
                ("OMNION_PUSH_CONTACT", bad),
            ])
            .expect("loads");
            assert!(config.push.contact().is_none(), "{bad:?} must be refused");
            assert!(!config.push.is_usable());
        }
        let good = config_from(&[
            ("OMNION_PUSH_PRIVATE_KEY", PUSH_KEY),
            ("OMNION_PUSH_CONTACT", "https://ops.example.com/push"),
        ])
        .expect("loads");
        assert_eq!(
            good.push.contact(),
            Some("https://ops.example.com/push"),
            "an https contact is as valid as a mailto one"
        );
    }

    #[test]
    fn the_push_private_key_is_never_rendered() {
        // The value that can push to every subscriber must not appear in a log line, a panic
        // message or a test failure. This is the SMTP password's rule, applied to the key that
        // outranks it.
        let config = config_from(&[
            ("OMNION_PUSH_PRIVATE_KEY", PUSH_KEY),
            ("OMNION_PUSH_CONTACT", PUSH_CONTACT),
        ])
        .expect("loads");

        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(PUSH_KEY),
            "the private key reached a Debug rendering: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // The whole config renders, and the key is absent from all of it.
        assert!(!format!("{:?}", config.push).contains(PUSH_KEY));
        // The public half is not a secret — and it is *not* in the Debug output either,
        // because `Config`'s own rendering is a field list that names the push section and
        // nothing more. The browser gets it from the API, which is where it belongs: it is
        // per-installation data, not a boot log.
        assert!(!rendered.contains(&config.push.public_key().expect("derived")));
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
    fn the_ai_health_probe_has_development_defaults_and_is_configurable() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.ai_hub.runner_enabled);
        assert_eq!(config.ai_hub.poll_ms, DEFAULT_AI_HEALTH_POLL_MS);
        assert_eq!(
            config.ai_hub.retention_days,
            AiHubConfig::default().retention_days
        );

        let tuned = config_from(&[
            ("OMNION_AI_HEALTH_RUNNER", "false"),
            ("OMNION_AI_HEALTH_POLL_MS", "5000"),
            ("OMNION_AI_HEALTH_RETENTION_DAYS", "7"),
        ])
        .expect("the AI Hub settings are valid");
        assert!(!tuned.ai_hub.runner_enabled);
        // The decision pruner is a second switch on purpose: disabling the health probe must
        // not silently disable the log retention (or the reverse), which is the mistake a
        // shared env var guarantees somebody will eventually make.
        assert!(config.ai_hub.log_runner_enabled);
        let pruner_off = config_from(&[("OMNION_AI_LOG_RUNNER", "false")])
            .expect("one flag must parse");
        assert!(!pruner_off.ai_hub.log_runner_enabled);
        assert!(
            pruner_off.ai_hub.runner_enabled,
            "switching the decision pruner off must not switch the health probe off"
        );
    }

    #[test]
    fn the_agent_runner_has_its_own_switch_and_its_own_concurrency() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.ai_hub.agent_runner_enabled, "the runner is on by default");
        assert_eq!(
            config.ai_hub.runner_concurrency, DEFAULT_AI_RUNNER_CONCURRENCY,
            "four in-flight runs is the documented default"
        );

        let off = config_from(&[("OMNION_AI_RUNNER", "false")]).expect("one flag must parse");
        assert!(!off.ai_hub.agent_runner_enabled);
        // The reason the switch is its own field and not a reading of the health probe's: an
        // installation that refuses to let an agent spend money must still get its providers
        // probed, and a shared flag takes both away together.
        assert!(
            off.ai_hub.runner_enabled,
            "switching the agent runner off must leave the health probe running"
        );
        assert!(off.ai_hub.log_runner_enabled);

        let narrow = config_from(&[("OMNION_AI_RUNNER_CONCURRENCY", "1")])
            .expect("one number must parse");
        assert_eq!(narrow.ai_hub.runner_concurrency, 1);

        // 0 is refused by the positive reader, and a concurrency of zero is a runner that reports
        // itself healthy and never runs anything.
        let zero = config_from(&[("OMNION_AI_RUNNER_CONCURRENCY", "0")]);
        assert!(
            zero.is_err(),
            "a runner with no slots would report itself healthy and run nothing"
        );
    }

    #[test]
    fn the_eval_runner_has_a_switch_of_its_own_and_a_floored_timeout() {
        let config = config_from(&[]).expect("defaults must load");
        assert!(config.ai_hub.eval_runner_enabled, "the runner is on by default");
        assert_eq!(
            config.ai_hub.eval_timeout_seconds, DEFAULT_AI_EVAL_TIMEOUT_SECONDS,
            "fifteen minutes is the documented default"
        );
        assert_eq!(
            config.ai_hub.eval_scheduler_ms, DEFAULT_AI_EVAL_SCHEDULER_MS,
            "a schedule is swept once a minute"
        );

        // The reason this is a fourth switch rather than a reading of the agent runner's: an
        // eval run is not an agent run — it has no identity and may call no tools — and the
        // operator who switched off agent autonomy still needs the evidence that their agents
        // work, which is the thing they most want when deciding whether to switch it back on.
        let off = config_from(&[("OMNION_AI_EVAL_RUNNER", "false")]).expect("one flag must parse");
        assert!(!off.ai_hub.eval_runner_enabled);
        assert!(
            off.ai_hub.agent_runner_enabled,
            "switching the eval runner off must leave agent runs working"
        );
        assert!(
            off.ai_hub.runner_enabled,
            "switching the eval runner off must leave the health probe working"
        );
        assert!(
            config_from(&[("OMNION_AI_RUNNER", "false")])
            .expect("one flag must parse")
            .ai_hub
            .eval_runner_enabled,
            "switching the agent runner off must leave eval runs working"
        );

        // A timeout under thirty seconds would fail healthy runs on any provider slower than
        // half a second a case. It is clamped rather than refused: booting is not the operator's
        // way to learn their number was too small, and a silent, documented floor is.
        let tight = config_from(&[("OMNION_AI_EVAL_TIMEOUT", "5")]).expect("a number must parse");
        assert_eq!(
            tight.ai_hub.eval_timeout_seconds, 30,
            "a five-second timeout is clamped to the floor, not honoured"
        );

        let wide = config_from(&[("OMNION_AI_EVAL_TIMEOUT", "1800")]).expect("a number must parse");
        assert_eq!(wide.ai_hub.eval_timeout_seconds, 1800, "a real value is kept");

        // 0 is refused by the positive reader: a reaper with a zero window fails the run it is
        // asked to rescue, on the same statement that finds it.
        assert!(
            config_from(&[("OMNION_AI_EVAL_TIMEOUT", "0")]).is_err(),
            "a zero timeout window would fail every run the sweep found"
        );
        assert!(
            config_from(&[("OMNION_AI_EVAL_SCHEDULER_MS", "0")]).is_err(),
            "a zero scheduler interval would spin the sweep against every suite"
        );
    }

    #[test]
    fn the_probe_interval_may_not_be_zero() {
        // A zero interval would spin the runner against every provider as fast as the box
        // allows; the read helper refuses it rather than letting a typo become a self-inflicted
        // denial of service on the operator's own API keys.
        let error = config_from(&[("OMNION_AI_HEALTH_POLL_MS", "0")])
            .expect_err("a zero probe interval must be refused");
        assert!(
            error.to_string().contains("OMNION_AI_HEALTH_POLL_MS"),
            "got {error}"
        );
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
