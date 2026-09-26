//! Shared application state handed to every route.

use std::sync::Arc;

use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_storage::{Storage, StorageConfig};

/// Cheap-to-clone handle shared by all HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
struct AppStateInner {
    build: BuildInfo,
    config: Config,
    db: Db,
    redis: RedisClient,
    storage: Storage,
}

impl AppState {
    /// Build state for a running service.
    #[must_use]
    pub fn new(
        build: BuildInfo,
        config: Config,
        db: Db,
        redis: RedisClient,
        storage: Storage,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                build,
                config,
                db,
                redis,
                storage,
            }),
        }
    }

    /// Build metadata of the running service.
    #[must_use]
    pub fn build(&self) -> BuildInfo {
        self.inner.build
    }

    /// Validated runtime configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// PostgreSQL pool.
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    /// Redis handle.
    #[must_use]
    pub fn redis(&self) -> &RedisClient {
        &self.inner.redis
    }

    /// Object store the media library writes through.
    #[must_use]
    pub fn storage(&self) -> &Storage {
        &self.inner.storage
    }
}

impl Default for AppState {
    /// State built from [`Config::default`] with unconnected (lazy) infrastructure handles.
    ///
    /// Used by tests and local harnesses — `/healthz` and router-level tests work without a
    /// database. The binary always builds real, connected state in `main.rs`; the object store
    /// does not connect at construction time either, so the default driver is harmless here.
    fn default() -> Self {
        let config = Config::default();
        let db = Db::connect_lazy(&config.database).expect("the default database URL is valid");
        let redis = RedisClient::new(&config.redis.url).expect("the default redis URL is valid");
        let storage = Storage::from_config(&StorageConfig::default())
            .expect("the default storage configuration is valid");
        Self::new(
            BuildInfo::new("omnion-api", env!("CARGO_PKG_VERSION")),
            config,
            db,
            redis,
            storage,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_exposes_build_config_and_handles() {
        let state = AppState::default();
        assert_eq!(state.build().service, "omnion-api");
        assert!(!state.build().version.is_empty());
        assert_eq!(
            state.config().env,
            omnion_core::config::Environment::Development
        );
        assert_eq!(state.db().pool().size(), 0, "default state must stay lazy");
        assert_eq!(
            state.storage().describe(),
            "s3://omnion-media",
            "the default object store is the development bucket"
        );
    }
}
