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

/// One named thing a model can do — the typed capability flags of REQ-097, as a closed set rather
/// than a column list every caller has to memorise.
///
/// The set is closed on purpose. A caller asks "can this model do X" and gets an answer it can
/// print and a code it can branch on; nothing anywhere guesses by looking at a model *name*.
///
/// `Chat` is the one flag that is not a column: every row in `ai_models` is a chat model by
/// construction (a provider that serves embeddings lists them the same way), so it answers `true`
/// and the registry lists it so an operator can see the flag is on rather than missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    /// Answers a conversation.
    Chat,
    /// Calls tools.
    Tools,
    /// Accepts images in the request.
    Vision,
    /// Answers as a stream.
    Streaming,
    /// Produces embedding vectors.
    Embeddings,
    /// Generates images.
    ImageGeneration,
    /// Generates audio.
    AudioGeneration,
    /// Transcribes audio.
    Transcription,
    /// Answers in the provider's structured-output mode.
    JsonMode,
    /// The endpoint can list its own models. A provider fact, kept in the closed set so a screen
    /// can show the whole vocabulary in one place.
    ListModels,
}

impl ModelCapability {
    /// Every capability, in the order the panel renders them.
    pub const ALL: &'static [Self] = &[
        Self::Chat,
        Self::Streaming,
        Self::Tools,
        Self::Vision,
        Self::JsonMode,
        Self::Embeddings,
        Self::ImageGeneration,
        Self::AudioGeneration,
        Self::Transcription,
        Self::ListModels,
    ];

    /// Wire name, as stored in JSON and as the panel's toggle key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Tools => "tools",
            Self::Vision => "vision",
            Self::Streaming => "streaming",
            Self::Embeddings => "embeddings",
            Self::ImageGeneration => "image_generation",
            Self::AudioGeneration => "audio_generation",
            Self::Transcription => "transcription",
            Self::JsonMode => "json_mode",
            Self::ListModels => "list_models",
        }
    }

    /// One line the panel shows under the toggle.
    #[must_use]
    pub fn note(self) -> &'static str {
        match self {
            Self::Chat => "Answers a conversation.",
            Self::Tools => "Calls the platform's tools.",
            Self::Vision => "Accepts images in a request.",
            Self::Streaming => "Answers as a stream.",
            Self::Embeddings => "Produces embedding vectors.",
            Self::ImageGeneration => "Generates images.",
            Self::AudioGeneration => "Generates audio.",
            Self::Transcription => "Transcribes audio.",
            Self::JsonMode => "Answers in the provider's structured-output mode.",
            Self::ListModels => "The endpoint can list its own models.",
        }
    }

    /// Read a wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|capability| capability.as_str() == value)
    }

    /// `true` for the flags a model row can turn on; `ListModels` belongs to the provider and
    /// `Chat` is true for every row, so neither is editable here.
    #[must_use]
    pub fn is_model_flag(self) -> bool {
        !matches!(self, Self::Chat | Self::ListModels)
    }
}

