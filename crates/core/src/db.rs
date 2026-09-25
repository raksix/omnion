//! PostgreSQL access and the migration runner.
//!
//! The pool and the migrations live in the core so every Omnion binary (API, workers, CLI)
//! connects the same way and applies the same schema (docs/02-ARCHITECTURE.md).

use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::config::DatabaseConfig;
use crate::error::{CoreError, Result};

/// Migrations are embedded at build time, so any binary can migrate its own database
/// without shipping the SQL files next to it (docs/04-MONOREPO.md: `database/migrations`).
static MIGRATOR: Migrator = sqlx::migrate!("../../database/migrations");

/// Upper bound for a health ping so readiness probes answer quickly.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a caller waits for a free pooled connection before giving up.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Thin wrapper around the shared PostgreSQL pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect to the configured database and verify the pool.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        let pool = pool_options(config).connect(&config.url).await?;
        Ok(Self { pool })
    }

    /// Build a pool without touching the network — the first query connects.
    pub fn connect_lazy(config: &DatabaseConfig) -> Result<Self> {
        let pool = pool_options(config).connect_lazy(&config.url)?;
        Ok(Self { pool })
    }

    /// Apply every pending migration.
    ///
    /// Idempotent and safe to run from several instances at once: SQLx takes an advisory
    /// lock and records applied versions in `_sqlx_migrations`.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Cheap round-trip used by `/readyz`, bounded by [`PING_TIMEOUT`].
    pub async fn ping(&self) -> Result<()> {
        match tokio::time::timeout(PING_TIMEOUT, sqlx::query("select 1").fetch_one(&self.pool))
            .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(CoreError::Database(err)),
            Err(_) => Err(CoreError::Unavailable {
                dependency: "database",
                message: format!("`select 1` did not answer within {PING_TIMEOUT:?}"),
            }),
        }
    }

    /// The underlying pool, for feature crates that run their own queries.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn pool_options(config: &DatabaseConfig) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(ACQUIRE_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn lazy_pool_opens_no_connections() {
        let config = Config::default();
        let db = Db::connect_lazy(&config.database).expect("default URL must parse");
        assert_eq!(db.pool().size(), 0, "lazy pool must not connect up front");
    }

    #[test]
    fn malformed_database_url_is_rejected() {
        let mut config = Config::default();
        config.database.url = "not-a-database".to_owned();
        assert!(Db::connect_lazy(&config.database).is_err());
    }
}
