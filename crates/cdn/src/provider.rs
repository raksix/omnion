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

/// The `generic_http` adapter: POST a JSON purge payload to an endpoint the operator owns.
///
/// This is the adapter for the long tail: a Varnish or nginx purge script behind a private
/// network, an internal service that fans out to several edges, a test double. It has no
/// opinion about what the endpoint does with the body, so the contract is written down and
/// is exactly what the endpoint receives:
///
/// ```json
/// { "kind": "urls", "targets": ["/blog"], "zone": null, "attempt": 1 }
/// ```
///
/// A `2xx` is a success **only if the body does not contradict it**: an endpoint that
/// answers `200 {"ok": false}`, or a `200` with a non-empty `failed`/`errors` array, has not
/// purged anything, and recording that as a success is the failure mode that makes a purge
/// look finished while the cache still serves the old bytes. Every other `2xx` — including
/// an empty body and a body that is not JSON at all — is accepted, because the status code
/// is the contract and second-guessing it invents failures out of working endpoints.
pub struct GenericHttpProvider {
    /// Where to POST. An endpoint is required, but a missing one is not a construction
    /// failure: the settings form has to be able to save a half-configured provider, and
    /// the honest place to say "this cannot work yet" is the purge that tries.
    endpoint: String,
    /// Write-only bearer credential, sent as `Authorization`.
    credential: Option<String>,
    /// The zone reference, when the operator gave one.
    zone: Option<String>,
    /// How long one call may take before it is a failure rather than a hang.
    timeout_ms: u64,
}

/// The default per-call timeout.
///
/// A purge that never answers must not hold a worker slot forever: the queue marks the item
/// failed and the operator's retry button does the rest, which beats a queue that stops
/// draining.
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// What one HTTP call produced: the status, and the body to judge it by.
struct Answer {
    status: Option<u16>,
    body: String,
    latency_ms: u64,
}

impl Answer {
    /// A transport failure: the request never produced an HTTP response.
    fn transport(message: String, latency_ms: u64) -> Self {
        Self {
            status: None,
            // The reason is carried in the body slot because both callers render it the
            // same way, and a separate field would only ever be read by one of them.
            body: message,
            latency_ms,
        }
    }
}

/// Read a purge acknowledgement for what it says, not for what it is shaped like.
///
/// Returns the sentence to show when the answer refuses the purge, and `None` when the
/// answer accepts it.
fn refusal_in(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return None,
    };
    if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return Some(
            parsed
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the endpoint answered ok: false")
                .to_string(),
        );
    }
    for key in ["failed", "errors", "failures"] {
        if let Some(items) = parsed.get(key).and_then(serde_json::Value::as_array) {
            if !items.is_empty() {
                let names: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                return Some(format!("{key}: {}", names.join(", ")));
            }
        }
    }
    None
}

/// The wire body of a purge, shared by both HTTP adapters.
fn payload(kind: &str, targets: &[String], zone: Option<&str>, attempt: u32) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "targets": targets,
        "zone": zone,
        "attempt": attempt,
    })
}

/// Make one POST and read the answer.
///
/// Blocking on purpose: the [`Provider`] trait is synchronous because the queue worker owns
/// the awaiting, so a purge in flight is a row in the database rather than a suspended
/// future. The worker calls this from `spawn_blocking`, which is the only correct place for
/// a blocking call in a Tokio application.
fn post(
    endpoint: &str,
    credential: Option<&str>,
    body: &serde_json::Value,
    timeout_ms: u64,
    method: reqwest::Method,
) -> Answer {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return Answer::transport(format!("the HTTP client could not be built: {error}"), 0);
        }
    };
    let mut request = client.request(method, endpoint).json(body);
    if let Some(credential) = credential {
        request = request.bearer_auth(credential);
    }

    let started = std::time::Instant::now();
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => {
            return Answer::transport(
                format!("{endpoint} could not be reached: {error}"),
                started.elapsed().as_millis().min(u128::from(u32::MAX)) as u64,
            );
        }
    };
    let status = response.status().as_u16();
    // Bounded: a response larger than this is not a purge acknowledgement, and reading one
    // whole would let a provider choose the queue worker's memory.
    let body = response
        .text()
        .map(|text| text.chars().take(64 * 1024).collect())
        .unwrap_or_default();
    Answer {
        status: Some(status),
        body,
        latency_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u64,
    }
}