impl std::fmt::Display for ModelCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Check one capability against a model and refuse with a message a person can act on.
///
/// This is the enforcement half of "the flags are data the registry edits and the router
/// enforces": a request that needs a capability the model does not claim is refused **before any
/// call leaves the process**, and the refusal names the model and the capability, so the fix is
/// obvious from the error alone.
pub fn require_capability(
    model: &AiModel,
    capability: ModelCapability,
) -> std::result::Result<(), AiHubError> {
    if model.capability(capability) {
        return Ok(());
    }

    Err(AiHubError::CapabilityUnsupported {
        model: model.model_key.clone(),
        capability: capability.as_str(),
    })
}

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
/// Longest model display name — a label for the panel, not a description.
pub const MAX_DISPLAY_NAME_LEN: usize = 120;

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
    /// Whether the model generates images.
    pub supports_image_generation: bool,
    /// Whether the model generates audio.
    pub supports_audio_generation: bool,
    /// Whether the model transcribes audio.
    pub supports_transcription: bool,
    /// Whether the model answers in the provider's own JSON mode.
    pub supports_json_mode: bool,
    /// Largest answer the model advertises, when it says.
    pub max_output_tokens: Option<i32>,
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

    /// What one named capability flag says about this model.
    ///
    /// The router and the panel ask the same question through this one function, so a flag the
    /// panel shows and a flag the router reads can never drift: there is one row, one accessor,
    /// one answer.
    #[must_use]
    pub fn capability(&self, capability: ModelCapability) -> bool {
        match capability {
            ModelCapability::Chat => true,
            ModelCapability::Tools => self.supports_tools,
            ModelCapability::Vision => self.supports_vision,
            ModelCapability::Streaming => self.supports_streaming,
            ModelCapability::Embeddings => self.supports_embeddings,
            ModelCapability::ImageGeneration => self.supports_image_generation,
            ModelCapability::AudioGeneration => self.supports_audio_generation,
            ModelCapability::Transcription => self.supports_transcription,
            ModelCapability::JsonMode => self.supports_json_mode,
            // Whether the endpoint can list its own models is a fact about the *provider*, not
            // about one model of it; the answer is carried by the provider, so a model answers
            // "no" and the caller asks the provider instead.
            ModelCapability::ListModels => false,
        }
    }

    /// The `true` capabilities of this model, in the closed order the panel renders them in.
    #[must_use]
    pub fn capabilities(&self) -> Vec<ModelCapability> {
        ModelCapability::ALL
            .iter()
            .copied()
            .filter(|capability| self.capability(*capability))
            .collect()
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
    /// Image generation.
    pub supports_image_generation: Option<bool>,
    /// Audio generation.
    pub supports_audio_generation: Option<bool>,
    /// Transcription.
    pub supports_transcription: Option<bool>,
    /// Structured output through the provider's own mode.
    pub supports_json_mode: Option<bool>,
    /// Largest answer the model advertises.
    pub max_output_tokens: Option<i32>,
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
            supports_image_generation: None,
            supports_audio_generation: None,
            supports_transcription: None,
            supports_json_mode: None,
            max_output_tokens: None,
        }
    }

    /// Turn on one capability flag.
    #[must_use]
    pub fn with(mut self, capability: ModelCapability, on: bool) -> Self {
        let slot = match capability {
            ModelCapability::Tools => &mut self.supports_tools,
            ModelCapability::Vision => &mut self.supports_vision,
            ModelCapability::Streaming => &mut self.supports_streaming,
            ModelCapability::Embeddings => &mut self.supports_embeddings,
            ModelCapability::ImageGeneration => &mut self.supports_image_generation,
            ModelCapability::AudioGeneration => &mut self.supports_audio_generation,
            ModelCapability::Transcription => &mut self.supports_transcription,
            ModelCapability::JsonMode => &mut self.supports_json_mode,
            // Chat and list-models are not model-row columns; a caller that asks to set them is
            // answered with the value it already has rather than a silent no-op.
            ModelCapability::Chat | ModelCapability::ListModels => return self,
        };
        *slot = Some(on);

        self
    }

    /// The value this input carries for one capability; `None` means "the caller said nothing".
    #[must_use]
    pub fn capability(&self, capability: ModelCapability) -> Option<bool> {
        match capability {
            ModelCapability::Chat => Some(true),
            ModelCapability::ListModels => None,
            ModelCapability::Tools => self.supports_tools,
            ModelCapability::Vision => self.supports_vision,
            ModelCapability::Streaming => self.supports_streaming,
            ModelCapability::Embeddings => self.supports_embeddings,
            ModelCapability::ImageGeneration => self.supports_image_generation,
            ModelCapability::AudioGeneration => self.supports_audio_generation,
            ModelCapability::Transcription => self.supports_transcription,
            ModelCapability::JsonMode => self.supports_json_mode,
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
    /// New display name.
    pub display_name: Option<String>,
    /// New context window.
    pub context_window: Option<i32>,
    /// New tool-calling flag.
    pub supports_tools: Option<bool>,
    /// New image-input flag.
    pub supports_vision: Option<bool>,
    /// New streaming flag.
    pub supports_streaming: Option<bool>,
    /// New embeddings flag.
    pub supports_embeddings: Option<bool>,
    /// New image-generation flag.
    pub supports_image_generation: Option<bool>,
    /// New audio-generation flag.
    pub supports_audio_generation: Option<bool>,
    /// New transcription flag.
    pub supports_transcription: Option<bool>,
    /// New JSON-mode flag.
    pub supports_json_mode: Option<bool>,
    /// New answer ceiling.
    pub max_output_tokens: Option<Option<i32>>,
}

impl ModelChanges {
    /// Set one capability flag.
    #[must_use]
    pub fn with(mut self, capability: ModelCapability, on: bool) -> Self {
        let slot = match capability {
            ModelCapability::Tools => &mut self.supports_tools,
            ModelCapability::Vision => &mut self.supports_vision,
            ModelCapability::Streaming => &mut self.supports_streaming,
            ModelCapability::Embeddings => &mut self.supports_embeddings,
            ModelCapability::ImageGeneration => &mut self.supports_image_generation,
            ModelCapability::AudioGeneration => &mut self.supports_audio_generation,
            ModelCapability::Transcription => &mut self.supports_transcription,
            ModelCapability::JsonMode => &mut self.supports_json_mode,
            ModelCapability::Chat | ModelCapability::ListModels => return self,
        };
        *slot = Some(on);

        self
    }
}

/// What one discovery line means for the registry: the key is new, the metadata moved, or the
/// endpoint stopped serving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryAction {
    /// The endpoint serves it and the registry does not carry it.
    Added,
    /// The endpoint serves it and the registry carries it with different metadata.
    Changed,
    /// The registry carries it and the endpoint no longer lists it.
    Removed,
}

