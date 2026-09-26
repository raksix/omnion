//! Omnion core.
//!
//! Infrastructure-only base crate shared by every Omnion application (see
//! `docs/04-MONOREPO.md`). Business features do **not** live here: they are built as
//! toggleable modules on top of the core, so the core stays thin and stable.
//!
//! P01 adds the shared infrastructure primitives on top of the P00 build metadata:
//! typed configuration ([`config`]), structured logging ([`telemetry`]), the PostgreSQL
//! pool and migration runner ([`db`]), the Redis handle ([`redis_client`]) and the shared
//! error types ([`error`]).

#![forbid(unsafe_code)]

pub mod config;
pub mod db;
pub mod error;
pub mod redis_client;
pub mod telemetry;

pub use config::{Config, WorkflowConfig};
pub use db::Db;
pub use error::{ConfigError, CoreError, Result};
pub use redis_client::RedisClient;
pub use telemetry::Telemetry;

/// Crate name of the Omnion core, as declared in `Cargo.toml`.
pub const CORE_NAME: &str = env!("CARGO_PKG_NAME");

/// Version of the Omnion core, as declared in `Cargo.toml`.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build metadata for a running Omnion service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// Service identifier, e.g. `omnion-api`.
    pub service: &'static str,
    /// Running version of that service (SemVer).
    pub version: &'static str,
}

impl BuildInfo {
    /// Build metadata for the given service at the given version.
    #[must_use]
    pub const fn new(service: &'static str, version: &'static str) -> Self {
        Self { service, version }
    }

    /// Build metadata of the core crate itself.
    #[must_use]
    pub const fn core() -> Self {
        Self::new(CORE_NAME, CORE_VERSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_build_info_reports_crate_metadata() {
        let info = BuildInfo::core();
        assert_eq!(info.service, "omnion-core");
        assert!(!info.version.is_empty());
    }
}
