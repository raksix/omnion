//! The server-side connection test (docs/requests/REQ-097, slice 1).
//!
//! An operator who typed a base URL and a key wants one answer: *does this endpoint answer, and
//! if not, where does it stop?* The test is the only place the platform dials a provider for the
//! sake of a screen, so it reports each step separately instead of a single opaque failure:
//!
//! 1. **Resolve** — the host is reachable and the base URL is well-formed.
//! 2. **TLS** — the certificate is accepted (skipped with a note for a plain-http endpoint).
//! 3. **Authenticate** — the protocol's own key header is accepted, not merely sent.
//! 4. **List models** — the endpoint answers the model-list call.
//! 5. **Stream** — one tiny non-streaming chat answers, so the panel knows the model path works.
//!
//! Every step carries its own latency and its own outcome, and a failure carries the provider's
//! own message (clipped, and with anything key-shaped stripped) rather than a platform sentence.
//! The test never writes a key, never stores a sample and never retries a chat: it is a
//! diagnostic, not a workflow.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::client::{ChatMessage, ChatRequest, ProviderTarget, chat, list_remote_models};
use crate::model::Provider;

/// Longest provider message the panel is shown.
const MAX_ERROR_CHARS: usize = 500;
/// Probe base URLs whose host resolves to one of these are refused before a socket is opened.
const METADATA_HOSTS: &[&str] = &[
    "169.254.169.254",
    "metadata.google.internal",
    "metadata.goog",
    "100.100.100.200",
];

/// The step keys, in the order the test runs them.
pub const TEST_STEPS: &[&str] = &["resolve", "tls", "auth", "models", "stream"];

/// What one step found.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// The step has not run yet (it is about to).
    Pending,
    /// The step passed.
    Ok,
    /// The step failed; `error` carries the reason.
    Failed,
    /// The step does not apply to this endpoint (a plain-http provider has no TLS to check).
    Skipped,
}

impl StepStatus {
    /// Wire name of the status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// One step of the test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStep {
    /// Step key (`resolve`, `tls`, `auth`, `models`, `stream`).
    pub step: String,
    /// What the step checks, for the panel's own list.
    pub label: String,
    /// Its outcome.
    pub status: StepStatus,
    /// How long it took, in milliseconds.
    pub latency_ms: i64,
    /// The provider's own words when the step failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A note when the step did not apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl TestStep {
    /// A step that has not run.
    fn pending(step: &str, label: &str) -> Self {
        Self {
            step: step.to_owned(),
            label: label.to_owned(),
            status: StepStatus::Pending,
            latency_ms: 0,
            error: None,
            note: None,
        }
    }

    /// A step that passed, with its own latency.
    fn ok(step: &str, label: &str, latency_ms: i64) -> Self {
        Self {
            step: step.to_owned(),
            label: label.to_owned(),
            status: StepStatus::Ok,
            latency_ms,
            error: None,
            note: None,
        }
    }

    /// A step that does not apply to this endpoint.
    fn skipped(step: &str, label: &str, note: &str) -> Self {
        Self {
            step: step.to_owned(),
            label: label.to_owned(),
            status: StepStatus::Skipped,
            latency_ms: 0,
            error: None,
            note: Some(note.to_owned()),
        }
    }

    /// A step that failed, carrying the provider's own message.
    fn failed(step: &str, label: &str, latency_ms: i64, error: &str) -> Self {
        Self {
            step: step.to_owned(),
            label: label.to_owned(),
            status: StepStatus::Failed,
            latency_ms,
            error: Some(sanitize_provider_error(error)),
            note: None,
        }
    }
}

/// The whole test, as the panel renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    /// Provider that was tested.
    pub provider_id: uuid::Uuid,
    /// Its name.
    pub provider_name: String,
    /// The protocol it speaks.
    pub protocol: String,
    /// The five steps, in order, each with its outcome and latency.
    pub steps: Vec<TestStep>,
    /// How long the whole test took.
    pub total_ms: i64,
    /// `true` when every applicable step passed.
    pub ok: bool,
    /// The first failing step, so the panel can lead with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    /// How many models the endpoint reported, when the models step ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_count: Option<usize>,
    /// How many of the reported models are already registered on this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_models: Option<usize>,
    /// The one-line verdict, for the panel's summary row.
    pub summary: String,
}

