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
        }
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
        };

        let config = Self {
            env,
            http: HttpConfig { host, port },
            database,
            redis,
            admin,
            workflows,
            events,
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
}
