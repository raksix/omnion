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

        let config = Self {
            env,
            http: HttpConfig { host, port },
            database,
            redis,
            admin,
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
            log: LogConfig::new(DEFAULT_LOG_FILTER, LogFormat::Pretty),
        }
    }
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
}