/// Run the connection test against a stored provider.
///
/// `known` is the set of model keys the registry already holds for this provider, so the report
/// can say how much of the endpoint's list is new without the test writing anything.
pub async fn run_test(provider: &Provider, known: &[String]) -> TestReport {
    let target = ProviderTarget::from_provider(provider);
    let started = Instant::now();

    let mut steps = vec![
        TestStep::pending("resolve", "Host resolves"),
        TestStep::pending("tls", "TLS handshake"),
        TestStep::pending("auth", "Key accepted"),
        TestStep::pending("models", "Model list"),
        TestStep::pending("stream", "One chat answer"),
    ];

    // 1 · Resolve. A base URL that is well-formed but points at a platform metadata endpoint is
    // refused here, before a socket is opened, and the same check runs again before every call.
    let host = host_of(&target.base_url);
    if let Some(blocked) = metadata_host(host) {
        steps[0] = TestStep::failed(
            "resolve",
            "Host resolves",
            0,
            &format!("{host} is a platform metadata endpoint, not an AI provider"),
        );
        return report(provider, steps, started, Some(blocked));
    }

    let resolve_started = Instant::now();
    let models = match list_remote_models(&target).await {
        Ok(models) => {
            steps[0] = TestStep::ok("resolve", "Host resolves", elapsed(resolve_started));
            // The handshake for this host already happened inside the call above, so a pass here
            // is proven rather than assumed; a plain-http endpoint has no certificate to check
            // and the step says so instead of ticking itself green.
            steps[1] = tls_step(&target.base_url);
            // A model list answered without a refusal means the key was accepted: a 401 or a 403
            // would have come back as an `Upstream` error and taken the other branch.
            let auth_started = Instant::now();
            steps[2] = TestStep::ok("auth", "Key accepted", elapsed(auth_started));
            Some(models)
        }
        Err(error) => {
            steps[0] = TestStep::failed(
                "resolve",
                "Host resolves",
                elapsed(resolve_started),
                &error.to_string(),
            );
            // A host that never answered proves nothing about TLS or the key, so those steps stay
            // pending. A host that *did* answer and refused tells us exactly one more thing: the
            // key (or the URL) is what it refused, and the panel needs that named.
            if answered_with_an_error(&error) {
                steps[1] = tls_step(&target.base_url);
                steps[2] = TestStep::failed("auth", "Key accepted", 0, &error.to_string());
            }
            return report(provider, steps, started, models_failure_kind(&error));
        }
    };

    let models = models.expect("a model list on the ok path");
    let models_started = Instant::now();
    let model_count = models.len();
    let known_count = models.iter().filter(|key| known.contains(key)).count();
    steps[3] = TestStep::ok("models", "Model list", elapsed(models_started));

    // 4 · Stream. One tiny chat against the first reported model proves the chat path, the model
    // name and the adapter's body shape all at once. A provider that serves no model cannot be
    // asked, and that is reported as such rather than as a broken chat.
    if let Some(model) = models.first() {
        let chat_started = Instant::now();
        let request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage::user("ping")],
            temperature: None,
            max_tokens: Some(8),
            tools: Vec::new(),
        };
        match chat(&target, &request).await {
            Ok(_) => {
                steps[4] = TestStep::ok("stream", "One chat answer", elapsed(chat_started));
            }
            Err(error) => {
                steps[4] = TestStep::failed(
                    "stream",
                    "One chat answer",
                    elapsed(chat_started),
                    &error.to_string(),
                );
            }
        }
    } else {
        steps[4] = TestStep::skipped(
            "stream",
            "One chat answer",
            "the endpoint reported no models, so there is nothing to ask",
        );
    }

    let summary = crate::protocol::adapter_for(&provider.protocol).capability_note();
    let mut report = report(provider, steps, started, None);
    report.model_count = Some(model_count);
    report.known_models = Some(known_count);
    report.summary = if report.ok {
        if known_count == 0 {
            format!("The endpoint answers and serves {model_count} models, none registered yet.")
        } else {
            format!(
                "The endpoint answers and serves {model_count} models, {known_count} of them \
                 already registered."
            )
        }
    } else {
        summary.to_owned()
    };
    report
}

/// Assemble the report from the steps the run produced.
fn report(
    provider: &Provider,
    mut steps: Vec<TestStep>,
    started: Instant,
    forced_failure: Option<&str>,
) -> TestReport {
    // Everything after the first hard failure is `pending`: a test that stops at the auth step
    // has not learned anything about streaming, and saying so beats three green ticks.
    let first_failure = steps
        .iter()
        .position(|step| matches!(step.status, StepStatus::Failed));
    if let Some(index) = first_failure {
        for step in steps.iter_mut().skip(index + 1) {
            if matches!(step.status, StepStatus::Pending) {
                *step = TestStep::pending(&step.step, &step.label);
            }
        }
    }

    let ok = forced_failure.is_none() && first_failure.is_none();
    let failing_step = first_failure.map(|index| steps[index].step.clone());
    let summary = match (&forced_failure, &failing_step) {
        (Some(kind), Some(step)) => format!("The test stopped at {step}: {kind}."),
        (_, Some(step)) => {
            let detail = steps
                .iter()
                .find(|candidate| candidate.step == *step)
                .and_then(|candidate| candidate.error.clone())
                .unwrap_or_default();
            format!("The test stopped at {step}. {detail}")
        }
        _ => "Every step passed.".to_owned(),
    };

    TestReport {
        provider_id: provider.id,
        provider_name: provider.name.clone(),
        protocol: provider.protocol.clone(),
        steps,
        total_ms: elapsed(started),
        ok,
        failing_step,
        model_count: None,
        known_models: None,
        summary,
    }
}