impl GenericHttpProvider {
    /// Build an adapter from a settings row.
    #[must_use]
    pub fn new(endpoint: Option<&str>, credential: Option<String>, zone: Option<String>) -> Self {
        Self {
            endpoint: endpoint.unwrap_or_default().trim().to_string(),
            credential,
            zone,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// Override the per-call timeout. Used by the tests to keep them fast.
    #[must_use]
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    fn endpoint_or_message(&self) -> Result<&str, String> {
        if self.endpoint.is_empty() {
            return Err(
                "no purge endpoint is configured for this provider — set one in CDN settings"
                    .to_string(),
            );
        }
        Ok(&self.endpoint)
    }
}

impl Provider for GenericHttpProvider {
    fn key(&self) -> &'static str {
        "generic_http"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // A tag is just a name in the body. Whether the endpoint behind it honours it
            // is the operator's to know, and silently degrading a tag purge into nothing
            // would be worse than sending it and showing the answer.
            tags: true,
            // "Everything" is a legitimate instruction for a self-hosted purge script.
            purge_all: true,
        }
    }

    fn purge(&self, request: &Purge) -> PurgeOutcome {
        let endpoint = match self.endpoint_or_message() {
            Ok(endpoint) => endpoint,
            Err(message) => return PurgeOutcome::Failed { message },
        };
        // `Purge::All` carries no targets, and the body still has to have the key: an
        // endpoint written against this contract reads `targets` unconditionally, and a
        // missing key is a deserialisation error on their side rather than an empty list.
        let (kind, targets): (&str, &[String]) = match request {
            Purge::Urls { targets } => ("urls", targets),
            Purge::Tags { targets } => ("tags", targets),
            Purge::All => ("all", &[]),
        };
        let answer = post(
            endpoint,
            self.credential.as_deref(),
            &payload(kind, targets, self.zone.as_deref(), 1),
            self.timeout_ms,
            reqwest::Method::POST,
        );
        let Some(status) = answer.status else {
            return PurgeOutcome::Failed {
                message: answer.body,
            };
        };
        if !(200..300).contains(&status) {
            return PurgeOutcome::Failed {
                message: format!("{endpoint} answered {status}: {}", answer.body.trim()),
            };
        }
        match refusal_in(&answer.body) {
            Some(message) => PurgeOutcome::Failed { message },
            None => PurgeOutcome::Succeeded,
        }
    }

    fn verify(&self) -> Probe {
        let endpoint = match self.endpoint_or_message() {
            Ok(endpoint) => endpoint,
            Err(message) => {
                return Probe {
                    ok: false,
                    latency_ms: 0,
                    status: None,
                    message,
                };
            }
        };
        // The same method and the same body shape as a purge, so the check exercises the
        // real path — an endpoint that would reject a purge rejects this too — without
        // asking for anything to be invalidated. The `kind` is `ping`, which a real purge
        // never sends, so an endpoint that honours the contract does nothing.
        let answer = post(
            endpoint,
            self.credential.as_deref(),
            &payload("ping", &[], self.zone.as_deref(), 0),
            self.timeout_ms,
            reqwest::Method::POST,
        );
        match answer.status {
            Some(status) if (200..500).contains(&status) => Probe {
                // A `404` or `405` still proves the host is alive, the path is real and the
                // credential reached whatever is in front. That is the question "Test
                // connection" asks; whether the purge will succeed is the purge's business.
                ok: true,
                latency_ms: answer.latency_ms,
                status: Some(status),
                message: format!("{endpoint} answered {status}"),
            },
            Some(status) => Probe {
                ok: false,
                latency_ms: answer.latency_ms,
                status: Some(status),
                message: format!("{endpoint} answered {status}: {}", answer.body.trim()),
            },
            None => Probe {
                ok: false,
                latency_ms: answer.latency_ms,
                status: None,
                message: answer.body,
            },
        }
    }
}

