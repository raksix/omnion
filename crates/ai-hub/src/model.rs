//! The AI Hub's stored shapes: the providers an installation connected and the models they
//! serve (docs/06-AI-HUB.md §1–§3).
//!
//! A **provider** is one connection: a name the operator sees, the protocol it speaks, the base
//! URL it lives at and the key the platform authenticates with. The key is write-only through
//! the API — it is stored so the platform can sign its calls, and never handed back out.
//!
//! A **model** belongs to exactly one provider and carries what the router needs to pick it:
//! its wire key (the identifier the provider knows), a context window and the capability flags
//! from docs/06 §3 (`supports_tools`, `supports_vision`, `supports_streaming`,
//! `supports_embeddings`). Exactly one enabled model may be the installation's default.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// Wire protocols an installation can connect.
///
/// The OpenAI-compatible protocol is the one OpenAI itself, OpenCode, CommandCode, Ollama and a
/// local vLLM server all expose (docs/06-AI-HUB.md §1: "almost any service exposing an
/// OpenAI-compatible API can connect"); the other two cover the messages and generateContent
/// shapes (docs/requests/REQ-097). A fourth adapter attaches through the trait in
/// [`crate::protocol`] without a fourth value here.
pub const SUPPORTED_PROTOCOLS: &[&str] =
    &["openai_compatible", "anthropic_messages", "google_gemini"];

/// Where a provider lives. The runtime dials both the same way; the kind is what a screen groups
/// by and what tells an operator that a provider answers on their own network.
pub const PROVIDER_KINDS: &[&str] = &["cloud", "local"];

/// Health verdicts a provider carries. `unknown` is a real value: never probed yet.
pub const HEALTH_STATUSES: &[&str] = &["ok", "degraded", "down", "unknown"];

/// Smallest timeout a provider may be given, in milliseconds.
pub const MIN_TIMEOUT_MS: i32 = 1000;
/// Largest timeout a provider may be given, in milliseconds.
pub const MAX_TIMEOUT_MS: i32 = 120_000;
/// Retry ceiling for one call, before the first byte.
pub const MAX_RETRIES_CEILING: i32 = 5;
/// Priority bounds, lowest asked first.
pub const MIN_PRIORITY: i32 = 1;
/// Priority bounds, lowest asked first.
pub const MAX_PRIORITY: i32 = 1000;

/// Protocol used when a request does not name one.
pub const DEFAULT_PROTOCOL: &str = "openai_compatible";

/// Longest provider name.
pub const MAX_NAME_LEN: usize = 64;
/// Longest model key.
pub const MAX_MODEL_KEY_LEN: usize = 200;
/// Longest base URL.
pub const MAX_BASE_URL_LEN: usize = 512;

/// One connected AI provider.
#[derive(Debug, Clone, FromRow)]
pub struct Provider {
    /// Provider id.
    pub id: Uuid,
    /// Display name, unique per installation (case-insensitive).
    pub name: String,
    /// Wire protocol (`openai_compatible`, `anthropic_messages`, `google_gemini`).
    pub protocol: String,
    /// `cloud` or `local` (docs/requests/REQ-097).
    pub kind: String,
    /// How long one call may take, in milliseconds.
    pub timeout_ms: i32,
    /// How often a failure before the first byte is retried.
    pub max_retries: i32,
    /// Position in the failover chain; lower is asked first.
    pub priority: i32,
    /// `ok`, `degraded`, `down` or `unknown` as the last probe left it.
    pub last_health: String,
    /// When a probe last took a sample here.
    pub last_checked_at: Option<OffsetDateTime>,
    /// What the last failed probe said, clipped.
    pub last_error: Option<String>,
    /// Base URL of the API, version segment included (e.g. `https://api.example.com/v1`).
    pub base_url: String,
    /// Key the platform authenticates with. Never leaves the platform again.
    pub api_key: Option<String>,
    /// `false` when the operator switched the provider off: the router skips it.
    pub enabled: bool,
    /// `true` for the provider the router prefers when a model name is ambiguous.
    pub is_default: bool,
    /// When the provider was connected.
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    pub updated_at: OffsetDateTime,
}

impl Provider {
    /// `true` when a usable key is stored — the only fact the API hands out about the key.
    #[must_use]
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_deref().is_some_and(|key| !key.is_empty())
    }
}

