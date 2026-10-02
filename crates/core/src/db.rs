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

    /// Migration bookkeeping: what the binary embeds, what the database has applied.
    ///
    /// `omnion doctor` reports it and `omnion migrate` prints it before and after, so an
    /// operator can see whether a deployment actually carried its schema with it. A database
    /// that has never been migrated has no `_sqlx_migrations` table yet — that is "nothing
    /// applied", not an error.
    pub async fn migration_status(&self) -> Result<MigrationStatus> {
        let applied: Vec<i64> = match sqlx::query_scalar::<_, i64>(
            "select version from _sqlx_migrations where success order by version",
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(versions) => versions,
            Err(err) if is_undefined_table(&err) => Vec::new(),
            Err(err) => return Err(CoreError::Database(err)),
        };

        let pending = MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .filter(|version| !applied.contains(version))
            .collect();

        Ok(MigrationStatus { applied, pending })
    }
}

/// Migration state of one database, against the migrations this binary embeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// Versions recorded as successfully applied.
    pub applied: Vec<i64>,
    /// Versions this binary embeds that the database does not have yet.
    pub pending: Vec<i64>,
}

impl MigrationStatus {
    /// Number of migrations this binary embeds.
    #[must_use]
    pub fn total(&self) -> usize {
        MIGRATOR.iter().count()
    }

    /// `true` when the database has every embedded migration.
    #[must_use]
    pub fn is_up_to_date(&self) -> bool {
        self.pending.is_empty()
    }
}

/// `true` when PostgreSQL answered `undefined_table` (`42P01`).
fn is_undefined_table(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "42P01")
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

    #[test]
    fn migration_status_knows_the_embedded_migrations() {
        let status = MigrationStatus {
            applied: Vec::new(),
            pending: Vec::new(),
        };
        assert!(
            status.total() >= 7,
            "the binary embeds at least the seven released migrations, got {}",
            status.total()
        );
        assert!(status.is_up_to_date());

        let behind = MigrationStatus {
            applied: Vec::new(),
            pending: vec![7],
        };
        assert!(!behind.is_up_to_date());
    }
}