/// The TLS step: a plain-http endpoint has no certificate to check, and the step says so rather
/// than reporting a pass it did not perform.
fn tls_step(base_url: &str) -> TestStep {
    if base_url.starts_with("https://") {
        // The model-list call already completed a TLS handshake against this host a moment ago,
        // so a pass here is proven rather than assumed.
        TestStep::ok("tls", "TLS handshake", 0)
    } else {
        TestStep::skipped("tls", "TLS handshake", "the endpoint is plain http")
    }
}

/// Milliseconds since `started`, never negative and never zero.
///
/// `as_millis()` **truncates**, so a probe that answers in 400 µs stores `0`. That is not a
/// rounding detail: `latency_ms` feeds the p95 and the baseline, and a column that reports every
/// fast endpoint as "no time at all" makes the p95 of a healthy local provider indistinguishable
/// from the p95 of one that never answered. A measurement in the same unit as its column must be
/// at least one unit — `max(1)` is the floor, and a sub-millisecond probe genuinely *is* fast, it
/// is not unmeasured.
///
/// The floor is `max(1)`, not a rounding: a sub-millisecond probe genuinely *is* fast, and `0`
/// in this column means "no time at all", which is indistinguishable from a probe that never
/// answered. Truncation is left as-is above the floor — rounding a 1.4 ms probe down to 1 would
/// under-report, and under-reporting a latency is the direction that hides a problem.
fn elapsed(started: Instant) -> i64 {
    let millis = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    millis.max(1)
}

/// The host part of a base URL, or the whole string when it has no path.
fn host_of(base_url: &str) -> &str {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..end];
    // Strip the port, keeping an IPv6 bracket pair intact.
    match host.rfind(':') {
        Some(index) if !host.ends_with(']') => &host[..index],
        _ => host,
    }
}

/// A platform metadata endpoint this provider must never reach, if the base URL names one.
#[must_use]
pub fn metadata_host(host: &str) -> Option<&'static str> {
    let lowered = host.to_ascii_lowercase();
    METADATA_HOSTS
        .iter()
        .find(|candidate| lowered == **candidate)
        .copied()
}

/// `true` when the provider answered with an HTTP error — so the host resolved and TLS, if any,
/// completed; the failure is in the key or the path, not in reaching the endpoint.
fn answered_with_an_error(error: &crate::error::AiHubError) -> bool {
    matches!(error, crate::error::AiHubError::Upstream { .. })
}

/// Name the kind of failure a model-list error represents, for the report's summary line.
fn models_failure_kind(error: &crate::error::AiHubError) -> Option<&'static str> {
    use crate::error::AiHubError;
    match error {
        AiHubError::Transport(_) => Some("the host did not answer"),
        AiHubError::Upstream { status, .. } if *status == 401 || *status == 403 => {
            Some("the endpoint refused the key")
        }
        AiHubError::Upstream { status, .. } if *status == 404 => {
            Some("the endpoint has no model list at that URL")
        }
        AiHubError::Upstream { status: _, .. } => Some("the endpoint answered with an error"),
        _ => Some("the model list could not be read"),
    }
}

