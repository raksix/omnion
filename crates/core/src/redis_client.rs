//! Redis handle for cache, queues and short-lived coordination state.
//!
//! The connection is created on first use and re-established by the client itself, so the
//! API can boot — and report itself *not ready* — while Redis is down, then recover without
//! a restart (readiness probes, docs/02-ARCHITECTURE.md).

use std::sync::Arc;
use std::time::Duration;

use redis::aio::ConnectionManager;
use tokio::sync::Mutex;

use crate::error::{CoreError, Result};

/// Upper bound for a health ping so readiness probes answer quickly.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Lazily connected Redis handle shared by every request.
#[derive(Debug, Clone)]
pub struct RedisClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    client: redis::Client,
    manager: Mutex<Option<ConnectionManager>>,
}

impl RedisClient {
    /// Build a handle for `url`; no connection is opened yet.
    pub fn new(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                manager: Mutex::new(None),
            }),
        })
    }

    /// `PING` round-trip used by `/readyz`, bounded by [`PING_TIMEOUT`].
    pub async fn ping(&self) -> Result<()> {
        let ping = async {
            let mut connection = self.connection().await?;
            let pong: String = redis::cmd("PING").query_async(&mut connection).await?;
            Ok::<String, CoreError>(pong)
        };

        match tokio::time::timeout(PING_TIMEOUT, ping).await {
            Ok(Ok(pong)) if pong == "PONG" => Ok(()),
            Ok(Ok(pong)) => Err(CoreError::Unavailable {
                dependency: "redis",
                message: format!("unexpected PING reply {pong:?}"),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(CoreError::Unavailable {
                dependency: "redis",
                message: format!("PING did not answer within {PING_TIMEOUT:?}"),
            }),
        }
    }

    /// A cloneable connection manager, connecting on first use.
    ///
    /// A failed connect is not cached: the next caller tries again.
    pub async fn connection(&self) -> Result<ConnectionManager> {
        let mut guard = self.inner.manager.lock().await;
        if let Some(manager) = guard.as_ref() {
            return Ok(manager.clone());
        }
        let manager = ConnectionManager::new(self.inner.client.clone()).await?;
        *guard = Some(manager.clone());
        Ok(manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_construction_does_not_connect() {
        // Port 6399 has nothing behind it; constructing the handle must still succeed.
        assert!(RedisClient::new("redis://127.0.0.1:6399").is_ok());
    }

    #[test]
    fn unsupported_scheme_is_rejected() {
        assert!(RedisClient::new("http://127.0.0.1:6379").is_err());
    }
}