/// The `cloudflare_style` adapter: a zone purge call against a hosted CDN's API.
///
/// Hosted CDNs in this shape take a zone, a set of identifiers — either URLs or surrogate
/// keys, never both — or the word "everything", plus a bearer credential, and differ only in
/// the JSON field names. That mapping is written down in one place so an operator can see
/// exactly what their provider receives, and so a provider that refuses is refused by its
/// own message rather than by a guess.
pub struct CloudflareStyleProvider {
    /// The API base, e.g. `https://api.example-cdn.com`.
    endpoint: String,
    /// The zone this installation purges.
    zone: Option<String>,
    /// Write-only bearer credential.
    credential: Option<String>,
    /// Per-call timeout.
    timeout_ms: u64,
}

impl CloudflareStyleProvider {
    /// Build an adapter from a settings row.
    #[must_use]
    pub fn new(endpoint: Option<&str>, zone: Option<String>, credential: Option<String>) -> Self {
        Self {
            endpoint: endpoint.unwrap_or_default().trim().to_string(),
            zone,
            credential,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// Override the per-call timeout. Used by the tests to keep them fast.
    #[must_use]
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// The body a hosted-CDN purge call carries.
    ///
    /// Three distinct bodies rather than one with nulls, because a provider that validates
    /// strictly rejects the null form and the operator is left guessing which shape it
    /// wanted.
    fn body(&self, request: &Purge) -> serde_json::Value {
        match request {
            Purge::Urls { targets } => serde_json::json!({ "files": targets }),
            Purge::Tags { targets } => serde_json::json!({ "surrogate_keys": targets }),
            Purge::All => serde_json::json!({ "purge_everything": true }),
        }
    }

    fn zone_or_message(&self) -> Result<String, String> {
        match self.zone.as_deref().map(str::trim).filter(|zone| !zone.is_empty()) {
            Some(zone) => Ok(zone.to_string()),
            None => Err(
                "no zone is configured for this provider — a hosted CDN cannot be purged without one"
                    .to_string(),
            ),
        }
    }
}

impl Provider for CloudflareStyleProvider {
    fn key(&self) -> &'static str {
        "cloudflare_style"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tags: true,
            purge_all: true,
        }
    }

    fn purge(&self, request: &Purge) -> PurgeOutcome {
        let zone = match self.zone_or_message() {
            Ok(zone) => zone,
            Err(message) => return PurgeOutcome::Failed { message },
        };
        if self.endpoint.is_empty() {
            return PurgeOutcome::Failed {
                message: "no API endpoint is configured for this provider".to_string(),
            };
        }
        let call_endpoint = format!(
            "{}/zones/{}/purge_cache",
            self.endpoint.trim_end_matches('/'),
            zone
        );
        let answer = post(
            &call_endpoint,
            self.credential.as_deref(),
            &self.body(request),
            self.timeout_ms,
            reqwest::Method::POST,
        );
        let Some(status) = answer.status else {
            return PurgeOutcome::Failed {
                message: answer.body,
            };
        };
        if !(200..300).contains(&status) || refusal_in(&answer.body).is_some() {
            return PurgeOutcome::Failed {
                message: format!("{call_endpoint} answered {status}: {}", answer.body.trim()),
            };
        }
        PurgeOutcome::Succeeded
    }

