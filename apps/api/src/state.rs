//! Shared application state handed to every route.
//!
//! Later phases extend this with the database pool, Redis client and typed configuration;
//! P00 only carries build metadata so the wiring is already in place.

use std::sync::Arc;

use omnion_core::BuildInfo;

/// Cheap-to-clone handle shared by all HTTP handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
struct AppStateInner {
    build: BuildInfo,
}

impl AppState {
    /// Create state for the named service using this binary's own version.
    #[must_use]
    pub fn new(service: &'static str) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                build: BuildInfo::new(service, env!("CARGO_PKG_VERSION")),
            }),
        }
    }

    /// Build metadata of the running service.
    #[must_use]
    pub fn build(&self) -> BuildInfo {
        self.inner.build
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new("omnion-api")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_reports_api_build_info() {
        let state = AppState::new("omnion-api");
        assert_eq!(state.build().service, "omnion-api");
        assert!(!state.build().version.is_empty());
    }
}