impl DiscoveryAction {
    /// Wire name, as the panel's diff badge reads it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Removed => "removed",
        }
    }
}

/// One line of a discovery diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryLine {
    /// Wire key the endpoint reported.
    pub model_key: String,
    /// What this line means.
    pub action: DiscoveryAction,
    /// Field names whose value moved, for a `changed` line; empty otherwise.
    ///
    /// `String` rather than `&'static str`: the only field names that exist are the ones this
    /// module writes, but a serializable response type is also deserialized on the way back
    /// in the test harness, and a borrowed `'static` cannot be built from an incoming buffer.
    pub changed_fields: Vec<String>,
}

/// What a discovery run found, and what applying it would do.
///
/// The diff is computed and returned, never written: the panel shows it and the operator
/// confirms. That is the whole point of a diff — an endpoint that suddenly reports two hundred
/// models must not be able to rewrite the registry by being asked a question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryDiff {
    /// Provider that was asked.
    pub provider_id: Uuid,
    /// Its name.
    pub provider_name: String,
    /// Keys the endpoint reported.
    pub reported: Vec<String>,
    /// Keys the registry carried before.
    pub stored: Vec<String>,
    /// The lines, in key order.
    pub lines: Vec<DiscoveryLine>,
}

impl DiscoveryDiff {
    /// How many lines carry one action.
    #[must_use]
    pub fn count(&self, action: DiscoveryAction) -> usize {
        self.lines
            .iter()
            .filter(|line| line.action == action)
            .count()
    }