/// A provider to be created.
#[derive(Debug, Clone)]
pub struct NewProvider {
    /// Display name (validated by [`validate_name`]).
    pub name: String,
    /// Wire protocol (validated by [`validate_protocol`]).
    pub protocol: String,
    /// `cloud` or `local` (validated by [`validate_kind`]).
    pub kind: String,
    /// Base URL (normalized by [`normalize_base_url`]).
    pub base_url: String,
    /// Key to authenticate with; `None` for a local endpoint that wants none.
    pub api_key: Option<String>,
    /// How long one call may take, in milliseconds (validated by [`validate_timeout`]).
    pub timeout_ms: i32,
    /// How often a pre-first-byte failure is retried (validated by [`validate_retries`]).
    pub max_retries: i32,
    /// Position in the failover chain (validated by [`validate_priority`]).
    pub priority: i32,
    /// Whether the provider starts enabled.
    pub enabled: bool,
    /// Whether the provider becomes the installation's default.
    pub is_default: bool,
}

/// What an update does with the stored key.
#[derive(Debug, Clone, Default)]
pub enum ApiKeyChange {
    /// Leave the stored key alone.
    #[default]
    Keep,
    /// Replace it.
    Set(String),
    /// Forget it (a local endpoint that wants no key).
    Clear,
}

/// Changes applied to one provider; `None` leaves a field as it was.
#[derive(Debug, Clone, Default)]
pub struct ProviderChanges {
    /// New display name.
    pub name: Option<String>,
    /// New base URL.
    pub base_url: Option<String>,
    /// What happens to the key.
    pub api_key: ApiKeyChange,
    /// New kind.
    pub kind: Option<String>,
    /// New timeout.
    pub timeout_ms: Option<i32>,
    /// New retry ceiling.
    pub max_retries: Option<i32>,
    /// New position in the failover chain.
    pub priority: Option<i32>,
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// `true` makes this provider the installation's default.
    pub is_default: Option<bool>,
}

/// One model of one provider.
#[derive(Debug, Clone, FromRow)]
pub struct AiModel {
    /// Model id.
    pub id: Uuid,
    /// Provider that serves it.
    pub provider_id: Uuid,
    /// Wire key — the identifier the provider knows (e.g. `gpt-4o-mini`).
    pub model_key: String,
    /// Name shown in the panel; falls back to the key.
    pub display_name: Option<String>,
    /// Context window in tokens, when known.
    pub context_window: Option<i32>,
    /// Whether the model can call tools.
    pub supports_tools: bool,
    /// Whether the model accepts images.
    pub supports_vision: bool,
    /// Whether the model can answer as a stream.
    pub supports_streaming: bool,
    /// Whether the model produces embeddings.
    pub supports_embeddings: bool,
    /// `false` when the operator switched the model off.
    pub enabled: bool,
    /// `true` for the installation's default model.
    pub is_default: bool,
    /// When the model was registered.
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    pub updated_at: OffsetDateTime,
}

impl AiModel {
    /// Name to show in the panel.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.model_key)
    }
}

/// A model to be registered on a provider.
///
/// Optional fields carry "the caller said nothing": an update keeps whatever the row already
/// holds, so re-applying a model list never silently drops a capability or a context window.
#[derive(Debug, Clone)]
pub struct NewAiModel {
    /// Wire key.
    pub model_key: String,
    /// Name shown in the panel.
    pub display_name: Option<String>,
    /// Context window in tokens.
    pub context_window: Option<i32>,
    /// Tool calling.
    pub supports_tools: Option<bool>,
    /// Image input.
    pub supports_vision: Option<bool>,
    /// Streaming answers.
    pub supports_streaming: Option<bool>,
    /// Embedding output.
    pub supports_embeddings: Option<bool>,
}

impl NewAiModel {
    /// A model identified by its key alone.
    #[must_use]
    pub fn new(model_key: impl Into<String>) -> Self {
        Self {
            model_key: model_key.into(),
            display_name: None,
            context_window: None,
            supports_tools: None,
            supports_vision: None,
            supports_streaming: None,
            supports_embeddings: None,
        }
    }
}

/// Changes applied to one model; `None` leaves a field as it was.
#[derive(Debug, Clone, Default)]
pub struct ModelChanges {
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// `true` makes this model the installation's default.
    pub is_default: Option<bool>,
}

/// Check a protocol key against [`SUPPORTED_PROTOCOLS`].
pub fn validate_protocol(protocol: &str) -> Result<()> {
    if SUPPORTED_PROTOCOLS.contains(&protocol) {
        return Ok(());
    }

    Err(AiHubError::InvalidProvider(format!(
        "protocol \"{protocol}\" is not supported yet (supported: {})",
        SUPPORTED_PROTOCOLS.join(", ")
    )))
}

/// Check a provider kind against [`PROVIDER_KINDS`].
pub fn validate_kind(kind: &str) -> Result<()> {
    if PROVIDER_KINDS.contains(&kind) {
        return Ok(());
    }

    Err(AiHubError::InvalidProvider(format!(
        "kind \"{kind}\" is not supported (supported: {})",
        PROVIDER_KINDS.join(", ")
    )))
}