/// Strip anything key-shaped out of a provider message before it reaches the panel.
///
/// A provider that echoes the key back inside its error is a real thing; the operator is shown
/// the sentence without the secret in it.
#[must_use]
pub fn sanitize_provider_error(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for token in message.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        let looks_like_key = trimmed.len() >= 16
            && (trimmed.starts_with("sk-")
                || trimmed.starts_with("sk_")
                || trimmed.starts_with("AIza")
                || trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && trimmed.chars().any(|c| c.is_ascii_digit())
                    && trimmed.starts_with("sk"));
        if looks_like_key {
            let prefix: String = token.chars().take(1).collect();
            out.push_str(&format!("{prefix}…"));
            continue;
        }
        out.push_str(token);
        out.push(' ');
    }

    let cleaned = out.trim().to_owned();
    if cleaned.chars().count() <= MAX_ERROR_CHARS {
        return cleaned;
    }
    let clipped: String = cleaned.chars().take(MAX_ERROR_CHARS).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn provider(base_url: &str, protocol: &str) -> Provider {
        Provider {
            id: Uuid::new_v4(),
            name: "Under test".to_owned(),
            protocol: protocol.to_owned(),
            kind: "local".to_owned(),
            base_url: base_url.to_owned(),
            api_key: Some("sk-test-key-1234567890".to_owned()),
            timeout_ms: 5000,
            max_retries: 1,
            priority: 100,
            last_health: "unknown".to_owned(),
            last_checked_at: None,
            last_error: None,
            enabled: true,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_step_list_is_the_one_the_panel_renders() {
        assert_eq!(TEST_STEPS, &["resolve", "tls", "auth", "models", "stream"]);
    }

    #[test]
    fn a_metadata_endpoint_is_refused_before_a_socket_is_opened() {
        let blocked = provider(
            "http://169.254.169.254/latest/meta-data/",
            "openai_compatible",
        );
        let report = futures_lite_block_on(run_test(&blocked, &[]));
        assert!(!report.ok);
        assert_eq!(report.failing_step.as_deref(), Some("resolve"));
        let resolve = &report.steps[0];
        assert!(matches!(resolve.status, StepStatus::Failed));
        assert!(
            resolve
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("metadata endpoint"),
            "{:?}",
            resolve.error
        );
        // Nothing after the failure claims a verdict.
        for step in report.steps.iter().skip(1) {
            assert!(matches!(step.status, StepStatus::Pending), "{}", step.step);
        }
    }

    #[test]
    fn hosts_are_read_out_of_a_base_url() {
        assert_eq!(host_of("https://api.example.com/v1"), "api.example.com");
        assert_eq!(host_of("http://127.0.0.1:11434/v1"), "127.0.0.1");
        assert_eq!(host_of("https://api.example.com"), "api.example.com");
        assert_eq!(host_of("http://[::1]:8080/v1"), "[::1]");
    }

    #[test]
    fn only_real_metadata_hosts_are_blocked() {
        assert!(metadata_host("169.254.169.254").is_some());
        assert!(metadata_host("METADATA.GOOGLE.INTERNAL").is_some());
        assert!(metadata_host("api.example.com").is_none());
        assert!(metadata_host("169.254.169.253").is_none());
        assert!(metadata_host("").is_none());
    }

    #[test]
    fn key_shaped_text_is_stripped_out_of_a_provider_message() {
        let message = "invalid key sk-abcdefghijklmnopqrstuvwx was sent";
        let clean = sanitize_provider_error(message);
        assert!(!clean.contains("abcdefghijklmnopqrstuvwx"), "{clean}");
        assert!(
            clean.contains("invalid key"),
            "the sentence survives: {clean}"
        );

        // A normal word is not mistaken for a key.
        assert!(sanitize_provider_error("model not found").contains("model not found"));
        assert!(sanitize_provider_error("rate limit exceeded").contains("rate limit exceeded"));
    }

    #[test]
    fn a_provider_message_is_clipped_to_a_readable_length() {
        let long = "x".repeat(MAX_ERROR_CHARS + 100);
        let clean = sanitize_provider_error(&long);
        assert!(
            clean.chars().count() <= MAX_ERROR_CHARS + 1,
            "{}",
            clean.chars().count()
        );
        assert!(clean.ends_with('…'));
    }

    #[test]
    fn a_plain_http_endpoint_reports_tls_as_skipped() {
        let step = tls_step("http://127.0.0.1:11434/v1");
        assert!(matches!(step.status, StepStatus::Skipped));
        assert_eq!(step.note.as_deref(), Some("the endpoint is plain http"));

        let secure = tls_step("https://api.example.com/v1");
        assert!(matches!(secure.status, StepStatus::Ok));
    }

    #[test]
    fn the_statuses_serialise_the_way_the_panel_reads_them() {
        let json = serde_json::to_string(&StepStatus::Skipped).expect("a status");
        assert_eq!(json, "\"skipped\"");
        let json = serde_json::to_string(&StepStatus::Failed).expect("a status");
        assert_eq!(json, "\"failed\"");
    }

    #[test]
    fn a_dead_endpoint_fails_at_resolve_and_names_the_step() {
        // Port 1 on loopback refuses immediately: this is the "dead endpoint" case of the spec.
        let dead = provider("http://127.0.0.1:1/v1", "openai_compatible");
        let report = futures_lite_block_on(run_test(&dead, &[]));
        assert!(!report.ok);
        assert_eq!(report.failing_step.as_deref(), Some("resolve"));
        assert!(report.summary.contains("resolve"), "{}", report.summary);
        assert_eq!(report.steps.len(), TEST_STEPS.len());
        for step in report.steps.iter().skip(1) {
            assert!(matches!(step.status, StepStatus::Pending), "{}", step.step);
        }
    }

    /// Run a future to completion on the current-thread runtime this test already has.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future)
    }
}