    /// `true` when applying the diff would change nothing — the case a second discovery run has
    /// to reach, because a diff that re-reports itself is a diff nobody trusts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Compare what an endpoint reports against what the registry carries.
///
/// Metadata is compared field by field rather than as a whole row, so the diff says *what*
/// changed. An endpoint that lists only keys — which is most of them, including Ollama's and
/// OpenAI's own — produces no `changed` lines at all rather than claiming every model was
/// edited, because discovery knows the keys and nothing else; the capability flags stay the
/// operator's.
#[must_use]
pub fn diff_discovery(stored: &[AiModel], reported: &[String]) -> Vec<DiscoveryLine> {
    let mut reported_keys: Vec<&String> = reported.iter().collect();
    reported_keys.sort();
    reported_keys.dedup();

    let mut lines = Vec::new();
    for key in &reported_keys {
        match stored.iter().find(|model| model.model_key == key.as_str()) {
            None => lines.push(DiscoveryLine {
                model_key: (*key).clone(),
                action: DiscoveryAction::Added,
                changed_fields: Vec::new(),
            }),
            Some(model) => {
                let changed = model_diff(model, key.as_str());
                if !changed.is_empty() {
                    lines.push(DiscoveryLine {
                        model_key: (*key).clone(),
                        action: DiscoveryAction::Changed,
                        changed_fields: changed,
                    });
                }
            }
        }
    }

    for model in stored {
        if !reported_keys
            .iter()
            .any(|key| key.as_str() == model.model_key)
        {
            lines.push(DiscoveryLine {
                model_key: model.model_key.clone(),
                action: DiscoveryAction::Removed,
                changed_fields: Vec::new(),
            });
        }
    }

    lines.sort_by(|left, right| left.model_key.cmp(&right.model_key));
    lines
}

/// The fields a discovery apply would write, or nothing when the row already agrees.
///
/// A reported key carries no metadata, so this compares what the reported list *can* say: the
/// key itself, and whether the row is enabled against the fact the endpoint still serves it. The
/// capability flags are deliberately absent — they are the operator's, and an endpoint that does
/// not publish them must not be able to switch them off.
fn model_diff(model: &AiModel, _reported_key: &str) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    if !model.enabled {
        // The endpoint serves it while the registry has it switched off. Applying the diff does
        // not re-enable it — the operator decides — so this line is reported as a change of
        // *visibility* and the apply leaves the flag alone. It is listed so the panel can say
        // "served but switched off" rather than leaving the operator to notice.
        changed.push("enabled".to_owned());
    }
    changed
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

/// Check a model's answer ceiling, and its context window beside it.
///
/// Both are "how many tokens" and both are optional, so `None` passes. A ceiling of zero is
/// refused rather than stored: a model that can produce no tokens cannot answer, and a row that
/// says so would break the router at request time instead of at edit time.
pub fn validate_token_limits(
    context_window: Option<i32>,
    max_output_tokens: Option<i32>,
) -> Result<()> {
    if let Some(window) = context_window
        && window <= 0
    {
        return Err(AiHubError::InvalidModel(
            "a context window must be a positive number of tokens".to_owned(),
        ));
    }
    if let Some(ceiling) = max_output_tokens
        && ceiling <= 0
    {
        return Err(AiHubError::InvalidModel(
            "a max output token count must be a positive number of tokens".to_owned(),
        ));
    }
    // An answer longer than the context that holds it is a fact about the model, not about the
    // platform, and every provider refuses it — so the registry refuses it first, in the field
    // the operator is editing, instead of at request time.
    if let (Some(window), Some(ceiling)) = (context_window, max_output_tokens)
        && ceiling > window
    {
        return Err(AiHubError::InvalidModel(format!(
            "a max output of {ceiling} tokens cannot fit in a context window of {window}"
        )));
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
            supports_image_generation: false,
            supports_audio_generation: false,
            supports_transcription: false,
            supports_json_mode: false,
            max_output_tokens: None,
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

    /// A model with every flag off, for the capability and diff tests.
    fn bare(key: &str) -> AiModel {
        AiModel {
            id: Uuid::new_v4(),
            provider_id: Uuid::new_v4(),
            model_key: key.to_owned(),
            display_name: None,
            context_window: None,
            supports_tools: false,
            supports_vision: false,
            supports_streaming: true,
            supports_embeddings: false,
            supports_image_generation: false,
            supports_audio_generation: false,
            supports_transcription: false,
            supports_json_mode: false,
            max_output_tokens: None,
            enabled: true,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_capability_vocabulary_is_closed_and_round_trips() {
        assert_eq!(ModelCapability::ALL.len(), 10);
        for capability in ModelCapability::ALL {
            assert_eq!(
                ModelCapability::parse(capability.as_str()),
                Some(*capability)
            );
            assert!(!capability.note().is_empty(), "{capability} needs a note");
        }
        assert_eq!(ModelCapability::parse("telepathy"), None);
        assert!(
            !ModelCapability::Chat.is_model_flag(),
            "chat is true for every row"
        );
        assert!(
            !ModelCapability::ListModels.is_model_flag(),
            "list-models is a provider fact"
        );
        assert!(ModelCapability::Vision.is_model_flag());
    }

    #[test]
    fn a_models_capabilities_come_from_the_row_and_nothing_else() {
        let mut model = bare("m");
        // Chat is a fact about the row existing; list-models is a fact about the endpoint.
        assert!(model.capability(ModelCapability::Chat));
        assert!(
            model.capability(ModelCapability::Streaming),
            "the default is on"
        );
        assert!(!model.capability(ModelCapability::ListModels));
        assert!(!model.capability(ModelCapability::Vision));

        model.supports_vision = true;
        model.supports_json_mode = true;
        model.supports_image_generation = true;
        assert!(model.capability(ModelCapability::Vision));
        assert!(model.capability(ModelCapability::JsonMode));
        assert!(model.capability(ModelCapability::ImageGeneration));

        let claimed = model.capabilities();
        assert!(claimed.contains(&ModelCapability::Chat));
        assert!(claimed.contains(&ModelCapability::Vision));
        assert!(!claimed.contains(&ModelCapability::Transcription));
    }

    #[test]
    fn a_refused_capability_names_the_model_and_the_flag() {
        let model = bare("plain-model");
        let error = require_capability(&model, ModelCapability::Vision).expect_err("refused");
        assert_eq!(error.code(), "capability_unsupported");
        let message = error.to_string();
        assert!(message.contains("plain-model"), "{message}");
        assert!(message.contains("vision"), "{message}");

        assert!(require_capability(&model, ModelCapability::Chat).is_ok());
        assert!(require_capability(&model, ModelCapability::Streaming).is_ok());
    }

    #[test]
    fn token_limits_are_checked_against_each_other() {
        assert!(validate_token_limits(None, None).is_ok());
        assert!(validate_token_limits(Some(128_000), Some(4096)).is_ok());
        assert!(validate_token_limits(Some(0), None).is_err());
        assert!(validate_token_limits(None, Some(0)).is_err());
        assert!(validate_token_limits(Some(-1), Some(10)).is_err());
        let error = validate_token_limits(Some(4096), Some(8192)).expect_err("cannot fit");
        assert!(error.to_string().contains("cannot fit"), "{error}");
    }

    #[test]
    fn a_discovery_diff_says_what_would_change() {
        let stored = vec![bare("kept"), bare("gone"), bare("off")];
        let reported = vec!["kept".to_owned(), "fresh".to_owned()];

        let lines = diff_discovery(&stored, &reported);
        let by_key = |key: &str| {
            lines
                .iter()
                .find(|line| line.model_key == key)
                .map(|line| (line.action, line.changed_fields.clone()))
        };

        assert_eq!(by_key("fresh").expect("new").0, DiscoveryAction::Added);
        assert_eq!(by_key("gone").expect("dropped").0, DiscoveryAction::Removed);
        // "kept" is served and enabled, so it produces no line at all — a diff that reported
        // every agreeing model would be unreadable at two hundred rows.
        assert!(by_key("kept").is_none());
    }

    #[test]
    fn a_switched_off_model_the_endpoint_still_serves_is_reported_not_reset() {
        let mut stored = vec![bare("served")];
        stored[0].enabled = false;
        let reported = vec!["served".to_owned()];

        let lines = diff_discovery(&stored, &reported);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action, DiscoveryAction::Changed);
        assert_eq!(lines[0].changed_fields, vec!["enabled"]);
    }

    #[test]
    fn a_diff_of_the_same_endpoint_twice_is_empty() {
        let stored = vec![bare("a"), bare("b")];
        let reported = vec!["b".to_owned(), "a".to_owned()];

        assert!(diff_discovery(&stored, &reported).is_empty());
    }

    #[test]
    fn a_duplicate_report_is_one_model() {
        let reported = vec!["a".to_owned(), "a".to_owned(), "b".to_owned()];
        let lines = diff_discovery(&[], &reported);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].model_key, "a");
        assert_eq!(lines[1].model_key, "b");
    }

    #[test]
    fn a_new_model_carries_no_flag_until_asked() {
        let plain = NewAiModel::new("m");
        for capability in ModelCapability::ALL {
            if capability.is_model_flag() {
                assert_eq!(
                    plain.capability(*capability),
                    None,
                    "{capability} must stay unset by default"
                );
            }
        }

        let flagged = NewAiModel::new("m")
            .with(ModelCapability::Vision, true)
            .with(ModelCapability::Streaming, false);
        assert_eq!(flagged.supports_vision, Some(true));
        assert_eq!(flagged.supports_streaming, Some(false));
        // Chat is always true and list-models is not a model column, so asking to set them is a
        // no-op rather than a silently dropped field.
        let ignored = NewAiModel::new("m").with(ModelCapability::Chat, false);
        assert_eq!(ignored.capability(ModelCapability::Chat), Some(true));
    }
}