/// Check a provider timeout: the bounds the form and the database both enforce.
pub fn validate_timeout(timeout_ms: i32) -> Result<()> {
    if (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Ok(());
    }

    Err(AiHubError::InvalidProvider(format!(
        "a timeout must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} milliseconds"
    )))
}

/// Check a provider retry ceiling.
pub fn validate_retries(max_retries: i32) -> Result<()> {
    if (0..=MAX_RETRIES_CEILING).contains(&max_retries) {
        return Ok(());
    }

    Err(AiHubError::InvalidProvider(format!(
        "max retries must be between 0 and {MAX_RETRIES_CEILING}"
    )))
}

/// Check a provider priority.
pub fn validate_priority(priority: i32) -> Result<()> {
    if (MIN_PRIORITY..=MAX_PRIORITY).contains(&priority) {
        return Ok(());
    }

    Err(AiHubError::InvalidProvider(format!(
        "a priority must be between {MIN_PRIORITY} and {MAX_PRIORITY}"
    )))
}

/// Check a provider name: visible characters, bounded length.
pub fn validate_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AiHubError::InvalidProvider(
            "a provider needs a name".to_owned(),
        ));
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(AiHubError::InvalidProvider(format!(
            "a provider name may carry at most {MAX_NAME_LEN} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(AiHubError::InvalidProvider(
            "a provider name may not carry control characters".to_owned(),
        ));
    }

    Ok(())
}

/// Check and normalize a base URL.
///
/// The stored form has no trailing slash, so the client can append `/chat/completions` without
/// building a double slash.
pub fn normalize_base_url(base_url: &str) -> Result<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(AiHubError::InvalidProvider(
            "a provider needs a base URL".to_owned(),
        ));
    }
    if trimmed.chars().count() > MAX_BASE_URL_LEN {
        return Err(AiHubError::InvalidProvider(format!(
            "a base URL may carry at most {MAX_BASE_URL_LEN} characters"
        )));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(AiHubError::InvalidProvider(
            "a base URL may not carry whitespace".to_owned(),
        ));
    }

    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"));
    let Some(rest) = rest else {
        return Err(AiHubError::InvalidProvider(
            "a base URL must start with http:// or https://".to_owned(),
        ));
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err(AiHubError::InvalidProvider(
            "a base URL needs a host".to_owned(),
        ));
    }

    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// Check a model key: the identifier the provider knows.
pub fn validate_model_key(model_key: &str) -> Result<()> {
    let trimmed = model_key.trim();
    if trimmed.is_empty() {
        return Err(AiHubError::InvalidModel("a model needs a key".to_owned()));
    }
    if trimmed.chars().count() > MAX_MODEL_KEY_LEN {
        return Err(AiHubError::InvalidModel(format!(
            "a model key may carry at most {MAX_MODEL_KEY_LEN} characters"
        )));
    }
    if trimmed.chars().any(char::is_whitespace) || trimmed.chars().any(char::is_control) {
        return Err(AiHubError::InvalidModel(
            "a model key may not carry whitespace or control characters".to_owned(),
        ));
    }

    Ok(())
}

/// Serialized shape of one provider row (the `Serialize` half of the model for the CLI and any
/// future SDK; the API has its own response bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    /// Provider id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Wire protocol.
    pub protocol: String,
    /// `cloud` or `local`.
    pub kind: String,
    /// Base URL.
    pub base_url: String,
    /// Whether a key is stored.
    pub has_api_key: bool,
    /// How long one call may take, in milliseconds.
    pub timeout_ms: i32,
    /// How often a pre-first-byte failure is retried.
    pub max_retries: i32,
    /// Position in the failover chain.
    pub priority: i32,
    /// `ok`, `degraded`, `down` or `unknown`.
    pub last_health: String,
    /// When a probe last took a sample here.
    pub last_checked_at: Option<OffsetDateTime>,
    /// What the last failed probe said.
    pub last_error: Option<String>,
    /// Whether the provider is enabled.
    pub enabled: bool,
    /// Whether the provider is the installation's default.
    pub is_default: bool,
}

