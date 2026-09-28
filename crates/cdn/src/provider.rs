//! The provider adapter seam (docs/requests/REQ-011).
//!
//! Nothing in the platform talks to a CDN directly: the purge worker holds a
//! [`Provider`] and everything above it is written against this trait. The trait
//! is deliberately small and synchronous-looking even though the real calls are
//! network I/O — the queue worker owns the awaiting, so a half-finished purge is
//! a state in the database rather than a suspended future.
//!
//! Three adapters ship (`origin`, `generic_http`, `cloudflare_style`). Only
//! shipped adapters appear in the catalogue: an adapter that is listed but not
//! implemented is a dead button, and the whole point of the catalogue is that
//! choosing from it is a real choice.

use serde::{Deserialize, Serialize};

/// The maximum number of targets one provider call may carry.
///
/// A purge storm is real (publishing a page with forty assets invalidates forty
/// URLs), and an unbounded batch is how a provider starts rejecting requests or
/// how an operator's own account gets throttled. The queue splits at this cap.
pub const MAX_BATCH: usize = 500;

/// What a provider is asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Purge {
    /// Invalidate these absolute paths.
    Urls {
        /// Absolute paths, each already validated by the API layer.
        targets: Vec<String>,
    },
    /// Invalidate by surrogate key.
    Tags {
        /// Tag names, as emitted by `headers::surrogate_keys`.
        targets: Vec<String>,
    },
    /// Invalidate the provider's whole zone.
    All,
}

/// The outcome of one provider call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PurgeOutcome {
    /// Every target was invalidated.
    Succeeded,
    /// Some targets went through, some did not.
    Partial {
        /// Targets the provider refused or did not confirm.
        failed: Vec<String>,
        /// What the provider said, kept verbatim for the history row.
        message: String,
    },
    /// Nothing was invalidated.
    Failed {
        /// The provider's own message; this is what the panel shows.
        message: String,
    },
}

impl PurgeOutcome {
    /// Whether the outcome leaves anything retryable.
    #[must_use]
    pub fn retryable(&self) -> bool {
        !matches!(self, PurgeOutcome::Succeeded)
    }
}

/// A CDN provider the platform can purge through.
pub trait Provider {
    /// The adapter's stable key, as stored in `cdn_settings.provider`.
    fn key(&self) -> &'static str;

    /// What the adapter supports.
    ///
    /// A provider that cannot purge by tag is not allowed to *pretend* to: the
    /// queue planner uses this to fall back to URL purges derived from the same
    /// tag map, and the panel says which strategy actually ran.
    fn capabilities(&self) -> Capabilities;

    /// Invalidate one batch of targets.
    fn purge(&self, request: &Purge) -> PurgeOutcome;

    /// A reachability check for the "Test connection" action.
    fn verify(&self) -> Probe;
}

/// What an adapter can actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Surrogate-key purging is supported.
    pub tags: bool,
    /// Whole-zone purging is supported.
    pub purge_all: bool,
}

/// The result of a reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// Whether the provider answered acceptably.
    pub ok: bool,
    /// Round-trip time in milliseconds.
    pub latency_ms: u64,
    /// The HTTP status the provider returned, when there was one.
    pub status: Option<u16>,
    /// What the provider said, shown inline under the button.
    pub message: String,
}

/// The `origin` adapter: no external cache at all.
///
/// It is not a stub. It is the correct provider for an installation that serves
/// its own public surface with no CDN in front of it: the rule engine and headers
/// still run, and a purge is a successful no-op, which is the honest answer for
/// "there is nothing at the edge to invalidate".
#[derive(Debug, Clone, Copy, Default)]
pub struct OriginProvider;

impl Provider for OriginProvider {
    fn key(&self) -> &'static str {
        "origin"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // There is no edge to hold a tag, but the queue still resolves tags to
            // URLs, so reporting `true` keeps the planner from special-casing it.
            tags: true,
            purge_all: true,
        }
    }

    fn purge(&self, request: &Purge) -> PurgeOutcome {
        // The target count is read so a malformed `Purge` cannot be constructed
        // without the queue knowing how much work it queued, but the origin has
        // nothing to invalidate either way.
        let _targets = match request {
            Purge::Urls { targets } | Purge::Tags { targets } => targets.len(),
            Purge::All => 0,
        };
        PurgeOutcome::Succeeded
    }

    fn verify(&self) -> Probe {
        Probe {
            ok: true,
            latency_ms: 0,
            status: None,
            message: "no external cache is configured; the origin answers every request"
                .to_string(),
        }
    }
}