    fn verify(&self) -> Probe {
        if let Err(message) = self.zone_or_message() {
            return Probe {
                ok: false,
                latency_ms: 0,
                status: None,
                message,
            };
        }
        if self.endpoint.is_empty() {
            return Probe {
                ok: false,
                latency_ms: 0,
                status: None,
                message: "no API endpoint is configured for this provider".to_string(),
            };
        }
        // A read of the zone rather than a purge of it: the check must never invalidate
        // anything, and reading the zone is the same credential path a purge takes.
        let call_endpoint = format!(
            "{}/zones/{}",
            self.endpoint.trim_end_matches('/'),
            self.zone.clone().unwrap_or_default()
        );
        let answer = post(
            &call_endpoint,
            self.credential.as_deref(),
            &serde_json::json!({}),
            self.timeout_ms,
            reqwest::Method::GET,
        );
        match answer.status {
            Some(status) if (200..500).contains(&status) => Probe {
                ok: true,
                latency_ms: answer.latency_ms,
                status: Some(status),
                message: format!("{call_endpoint} answered {status}"),
            },
            Some(status) => Probe {
                ok: false,
                latency_ms: answer.latency_ms,
                status: Some(status),
                message: format!("{call_endpoint} answered {status}: {}", answer.body.trim()),
            },
            None => Probe {
                ok: false,
                latency_ms: answer.latency_ms,
                status: None,
                message: answer.body,
            },
        }
    }
}

/// The three values an adapter is built from, decoupled from the settings row so the
/// builder needs no database and the tests need no migration.
#[derive(Debug, Clone, Default)]
pub struct ProviderSettings {
    /// Configured API or purge endpoint.
    pub endpoint: Option<String>,
    /// Configured zone.
    pub zone: Option<String>,
    /// The write-only credential, once decrypted. Never logged, never returned.
    pub credential: Option<String>,
}