impl From<&Provider> for ProviderSummary {
    fn from(provider: &Provider) -> Self {
        Self {
            id: provider.id,
            name: provider.name.clone(),
            protocol: provider.protocol.clone(),
            kind: provider.kind.clone(),
            base_url: provider.base_url.clone(),
            has_api_key: provider.has_api_key(),
            timeout_ms: provider.timeout_ms,
            max_retries: provider.max_retries,
            priority: provider.priority,
            last_health: provider.last_health.clone(),
            last_checked_at: provider.last_checked_at,
            last_error: provider.last_error.clone(),
            enabled: provider.enabled,
            is_default: provider.is_default,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert!(validate_name("Local Llama").is_ok());
        assert!(validate_name("  Office AI  ").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("with\nnewline").is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_LEN)).is_ok());
    }

    #[test]
    fn base_urls_are_normalized() {
        assert_eq!(
            normalize_base_url("https://api.example.com/v1/").expect("valid"),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_base_url(" http://127.0.0.1:11434/v1 ").expect("valid"),
            "http://127.0.0.1:11434/v1"
        );
        assert!(normalize_base_url("api.example.com/v1").is_err());
        assert!(normalize_base_url("https://").is_err());
        assert!(normalize_base_url("https:///v1").is_err());
        assert!(normalize_base_url("https://a b/v1").is_err());
        assert!(normalize_base_url("").is_err());
    }

    #[test]
    fn model_keys_are_checked() {
        assert!(validate_model_key("gpt-4o-mini").is_ok());
        assert!(validate_model_key("meta-llama/Llama-3.1-8B-Instruct").is_ok());
        assert!(validate_model_key("").is_err());
        assert!(validate_model_key("two words").is_err());
        assert!(validate_model_key(&"m".repeat(MAX_MODEL_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn the_runtime_ranges_are_checked() {
        assert!(validate_kind("cloud").is_ok());
        assert!(validate_kind("local").is_ok());
        assert!(validate_kind("on-premises").is_err());
        assert!(validate_kind("").is_err());

        assert!(validate_timeout(MIN_TIMEOUT_MS).is_ok());
        assert!(validate_timeout(MAX_TIMEOUT_MS).is_ok());
        assert!(validate_timeout(MIN_TIMEOUT_MS - 1).is_err());
        assert!(validate_timeout(MAX_TIMEOUT_MS + 1).is_err());

        assert!(validate_retries(0).is_ok());
        assert!(validate_retries(MAX_RETRIES_CEILING).is_ok());
        assert!(validate_retries(-1).is_err());
        assert!(validate_retries(MAX_RETRIES_CEILING + 1).is_err());

        assert!(validate_priority(MIN_PRIORITY).is_ok());
        assert!(validate_priority(MAX_PRIORITY).is_ok());
        assert!(validate_priority(0).is_err());
        assert!(validate_priority(MAX_PRIORITY + 1).is_err());
    }

    #[test]
    fn the_health_vocabulary_is_closed() {
        assert_eq!(HEALTH_STATUSES, &["ok", "degraded", "down", "unknown"]);
    }

    #[test]
    fn protocols_are_checked() {
        for protocol in SUPPORTED_PROTOCOLS {
            assert!(validate_protocol(protocol).is_ok(), "{protocol}");
        }
        assert!(validate_protocol("anthropic").is_err());
        assert_eq!(
            AiHubError::InvalidProvider(String::new())
                .to_string()
                .split(':')
                .next(),
            Some("invalid provider")
        );
    }

    #[test]
    fn labels_fall_back_to_the_key() {
        let model = AiModel {
            id: Uuid::nil(),
            provider_id: Uuid::nil(),
            model_key: "gpt-4o-mini".to_owned(),
            display_name: None,
            context_window: None,
            supports_tools: false,
            supports_vision: false,
            supports_streaming: true,
            supports_embeddings: false,
            enabled: true,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(model.label(), "gpt-4o-mini");

        let named = AiModel {
            display_name: Some("  ".to_owned()),
            ..model.clone()
        };
        assert_eq!(named.label(), "gpt-4o-mini", "blank names fall back too");

        let proper = AiModel {
            display_name: Some("Small".to_owned()),
            ..model
        };
        assert_eq!(proper.label(), "Small");
    }

    #[test]
    fn a_provider_reports_whether_it_holds_a_key() {
        let provider = Provider {
            id: Uuid::nil(),
            name: "Local".to_owned(),
            protocol: DEFAULT_PROTOCOL.to_owned(),
            kind: "local".to_owned(),
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            api_key: Some(String::new()),
            timeout_ms: 30_000,
            max_retries: 1,
            priority: 100,
            last_health: "unknown".to_owned(),
            last_checked_at: None,
            last_error: None,
            enabled: true,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(!provider.has_api_key(), "an empty key is no key");

        let summary = ProviderSummary::from(&Provider {
            api_key: Some("sk-test".to_owned()),
            ..provider
        });
        assert!(summary.has_api_key);
        assert_eq!(summary.name, "Local");
    }
}