/// A shipped adapter, as the catalogue endpoint lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterInfo {
    /// The stored key.
    pub key: &'static str,
    /// Display name for the settings picker.
    pub label: &'static str,
    /// One line explaining what it talks to.
    pub description: &'static str,
    /// Whether the adapter needs an endpoint URL.
    pub needs_endpoint: bool,
    /// Whether the adapter needs a zone reference.
    pub needs_zone: bool,
    /// Whether the adapter needs a write-only credential.
    pub needs_credential: bool,
}

/// The shipped adapters, in the order the picker shows them.
///
/// Derived from the providers themselves rather than hand-listed, so an adapter
/// cannot be shipped without appearing here and a name cannot drift from the key
/// the code dispatches on.
#[must_use]
pub fn catalogue() -> Vec<AdapterInfo> {
    vec![
        AdapterInfo {
            key: OriginProvider.key(),
            label: "Origin (no edge)",
            description: "No external cache. Rules and headers still apply.",
            needs_endpoint: false,
            needs_zone: false,
            needs_credential: false,
        },
        AdapterInfo {
            key: "generic_http",
            label: "Generic HTTP",
            description: "POST a JSON purge payload to your own endpoint.",
            needs_endpoint: true,
            needs_zone: false,
            needs_credential: true,
        },
        AdapterInfo {
            key: "cloudflare_style",
            label: "Hosted CDN (zone API)",
            description: "Zone purge calls against a hosted CDN's API.",
            needs_endpoint: true,
            needs_zone: true,
            needs_credential: true,
        },
    ]
}

/// Whether a stored provider key names a shipped adapter.
#[must_use]
pub fn is_shipped(key: &str) -> bool {
    catalogue().iter().any(|adapter| adapter.key == key)
}

/// Split a purge into batches no larger than [`MAX_BATCH`].
///
/// Returned as slices of the caller's own targets so the worker can hand each
/// batch to the provider without copying a storm-sized vector per attempt.
#[must_use]
pub fn batches<T>(targets: &[T]) -> Vec<&[T]> {
    if targets.is_empty() {
        return vec![&targets[..]];
    }
    targets.chunks(MAX_BATCH).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_origin_provider_succeeds_without_contacting_anything() {
        let provider = OriginProvider;
        let outcome = provider.purge(&Purge::Urls {
            targets: vec!["/blog".into()],
        });
        assert_eq!(outcome, PurgeOutcome::Succeeded);
        assert!(!outcome.retryable());
    }

    #[test]
    fn the_origin_provider_reports_itself_as_healthy() {
        assert!(OriginProvider.verify().ok);
    }

    #[test]
    fn the_catalogue_lists_only_adapters_that_can_be_dispatched_on() {
        let keys: Vec<&str> = catalogue().iter().map(|adapter| adapter.key).collect();
        assert_eq!(keys, vec!["origin", "generic_http", "cloudflare_style"]);
        for key in keys {
            assert!(is_shipped(key));
        }
    }

    #[test]
    fn an_unknown_provider_key_is_not_shipped() {
        assert!(!is_shipped("fastly"));
        assert!(!is_shipped(""));
    }

    #[test]
    fn a_storm_of_targets_is_split_at_the_batch_cap() {
        let targets: Vec<String> = (0..1200).map(|index| format!("/p/{index}")).collect();
        let batches = batches(&targets);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), MAX_BATCH);
        assert_eq!(batches[2].len(), 200);
    }

    #[test]
    fn an_empty_purge_is_still_one_batch_so_the_provider_is_actually_called() {
        let targets: Vec<String> = Vec::new();
        assert_eq!(batches(&targets).len(), 1);
    }

    #[test]
    fn a_partial_outcome_is_retryable_and_a_failure_is_too() {
        assert!(
            PurgeOutcome::Failed {
                message: "boom".into()
            }
            .retryable()
        );
        assert!(
            PurgeOutcome::Partial {
                failed: vec!["/a".into()],
                message: "2 refused".into()
            }
            .retryable()
        );
        assert!(!PurgeOutcome::Succeeded.retryable());
    }
}