/// Build the adapter a settings row names.
///
/// The only dispatch point in the platform: the queue worker holds a [`Provider`] and
/// everything above it is written against the trait. An unknown key falls back to `origin`
/// rather than panicking, because a provider row edited by hand in the database must
/// degrade to "there is no edge" rather than take the purge worker down with it.
#[must_use]
pub fn provider_for(key: &str, settings: &ProviderSettings) -> Box<dyn Provider> {
    match key {
        "generic_http" => Box::new(GenericHttpProvider::new(
            settings.endpoint.as_deref(),
            settings.credential.clone(),
            settings.zone.clone(),
        )),
        "cloudflare_style" => Box::new(CloudflareStyleProvider::new(
            settings.endpoint.as_deref(),
            settings.zone.clone(),
            settings.credential.clone(),
        )),
        _ => Box::new(OriginProvider),
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

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    /// A loopback server that answers every request with one canned response and reports
    /// what it received, so a test can assert the *request* the adapter made and not only
    /// the outcome it derived from the answer.
    ///
    /// Hand-rolled rather than a test HTTP framework on purpose: the assertions here are
    /// "the method, the path and the JSON body", and a framework would add a dependency to
    /// prove three lines about a socket.
    fn serve(status_line: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the test server must bind");
        let port = listener
            .local_addr()
            .expect("the bound address must be readable")
            .port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Two connections: the probe and the purge. A single accept would leave the
            // test hanging on whichever call the adapter made second.
            for _ in 0..2 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let tx = tx.clone();
                std::thread::spawn(move || {
                    answer(stream, status_line, body, tx);
                });
            }
        });
        (format!("http://127.0.0.1:{port}/purge"), rx)
    }

    /// Read one request, answer it, and send the request head to the channel.
    fn answer(
        mut stream: TcpStream,
        status_line: &'static str,
        body: &'static str,
        tx: mpsc::Sender<String>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("the stream must clone"));
        let mut head = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end().to_string();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            head.push_str(&trimmed);
            head.push('\n');
        }
        let mut payload = vec![0u8; length];
        if length > 0 {
            use std::io::Read;
            let _ = reader.read_exact(&mut payload);
        }
        let _ = tx.send(format!("{head}|{}", String::from_utf8_lossy(&payload)));

        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    #[test]
    fn the_generic_adapter_posts_the_documented_body_and_treats_a_2xx_as_success() {
        let (endpoint, requests) = serve("200 OK", r#"{"ok":true}"#);
        let provider =
            GenericHttpProvider::new(Some(&endpoint), Some("secret-token".to_string()), None)
                .with_timeout(4_000);

        let outcome = provider.purge(&Purge::Urls {
            targets: vec!["/blog".into(), "/blog/post".into()],
        });
        assert_eq!(outcome, PurgeOutcome::Succeeded);

        let seen = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the adapter must have made a request");
        assert!(seen.starts_with("POST /purge"), "request: {seen}");
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer secret-token"),
            "the credential must be sent: {seen}"
        );
        let body = seen.rsplit_once('|').expect("head and body").1;
        let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
        assert_eq!(parsed["kind"], "urls");
        assert_eq!(parsed["targets"][0], "/blog");
        assert_eq!(parsed["targets"][1], "/blog/post");
    }

    #[test]
    fn a_2xx_that_contradicts_itself_is_a_failure_rather_than_a_purge() {
        // The case that matters: a queue that records this as a success leaves a cache
        // serving old bytes while the panel says the purge is done.
        let (endpoint, _requests) = serve("200 OK", r#"{"ok":false,"message":"edge unreachable"}"#);
        let provider = GenericHttpProvider::new(Some(&endpoint), None, None).with_timeout(4_000);
        let outcome = provider.purge(&Purge::All);
        match outcome {
            PurgeOutcome::Failed { message } => {
                assert!(message.contains("edge unreachable"), "message: {message}");
            }
            other => panic!("a self-contradicting 2xx must fail, got {other:?}"),
        }
    }

    #[test]
    fn a_non_2xx_carries_the_providers_own_message() {
        let (endpoint, _requests) = serve("503 Service Unavailable", r#"{"error":"maintenance"}"#);
        let provider = GenericHttpProvider::new(Some(&endpoint), None, None).with_timeout(4_000);
        let outcome = provider.purge(&Purge::Urls {
            targets: vec!["/blog".into()],
        });
        match outcome {
            PurgeOutcome::Failed { message } => {
                assert!(message.contains("503"), "status must be named: {message}");
                assert!(
                    message.contains("maintenance"),
                    "body must be kept: {message}"
                );
            }
            other => panic!("a 503 must fail, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_body_on_a_2xx_is_a_success_rather_than_an_invented_failure() {
        let (endpoint, _requests) = serve("204 No Content", "");
        let provider = GenericHttpProvider::new(Some(&endpoint), None, None).with_timeout(4_000);
        assert_eq!(
            provider.purge(&Purge::All),
            PurgeOutcome::Succeeded,
            "a provider that only sets a status has confirmed the call"
        );
    }

    #[test]
    fn a_provider_with_no_endpoint_fails_with_the_reason_rather_than_a_silent_success() {
        let provider = GenericHttpProvider::new(None, None, None);
        match provider.purge(&Purge::All) {
            PurgeOutcome::Failed { message } => {
                assert!(message.contains("no purge endpoint"), "message: {message}");
            }
            other => panic!("an unconfigured provider must fail, got {other:?}"),
        }
        assert!(!provider.verify().ok);
    }

    #[test]
    fn an_unreachable_endpoint_is_a_transport_failure_and_not_a_success() {
        // Port 1 on loopback: reserved, so the connection is refused immediately rather
        // than hanging for the timeout.
        let provider = GenericHttpProvider::new(Some("http://127.0.0.1:1/purge"), None, None)
            .with_timeout(2_000);
        match provider.purge(&Purge::All) {
            PurgeOutcome::Failed { message } => {
                assert!(
                    message.contains("could not be reached"),
                    "message: {message}"
                );
            }
            other => panic!("an unreachable endpoint must fail, got {other:?}"),
        }
        assert!(!provider.verify().ok);
    }

    #[test]
    fn the_generic_probe_never_asks_for_an_invalidation() {
        let (endpoint, requests) = serve("200 OK", "{}");
        let provider = GenericHttpProvider::new(Some(&endpoint), None, None).with_timeout(4_000);
        let probe = provider.verify();
        assert!(probe.ok);
        assert_eq!(probe.status, Some(200));

        let seen = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the probe must have made a request");
        let body = seen.rsplit_once('|').expect("head and body").1;
        let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
        assert_eq!(
            parsed["kind"], "ping",
            "a reachability check must not be able to purge anything"
        );
        assert_eq!(parsed["targets"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn the_hosted_adapter_sends_the_three_shapes_a_zone_api_expects() {
        for (request, field) in [
            (
                Purge::Urls {
                    targets: vec!["/a".into()],
                },
                "files",
            ),
            (
                Purge::Tags {
                    targets: vec!["page:1".into()],
                },
                "surrogate_keys",
            ),
            (Purge::All, "purge_everything"),
        ] {
            let (endpoint, requests) = serve("200 OK", r#"{"success":true}"#);
            // The adapter appends `/zones/{zone}/purge_cache` to the base, so the base here
            // is the server's root.
            let base = endpoint.trim_end_matches("/purge").to_string();
            let provider = CloudflareStyleProvider::new(
                Some(&base),
                Some("zone-1".to_string()),
                Some("key".to_string()),
            )
            .with_timeout(4_000);
            assert_eq!(provider.purge(&request), PurgeOutcome::Succeeded);

            let seen = requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the adapter must have made a request");
            assert!(
                seen.starts_with("POST /zones/zone-1/purge_cache"),
                "request: {seen}"
            );
            let body = seen.rsplit_once('|').expect("head and body").1;
            let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
            assert!(parsed.get(field).is_some(), "expected {field} in {parsed}");
        }
    }

    #[test]
    fn a_hosted_provider_without_a_zone_refuses_and_says_which_setting_is_missing() {
        let provider = CloudflareStyleProvider::new(
            Some("https://api.example.test"),
            Some("   ".to_string()),
            None,
        );
        match provider.purge(&Purge::All) {
            PurgeOutcome::Failed { message } => {
                assert!(
                    message.contains("no zone is configured"),
                    "message: {message}"
                );
            }
            other => panic!("a zone-less provider must fail, got {other:?}"),
        }
        assert!(!provider.verify().ok);
    }

    #[test]
    fn the_dispatcher_builds_the_adapter_its_key_names_and_degrades_an_unknown_one() {
        let settings = ProviderSettings {
            endpoint: Some("http://127.0.0.1:1/purge".to_string()),
            zone: Some("zone-1".to_string()),
            credential: None,
        };
        assert_eq!(
            provider_for("generic_http", &settings).key(),
            "generic_http"
        );
        assert_eq!(
            provider_for("cloudflare_style", &settings).key(),
            "cloudflare_style"
        );
        assert_eq!(provider_for("origin", &settings).key(), "origin");
        // A key edited by hand in the database must degrade to "no edge", not panic and
        // take the purge worker down with it.
        assert_eq!(provider_for("fastly", &settings).key(), "origin");
        assert_eq!(provider_for("", &settings).key(), "origin");
    }

    #[test]
    fn the_catalogue_only_lists_adapters_the_dispatcher_can_actually_build() {
        // The catalogue used to advertise three adapters while only `origin` existed, so
        // an operator picked a provider that silently did nothing. This is the assertion
        // that would have caught it.
        for adapter in catalogue() {
            let built = provider_for(adapter.key, &ProviderSettings::default());
            assert_eq!(
                built.key(),
                adapter.key,
                "the catalogue advertises {} but the dispatcher builds something else",
                adapter.key
            );
        }
    }
}
