//! `/api/v1/ai` — the AI Hub surface (docs/06-AI-HUB.md, phase P11).
//!
//! Three things live here, and nothing else:
//!
//! * **Providers** — the connections an installation made. Reading them is `ai.providers.read`,
//!   connecting and changing them is `ai.providers.manage`; an administrator-level power,
//!   because a provider is where the platform sends its data. The stored key goes in and is
//!   never handed back: the list answers whether a key exists, never its value.
//! * **Models** — the registry (`GET /ai/models`), the set one provider serves
//!   (`PUT /ai/providers/{id}/models`) and the discovery call against the provider itself
//!   (`POST /ai/providers/{id}/discover-models`).
//! * **Chat** — `POST /ai/chat`, guarded by `ai.chat`, answered as `text/event-stream`: a
//!   `start` frame (which provider and model the router chose), `delta` frames with the answer
//!   as it arrives, then `done` with the finish reason and the token usage — or `error` with a
//!   stable code. Every exchange is audited (`ai.chat.completed` / `ai.chat.failed`), which is
//!   the first half of docs/06 §17.
//!
//! The agents of §5 and the tools of §6 build on this surface; v0 gives them a provider that
//! answers and a chat that streams.
//!
//! v0 keeps provider connections **installation-level** (the Owner runs the platform, not one
//! tenant — see `crates/onboarding`), so the handlers carry the permission guard and no tenant
//! scope rule. The per-organization and per-site limits of §16 arrive with the cost manager,
//! which is where connections become tenant-scoped.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_ai_hub::{
    AiHubError, AiModel, ApiKeyChange, ChatEvent, ChatMessage, ChatRequest, ChatRole, MAX_PRIORITY,
    MAX_RETRIES_CEILING, MAX_TIMEOUT_MS, MIN_PRIORITY, MIN_TIMEOUT_MS, ModelCapability,
    ModelChanges, NewAiModel, NewProvider, Provider, ProviderChanges, ProviderTarget, StepStatus,
    TestReport, protocol_infos, stream_chat, test_provider,
};
use omnion_audit::NewAuditEntry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// How many events may queue in front of a client before the stream waits for it.
const STREAM_BUFFER: usize = 64;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One provider as the panel sees it. The key itself is never part of this shape.
#[derive(Debug, Serialize)]
pub struct ProviderBody {
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
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_checked_at: Option<OffsetDateTime>,
    /// What the last failed probe said.
    pub last_error: Option<String>,
    /// Whether the provider is enabled.
    pub enabled: bool,
    /// Whether it is the installation's default provider.
    pub is_default: bool,
    /// How many models the registry holds for it.
    pub model_count: usize,
    /// When it was connected.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl ProviderBody {
    /// Describe one provider, with its model count.
    fn build(provider: &Provider, model_count: usize) -> Self {
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
            model_count,
            created_at: provider.created_at,
            updated_at: provider.updated_at,
        }
    }
}

/// One model as the panel sees it, with the provider's name attached.
#[derive(Debug, Serialize)]
pub struct ModelBody {
    /// Model id.
    pub id: Uuid,
    /// Provider that serves it.
    pub provider_id: Uuid,
    /// Name of that provider.
    pub provider_name: String,
    /// Wire key.
    pub model_key: String,
    /// Name shown in the panel.
    pub display_name: String,
    /// Context window in tokens.
    pub context_window: Option<i32>,
    /// Tool calling.
    pub supports_tools: bool,
    /// Image input.
    pub supports_vision: bool,
    /// Streaming answers.
    pub supports_streaming: bool,
    /// Embedding output.
    pub supports_embeddings: bool,
    /// Image generation.
    pub supports_image_generation: bool,
    /// Audio generation.
    pub supports_audio_generation: bool,
    /// Transcription.
    pub supports_transcription: bool,
    /// Structured output through the provider's own mode.
    pub supports_json_mode: bool,
    /// Largest answer the model advertises.
    pub max_output_tokens: Option<i32>,
    /// Whether the model is enabled.
    pub enabled: bool,
    /// Whether the model is the installation's default model.
    pub is_default: bool,
    /// `provider/model` — how the router addresses this pair.
    pub model_id: String,
    /// The closed capability vocabulary, so the panel renders every flag from one list.
    pub capability_catalog: Vec<CapabilityInfo>,
    /// The capabilities this model actually claims, in catalog order.
    pub capabilities: Vec<ModelCapability>,
    /// What this model costs, with both the per-million figure the column stores and the
    /// per-1K rendering the table shows (REQ-098).
    ///
    /// Sent as one object rather than four loose fields so a client cannot read the per-1K
    /// rendering of one half and the per-million figure of the other and print them side by
    /// side as if they described the same number.
    pub price: PriceBody,
    /// Where the capability flags came from, and when they were last confirmed.
    pub capabilities_source: String,
    /// When the capability flags were last confirmed against something, when ever that was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub capabilities_verified_at: Option<OffsetDateTime>,
    /// When it was registered.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// One model's price, as the panel reads it.
///
/// The `complete` flag is what stops the catalog from quietly pretending a half-priced model is
/// fully priced: without it a client would format a missing output rate as zero and every cost
/// estimate built from the table would understate the model.
#[derive(Debug, Clone, Serialize)]
pub struct PriceBody {
    /// Micros per million input tokens.
    pub input_micros_per_mtok: Option<i64>,
    /// Micros per million output tokens.
    pub output_micros_per_mtok: Option<i64>,
    /// The input half rendered per 1K.
    pub input_micros_per_1k: Option<i64>,
    /// The output half rendered per 1K.
    pub output_micros_per_1k: Option<i64>,
    /// `manual`, `discovery` or `probe`.
    pub source: String,
    /// What that source means, so the panel does not have to hard-code the wording.
    pub source_note: String,
    /// When the price was written down.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// `true` when both halves are known.
    pub complete: bool,
    /// How old the price is in whole days; `null` when none was ever written.
    pub age_days: Option<i64>,
    /// `true` when the price is older than the platform's staleness window.
    pub stale: bool,
}

impl ModelBody {
    /// Describe one model of a known provider.
    fn build(provider: &Provider, model: &AiModel) -> Self {
        Self {
            id: model.id,
            provider_id: model.provider_id,
            provider_name: provider.name.clone(),
            model_key: model.model_key.clone(),
            display_name: model.label().to_owned(),
            context_window: model.context_window,
            supports_tools: model.supports_tools,
            supports_vision: model.supports_vision,
            supports_streaming: model.supports_streaming,
            supports_embeddings: model.supports_embeddings,
            supports_image_generation: model.supports_image_generation,
            supports_audio_generation: model.supports_audio_generation,
            supports_transcription: model.supports_transcription,
            supports_json_mode: model.supports_json_mode,
            max_output_tokens: model.max_output_tokens,
            enabled: model.enabled,
            is_default: model.is_default,
            model_id: omnion_ai_hub::model_id(provider, model),
            capability_catalog: ModelCapability::ALL
                .iter()
                .map(|capability| CapabilityInfo {
                    capability: *capability,
                    note: capability.note(),
                    editable: capability.is_model_flag(),
                })
                .collect(),
            capabilities: model.capabilities(),
            price: PriceBody::build(&model.price()),
            capabilities_source: model.capabilities_source.clone(),
            capabilities_verified_at: model.capabilities_verified_at,
            created_at: model.created_at,
            updated_at: model.updated_at,
        }
    }
}

/// One entry of the closed capability vocabulary, as the panel's flag editor reads it.
///
/// The catalog travels with the models rather than being compiled into the panel, so a flag
/// added to the crate appears in the editor without a second edit — and the `editable` marker
/// tells the panel which toggles are a model's to claim and which are facts about the endpoint.
#[derive(Debug, Serialize)]
pub struct CapabilityInfo {
    /// Wire name, used as the toggle key.
    pub capability: ModelCapability,
    /// One line the panel shows under the toggle.
    pub note: &'static str,
    /// `true` when a model row can turn this on.
    pub editable: bool,
}

/// Response of the provider list.
#[derive(Debug, Serialize)]
pub struct ProviderListResponse {
    /// Providers, in name order.
    pub providers: Vec<ProviderBody>,
}

/// Response of a model list.
#[derive(Debug, Serialize)]
pub struct ModelListResponse {
    /// Models, in key order.
    pub models: Vec<ModelBody>,
}

/// Response of a discovery run: what the endpoint serves against what the registry holds.
///
/// The endpoint's own list comes back in `reported` so an operator can see what it published,
/// and `lines` says what *applying* it would do. Nothing is written by this call — that is the
/// apply endpoint's job, and it is a separate request with a separate confirmation.
#[derive(Debug, Serialize)]
pub struct DiscoverResponse {
    /// Provider that was asked.
    pub provider_id: Uuid,
    /// Its name.
    pub provider_name: String,
    /// Model keys it reported, sorted and deduplicated.
    pub reported: Vec<String>,
    /// Model keys the registry held before this run.
    pub stored: Vec<String>,
    /// The diff, in key order.
    pub lines: Vec<omnion_ai_hub::DiscoveryLine>,
    /// How many keys the endpoint serves.
    pub reported_count: usize,
    /// How many models the registry holds for this provider.
    pub stored_count: usize,
    /// How many would be added.
    pub added: usize,
    /// How many would be removed.
    pub removed: usize,
    /// How many would change.
    pub changed: usize,
    /// `true` when applying would change nothing.
    pub up_to_date: bool,
}

impl DiscoverResponse {
    /// Describe one discovery run.
    fn build(diff: omnion_ai_hub::DiscoveryDiff) -> Self {
        use omnion_ai_hub::DiscoveryAction;

        Self {
            reported_count: diff.reported.len(),
            stored_count: diff.stored.len(),
            added: diff.count(DiscoveryAction::Added),
            removed: diff.count(DiscoveryAction::Removed),
            changed: diff.count(DiscoveryAction::Changed),
            up_to_date: diff.is_empty(),
            provider_id: diff.provider_id,
            provider_name: diff.provider_name,
            reported: diff.reported,
            stored: diff.stored,
            lines: diff.lines,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// One model in a create or replace request: a bare key, or a key with metadata.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ModelInput {
    /// Just the wire key.
    Key(String),
    /// The wire key plus whatever metadata the caller knows.
    Described {
        /// Wire key.
        key: String,
        /// Name shown in the panel.
        #[serde(default)]
        display_name: Option<String>,
        /// Context window in tokens.
        #[serde(default)]
        context_window: Option<i32>,
        /// Tool calling.
        #[serde(default)]
        supports_tools: Option<bool>,
        /// Image input.
        #[serde(default)]
        supports_vision: Option<bool>,
        /// Streaming answers.
        #[serde(default)]
        supports_streaming: Option<bool>,
        /// Embedding output.
        #[serde(default)]
        supports_embeddings: Option<bool>,
        /// Image generation.
        #[serde(default)]
        supports_image_generation: Option<bool>,
        /// Audio generation.
        #[serde(default)]
        supports_audio_generation: Option<bool>,
        /// Transcription.
        #[serde(default)]
        supports_transcription: Option<bool>,
        /// Structured output.
        #[serde(default)]
        supports_json_mode: Option<bool>,
        /// Largest answer the model advertises.
        #[serde(default)]
        max_output_tokens: Option<i32>,
    },
}

impl ModelInput {
    /// The stored shape of this input.
    fn into_new(self) -> NewAiModel {
        match self {
            Self::Key(key) => NewAiModel::new(key),
            Self::Described {
                key,
                display_name,
                context_window,
                supports_tools,
                supports_vision,
                supports_streaming,
                supports_embeddings,
                supports_image_generation,
                supports_audio_generation,
                supports_transcription,
                supports_json_mode,
                max_output_tokens,
            } => NewAiModel {
                model_key: key,
                display_name,
                context_window,
                supports_tools,
                supports_vision,
                supports_streaming,
                supports_embeddings,
                supports_image_generation,
                supports_audio_generation,
                supports_transcription,
                supports_json_mode,
                max_output_tokens,
            },
        }
    }
}

/// Body of `POST /ai/providers`.
#[derive(Debug, Deserialize)]
pub struct CreateProviderBody {
    /// Display name.
    pub name: String,
    /// Wire protocol; the OpenAI-compatible one when absent.
    pub protocol: Option<String>,
    /// `cloud` or `local`; `cloud` when absent.
    pub kind: Option<String>,
    /// Base URL, version segment included.
    pub base_url: String,
    /// Key to authenticate with; absent for a local endpoint that wants none.
    pub api_key: Option<String>,
    /// How long one call may take, in milliseconds (default 30000).
    pub timeout_ms: Option<i32>,
    /// How often a pre-first-byte failure is retried (default 1).
    pub max_retries: Option<i32>,
    /// Position in the failover chain (default 100).
    pub priority: Option<i32>,
    /// Whether the provider starts enabled (default `true`).
    pub enabled: Option<bool>,
    /// Whether it becomes the installation's default provider.
    pub is_default: Option<bool>,
    /// Models it serves; keys or described models.
    #[serde(default)]
    pub models: Vec<ModelInput>,
}

/// Body of `PATCH /ai/providers/{id}`.
///
/// `api_key` distinguishes three cases, exactly as the panel needs them: absent keeps the
/// stored key, `null` forgets it, a string replaces it.
#[derive(Debug, Deserialize)]
pub struct UpdateProviderBody {
    /// New display name.
    pub name: Option<String>,
    /// New base URL.
    pub base_url: Option<String>,
    /// Absent = keep, `null` = clear, string = replace.
    #[serde(default, deserialize_with = "double_option")]
    pub api_key: Option<Option<String>>,
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
    /// `true` makes it the installation's default provider.
    pub is_default: Option<bool>,
}

/// Body of `PUT /ai/providers/{id}/models`.
#[derive(Debug, Deserialize)]
pub struct ReplaceModelsBody {
    /// Models the provider serves; the set replaces what is stored.
    pub models: Vec<ModelInput>,
}

/// Body of `PATCH /ai/models/{id}`.
///
/// `max_output_tokens` distinguishes three cases the way the provider key does: absent keeps the
/// stored ceiling, `null` forgets it, a number replaces it.
#[derive(Debug, Deserialize)]
pub struct UpdateModelBody {
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// `true` makes this the installation's default model.
    pub is_default: Option<bool>,
    /// New display name.
    pub display_name: Option<String>,
    /// New context window in tokens.
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
    /// Absent = keep, `null` = clear, number = replace.
    #[serde(default, deserialize_with = "double_option_i32")]
    pub max_output_tokens: Option<Option<i32>>,
    /// Absent = keep, `null` = clear, number = replace (REQ-098 slice 1).
    #[serde(default, deserialize_with = "double_option_i64")]
    pub input_cost_micros_per_mtok: Option<Option<i64>>,
    /// Absent = keep, `null` = clear, number = replace.
    #[serde(default, deserialize_with = "double_option_i64")]
    pub output_cost_micros_per_mtok: Option<Option<i64>>,
    /// New price source: `manual`, `discovery` or `probe`.
    ///
    /// Optional rather than defaulted because a PATCH that says nothing about the source must not
    /// restamp one: setting it implicitly on every flag toggle would make a capability edit
    /// silently claim the price had been re-verified today, which is the one claim this column
    /// exists to keep honest.
    pub price_source: Option<String>,
    /// New capability source: `manual`, `discovery` or `probe`.
    pub capabilities_source: Option<String>,
    /// When the capability flags were last confirmed against something.
    pub capabilities_verified_at: Option<OffsetDateTime>,
}

/// Read `null` as "forget this price", an absent field as "leave it".
fn double_option_i64<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<i64>::deserialize(deserializer).map(Some)
}

/// Read `null` as "forget this limit", an absent field as "leave it".
fn double_option_i32<'de, D>(deserializer: D) -> Result<Option<Option<i32>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<i32>::deserialize(deserializer).map(Some)
}

impl PriceBody {
    /// Describe one model's price, with its age measured against the clock.
    ///
    /// The age is computed here rather than in the panel so the staleness window lives next to
    /// the number it judges: a client with its own copy of the threshold would drift from the
    /// server's the first time somebody tuned it, and the two would disagree about whether a
    /// price is old.
    fn build(price: &omnion_ai_hub::ModelPrice) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            input_micros_per_mtok: price.input_micros_per_mtok,
            output_micros_per_mtok: price.output_micros_per_mtok,
            input_micros_per_1k: price.per_1k_micros(omnion_ai_hub::PriceHalf::Input),
            output_micros_per_1k: price.per_1k_micros(omnion_ai_hub::PriceHalf::Output),
            source: price.source.as_str().to_owned(),
            source_note: price.source.note().to_owned(),
            updated_at: price.updated_at,
            complete: price.is_complete(),
            age_days: omnion_ai_hub::price_age_days(price.updated_at, now),
            stale: omnion_ai_hub::price_is_stale(price.updated_at, now),
        }
    }
}

/// Query of `GET /ai/models`.
///
/// Every narrowing is optional and independent, so a caller may send any combination — and a
/// caller that sends none gets the whole registry, which is what the panel asks for on load.
#[derive(Debug, Deserialize)]
pub struct ModelQuery {
    /// Narrow the list to one provider.
    pub provider_id: Option<Uuid>,
    /// Free text, matched against the model key, the display name and the provider name.
    pub q: Option<String>,
    /// Comma-separated capability flags a row must **all** claim (REQ-098).
    pub capability: Option<String>,
    /// `enabled` or `disabled`.
    pub status: Option<String>,
    /// Which column the table is sorted by; an unknown key falls back to `model`.
    pub sort: Option<String>,
}

/// One message of a chat request.
#[derive(Debug, Deserialize)]
pub struct ChatInputMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// What was said.
    pub content: String,
}

/// Body of `POST /ai/chat`.
#[derive(Debug, Deserialize)]
pub struct ChatBody {
    /// Model to use — `provider/model` or a bare key; the default model when absent.
    pub model: Option<String>,
    /// Conversation so far.
    pub messages: Vec<ChatInputMessage>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Answer budget in tokens.
    pub max_tokens: Option<u32>,
}

/// Read `null` as "clear this", an absent field as "leave it".
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

// ---------------------------------------------------------------------------------------------
// Handlers — providers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/providers`.
pub async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<ProviderListResponse>, ApiError> {
    let providers = omnion_ai_hub::list_providers(state.db().pool()).await?;
    let models = omnion_ai_hub::list_models(state.db().pool(), None).await?;

    Ok(Json(ProviderListResponse {
        providers: providers
            .iter()
            .map(|provider| {
                let count = models
                    .iter()
                    .filter(|model| model.provider_id == provider.id)
                    .count();
                ProviderBody::build(provider, count)
            })
            .collect(),
    }))
}

/// `POST /api/v1/ai/providers`.
pub async fn create_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateProviderBody>,
) -> Result<(StatusCode, Json<ProviderBody>), ApiError> {
    let protocol = body
        .protocol
        .unwrap_or_else(|| omnion_ai_hub::DEFAULT_PROTOCOL.to_owned());

    let provider = omnion_ai_hub::create_provider(
        state.db().pool(),
        NewProvider {
            name: body.name,
            protocol,
            kind: body.kind.unwrap_or_else(|| "cloud".to_owned()),
            base_url: body.base_url,
            api_key: body.api_key,
            timeout_ms: body.timeout_ms.unwrap_or(30_000),
            max_retries: body.max_retries.unwrap_or(1),
            priority: body.priority.unwrap_or(100),
            enabled: body.enabled.unwrap_or(true),
            is_default: body.is_default.unwrap_or(false),
        },
    )
    .await?;

    let models = if body.models.is_empty() {
        Vec::new()
    } else {
        let models: Vec<NewAiModel> = body.models.into_iter().map(ModelInput::into_new).collect();
        omnion_ai_hub::replace_models(state.db().pool(), provider.id, models).await?
    };

    let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.connected")
        .organization(current.user.organization_id)
        .target("ai_provider", provider.id.to_string())
        .metadata(json!({
            "name": provider.name,
            "protocol": provider.protocol,
            "kind": provider.kind,
            "base_url": provider.base_url,
            "has_api_key": provider.has_api_key(),
            "models": models.len(),
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok((
        StatusCode::CREATED,
        Json(ProviderBody::build(&provider, models.len())),
    ))
}

/// `PATCH /api/v1/ai/providers/{id}`.
pub async fn update_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProviderBody>,
) -> Result<Json<ProviderBody>, ApiError> {
    let api_key = match body.api_key {
        None => ApiKeyChange::Keep,
        Some(None) => ApiKeyChange::Clear,
        Some(Some(key)) => ApiKeyChange::Set(key),
    };

    let provider = omnion_ai_hub::update_provider(
        state.db().pool(),
        id,
        ProviderChanges {
            name: body.name,
            base_url: body.base_url,
            api_key,
            kind: body.kind,
            timeout_ms: body.timeout_ms,
            max_retries: body.max_retries,
            priority: body.priority,
            enabled: body.enabled,
            is_default: body.is_default,
        },
    )
    .await?;

    let models = omnion_ai_hub::list_models(state.db().pool(), Some(provider.id)).await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.updated")
        .organization(current.user.organization_id)
        .target("ai_provider", provider.id.to_string())
        .metadata(json!({
            "name": provider.name,
            "enabled": provider.enabled,
            "is_default": provider.is_default,
            "kind": provider.kind,
            "timeout_ms": provider.timeout_ms,
            "max_retries": provider.max_retries,
            "priority": provider.priority,
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(ProviderBody::build(&provider, models.len())))
}

/// `DELETE /api/v1/ai/providers/{id}`.
pub async fn delete_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    omnion_ai_hub::delete_provider(state.db().pool(), id).await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.removed")
        .organization(current.user.organization_id)
        .target("ai_provider", provider.id.to_string())
        .metadata(json!({ "name": provider.name, "base_url": provider.base_url }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Handlers — models
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/models` — the model registry, narrowed (REQ-098 slice 1).
///
/// The narrowing happens here rather than in the panel so the API and the table agree about
/// which rows a query means: a client that filtered client-side would see a different set than
/// the server describes, and the acceptance criterion that the capability chips narrow the
/// *listing* would be true of the panel but not of the endpoint.
pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ModelQuery>,
) -> Result<Json<ModelListResponse>, ApiError> {
    let providers = omnion_ai_hub::list_providers(state.db().pool()).await?;
    let all = omnion_ai_hub::list_models(state.db().pool(), query.provider_id).await?;

    // An unknown capability key or an unknown status is a refusal, not a silently empty list: a
    // caller that misspells a filter and gets `[]` cannot tell a broken query from an empty
    // registry, and will conclude the wrong one.
    let capabilities =
        omnion_ai_hub::CatalogQuery::capabilities_from_param(query.capability.as_deref())?;
    let status = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some("enabled") | Some("active") => Some(true),
        Some("disabled") | Some("inactive") => Some(false),
        Some(other) => {
            return Err(ApiError::bad_request(
                "invalid_status",
                format!("status \"{other}\" is not one of (enabled, disabled)"),
            ));
        }
    };

    let query = omnion_ai_hub::CatalogQuery {
        q: query.q,
        capabilities,
        provider_id: query.provider_id,
        status,
        sort: omnion_ai_hub::CatalogSort::parse(query.sort.as_deref().unwrap_or_default()),
    };

    // The narrowing runs against the **stored** model rather than the response body, because the
    // capability flags and the enabled flag the filter reads are the crate's own fields. Reading
    // them off a `ModelBody` would make the filter a second implementation of the flags that
    // could disagree with the router's — and a disagreement here means a table that offers a
    // model the router will refuse.
    let provider_name_of = |id: Uuid| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .map(|provider| provider.name.clone())
            .unwrap_or_default()
    };
    let kept: Vec<Uuid> = all
        .iter()
        // A model whose provider row is missing cannot happen (the foreign key cascades), but
        // the fallback is the empty string rather than a panic: a text search that finds nothing
        // is an absent row, not a 500.
        .filter(|model| query.matches(model, &provider_name_of(model.provider_id)))
        .map(|model| model.id)
        .collect();

    let mut models: Vec<ModelBody> = models_in_provider_order(&providers, &all)
        .into_iter()
        .filter(|model| kept.contains(&model.id))
        .collect();

    sort_catalog(&mut models, &providers, query.sort);

    Ok(Json(ModelListResponse { models }))
}

/// Order a narrowed catalog the way the table's header says it is.
///
/// Done after the narrowing so the text search and the capability filter see the registry in
/// provider order — the order the empty state and the row numbering read from — and so the
/// `nulls last` rules live in one function rather than in two orderings that can disagree.
fn sort_catalog(models: &mut [ModelBody], providers: &[Provider], sort: omnion_ai_hub::CatalogSort) {
    let name_of = |id: Uuid| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .map(|provider| provider.name.clone())
            .unwrap_or_default()
    };

    match sort {
        omnion_ai_hub::CatalogSort::Model => {
            models.sort_by(|left, right| {
                left.model_key
                    .cmp(&right.model_key)
                    .then_with(|| name_of(left.provider_id).cmp(&name_of(right.provider_id)))
            });
        }
        omnion_ai_hub::CatalogSort::Provider => {
            models.sort_by(|left, right| {
                name_of(left.provider_id)
                    .cmp(&name_of(right.provider_id))
                    .then_with(|| left.model_key.cmp(&right.model_key))
            });
        }
        omnion_ai_hub::CatalogSort::Context => {
            // A model whose window nobody recorded sorts last rather than first: an unknown
            // window is not the smallest one, and putting the least-known row at the top of a
            // column an operator reads to find the model that can hold a document inverts it.
            models.sort_by(|left, right| {
                right
                    .context_window
                    .cmp(&left.context_window)
                    .then_with(|| left.model_key.cmp(&right.model_key))
            });
        }
        omnion_ai_hub::CatalogSort::Price => {
            models.sort_by(|left, right| {
                // `cmp_price`, not the raw `Option::cmp`: the derived ordering ranks `None`
                // **first**, which would print every unpriced model as the cheapest one in a
                // column the operator reads to answer "what can I afford". The `nulls last`
                // in `CatalogSort::order_by` says the same thing for the SQL path, so both
                // orderings now express one rule instead of two that can disagree.
                omnion_ai_hub::cmp_price(
                    left.price.input_micros_per_mtok,
                    right.price.input_micros_per_mtok,
                )
                .then_with(|| {
                    omnion_ai_hub::cmp_price(
                        left.price.output_micros_per_mtok,
                        right.price.output_micros_per_mtok,
                    )
                })
                .then_with(|| left.model_key.cmp(&right.model_key))
            });
        }
        omnion_ai_hub::CatalogSort::Updated => {
            models.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| left.model_key.cmp(&right.model_key))
            });
        }
    }
}

/// `PUT /api/v1/ai/providers/{id}/models` — replace the set one provider serves.
pub async fn replace_provider_models(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<ReplaceModelsBody>,
) -> Result<Json<ModelListResponse>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let models: Vec<NewAiModel> = body.models.into_iter().map(ModelInput::into_new).collect();
    let stored = omnion_ai_hub::replace_models(state.db().pool(), provider.id, models).await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.models_replaced")
        .organization(current.user.organization_id)
        .target("ai_provider", provider.id.to_string())
        .metadata(json!({
            "name": provider.name,
            "models": stored.iter().map(|model| model.model_key.clone()).collect::<Vec<_>>(),
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(ModelListResponse {
        models: stored
            .iter()
            .map(|model| ModelBody::build(&provider, model))
            .collect(),
    }))
}

/// `POST /api/v1/ai/providers/{id}/discover-models` — ask the provider which models it serves.
///
/// The endpoint is dialled and its list compared with the registry; **nothing is written**. The
/// panel renders the diff and the operator confirms by calling the apply route, so an endpoint
/// that suddenly reports two hundred models cannot rewrite the registry by being asked a
/// question.
pub async fn discover_provider_models(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<DiscoverResponse>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let target = ProviderTarget::from_provider(&provider);
    let reported = omnion_ai_hub::list_remote_models(&target).await?;
    let diff = omnion_ai_hub::discovery_diff(state.db().pool(), &provider, &reported).await?;

    Ok(Json(DiscoverResponse::build(diff)))
}

/// `POST /api/v1/ai/providers/{id}/apply-discovery` — apply the diff a discovery run reported.
///
/// The apply reconciles **keys** and nothing else: what the endpoint serves is added, what it
/// stopped serving is removed, and every row that survives keeps the capability flags the
/// operator gave it. Running it twice is a no-op, which is what makes a second discovery run
/// report an empty diff.
pub async fn apply_provider_discovery(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<DiscoverResponse>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let target = ProviderTarget::from_provider(&provider);
    let reported = omnion_ai_hub::list_remote_models(&target).await?;
    let diff = omnion_ai_hub::apply_discovery(state.db().pool(), &provider, &reported).await?;

    let models = omnion_ai_hub::list_models(state.db().pool(), Some(provider.id)).await?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.discovery_applied")
        .organization(current.user.organization_id)
        .target("ai_provider", provider.id.to_string())
        .metadata(json!({
            "name": provider.name,
            "added": diff.count(omnion_ai_hub::DiscoveryAction::Added),
            "removed": diff.count(omnion_ai_hub::DiscoveryAction::Removed),
            "changed": diff.count(omnion_ai_hub::DiscoveryAction::Changed),
            "models": models.len(),
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(DiscoverResponse::build(diff)))
}

/// `PATCH /api/v1/ai/models/{id}`.
pub async fn update_model(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateModelBody>,
) -> Result<Json<ModelBody>, ApiError> {
    let model = omnion_ai_hub::update_model(
        state.db().pool(),
        id,
        ModelChanges {
            enabled: body.enabled,
            is_default: body.is_default,
            display_name: body.display_name,
            context_window: body.context_window,
            supports_tools: body.supports_tools,
            supports_vision: body.supports_vision,
            supports_streaming: body.supports_streaming,
            supports_embeddings: body.supports_embeddings,
            supports_image_generation: body.supports_image_generation,
            supports_audio_generation: body.supports_audio_generation,
            supports_transcription: body.supports_transcription,
            supports_json_mode: body.supports_json_mode,
            max_output_tokens: body.max_output_tokens,
            input_cost_micros_per_mtok: body.input_cost_micros_per_mtok,
            output_cost_micros_per_mtok: body.output_cost_micros_per_mtok,
            price_source: body.price_source,
            capabilities_source: body.capabilities_source,
            capabilities_verified_at: body.capabilities_verified_at,
        },
    )
    .await?;

    let provider = omnion_ai_hub::find_provider(state.db().pool(), model.provider_id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let entry = NewAuditEntry::by_user(current.user.id, "ai.model.updated")
        .organization(current.user.organization_id)
        .target("ai_model", model.id.to_string())
        .metadata(json!({
            "provider": provider.name,
            "model": model.model_key,
            "enabled": model.enabled,
            "is_default": model.is_default,
            "capabilities": model.capabilities(),
            "context_window": model.context_window,
            "max_output_tokens": model.max_output_tokens,
            // The price is in the audit trail because a price edit is the change an operator
            // would want explained six months later: "why did last month's bill look like that"
            // is answered by this row and by nothing else.
            "input_cost_micros_per_mtok": model.input_cost_micros_per_mtok,
            "output_cost_micros_per_mtok": model.output_cost_micros_per_mtok,
            "price_source": model.price_source,
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(ModelBody::build(&provider, &model)))
}

// ---------------------------------------------------------------------------------------------
// Handlers — protocols and the connection test
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/protocols` — what the provider form offers.
///
/// The form's select is driven by this call rather than by a list compiled into the panel, so a
/// protocol added to the crate appears in the UI without a second edit, and the ranges the form
/// validates against come from the same place the API validates against.
pub async fn list_protocols(
    State(_state): State<AppState>,
) -> Result<Json<ProtocolListResponse>, ApiError> {
    Ok(Json(ProtocolListResponse {
        protocols: protocol_infos()
            .iter()
            .map(|info| ProtocolBody {
                protocol: info.protocol,
                note: info.note,
                chat_path: info.chat_path,
                auth: info.auth,
            })
            .collect(),
        bounds: ProtocolBounds {
            timeout_ms_min: MIN_TIMEOUT_MS,
            timeout_ms_max: MAX_TIMEOUT_MS,
            max_retries_max: MAX_RETRIES_CEILING,
            priority_min: MIN_PRIORITY,
            priority_max: MAX_PRIORITY,
        },
    }))
}

/// `POST /api/v1/ai/providers/{id}/test` — the connection test, run server-side.
///
/// The five steps report individually, with the provider's own message on a failure (clipped, and
/// with anything key-shaped stripped). A failing test records the verdict on the provider row and
/// writes an `ai.provider.test_failed` audit entry; a passing one clears the stored error.
pub async fn test_provider_connection(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<TestReport>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let known: Vec<String> = omnion_ai_hub::list_models(state.db().pool(), Some(id))
        .await?
        .into_iter()
        .map(|model| model.model_key)
        .collect();

    let report = test_provider(&provider, &known).await;
    let status = if report.ok { "ok" } else { "down" };
    let error = report
        .failing_step
        .as_ref()
        .and_then(|_| {
            report
                .steps
                .iter()
                .find(|step| matches!(step.status, StepStatus::Failed))
        })
        .and_then(|step| step.error.clone());
    // A test that failed is evidence, not noise: the verdict is stored so the list can show it,
    // and a passing test clears the previous failure instead of leaving a stale error behind.
    let _ = omnion_ai_hub::record_health(state.db().pool(), id, status, 0, error.as_deref()).await;

    if !report.ok {
        let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.test_failed")
            .organization(current.user.organization_id)
            .target("ai_provider", provider.id.to_string())
            .metadata(json!({
                "name": provider.name,
                "failing_step": report.failing_step,
                "error": error,
            }))
            .ip_address(address.as_text());
        omnion_audit::record(state.db().pool(), entry).await?;
    }

    Ok(Json(report))
}

/// One protocol the installation can connect, as the form reads it.
#[derive(Debug, Serialize)]
pub struct ProtocolBody {
    /// Protocol key as it is stored and sent.
    pub protocol: &'static str,
    /// One line about what the protocol covers.
    pub note: &'static str,
    /// Where a call goes, relative to the base URL.
    pub chat_path: &'static str,
    /// How the key is sent.
    pub auth: &'static str,
}

/// The ranges the provider form validates against, from the same constants the API uses.
#[derive(Debug, Serialize)]
pub struct ProtocolBounds {
    /// Smallest accepted timeout.
    pub timeout_ms_min: i32,
    /// Largest accepted timeout.
    pub timeout_ms_max: i32,
    /// Largest accepted retry ceiling.
    pub max_retries_max: i32,
    /// Lowest accepted priority.
    pub priority_min: i32,
    /// Highest accepted priority.
    pub priority_max: i32,
}

/// Response of `GET /ai/protocols`.
#[derive(Debug, Serialize)]
pub struct ProtocolListResponse {
    /// The protocols the form offers.
    pub protocols: Vec<ProtocolBody>,
    /// The numeric bounds the form validates against.
    pub bounds: ProtocolBounds,
}

// ---------------------------------------------------------------------------------------------
// Handlers — health, usage and the failover chain
// ---------------------------------------------------------------------------------------------

/// The window the Health and Usage tabs read, when the caller does not name one.
const DEFAULT_WINDOW_HOURS: i64 = 24;

/// Every window the tabs offer. A caller may ask for any of them; a caller that asks for
/// something absurd is answered from the disk rather than refused, because "show me a year of
/// health" is a real question an operator asks while a provider is misbehaving.
const WINDOW_CHOICES: &[(&str, i64)] = &[
    ("1h", 1),
    ("6h", 6),
    ("24h", 24),
    ("7d", 24 * 7),
    ("30d", 24 * 30),
];

/// `GET /api/v1/ai/providers/{id}/health?window=24h` — the Health tab in one call.
///
/// The header, the samples and the sparkline come from a single request on purpose: three calls
/// would let the header and the list describe two different moments, and a panel whose uptime
/// disagrees with the samples under it is a panel nobody trusts during an incident.
#[derive(Debug, Deserialize)]
pub struct HealthQuery {
    /// The window key from [`WINDOW_CHOICES`]; an unknown key falls back to 24 h.
    #[serde(default)]
    pub window: Option<String>,
}

/// The resolved window, echoed back so the client renders the same label the server used.
#[derive(Debug, Serialize)]
pub struct HealthView {
    /// Provider the view is about.
    pub provider_id: Uuid,
    /// Provider name, for the tab header.
    pub provider_name: String,
    /// The window key that was applied.
    pub window: &'static str,
    /// The computed status and its numbers.
    pub summary: omnion_ai_hub::health_store::HealthSummary,
    /// The recent samples, newest first.
    pub samples: Vec<omnion_ai_hub::health_store::HealthSample>,
    /// The window keys the tab offers.
    pub windows: Vec<&'static str>,
}

/// `GET /api/v1/ai/providers/{id}/health`.
pub async fn provider_health(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<HealthView>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;
    let (key, hours) = resolve_window(query.window.as_deref());

    Ok(Json(HealthView {
        provider_id: provider.id,
        provider_name: provider.name,
        window: key,
        summary: omnion_ai_hub::health_store::health_summary(state.db().pool(), id, hours).await?,
        samples: omnion_ai_hub::health_store::recent_samples(state.db().pool(), id, hours, 50)
            .await?,
        windows: WINDOW_CHOICES.iter().map(|(key, _)| *key).collect(),
    }))
}

/// `GET /api/v1/ai/providers/{id}/usage?window=24h` — the Usage tab in one call.
pub async fn provider_usage(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<UsageView>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;
    let (key, hours) = resolve_window(query.window.as_deref());

    Ok(Json(UsageView {
        provider_id: provider.id,
        provider_name: provider.name,
        window: key,
        summary: omnion_ai_hub::health_store::usage_summary(state.db().pool(), id, hours).await?,
        windows: WINDOW_CHOICES.iter().map(|(key, _)| *key).collect(),
    }))
}

/// The Usage tab's payload: the totals and the per-day breakdown, in one call.
#[derive(Debug, Serialize)]
pub struct UsageView {
    /// Provider the view is about.
    pub provider_id: Uuid,
    /// Provider name, for the tab header.
    pub provider_name: String,
    /// The window key that was applied.
    pub window: &'static str,
    /// Totals over the window.
    pub summary: omnion_ai_hub::health_store::UsageSummary,
    /// The window keys the tab offers.
    pub windows: Vec<&'static str>,
}

/// `POST /api/v1/ai/providers/{id}/probe` — "Probe now": exactly one sample, taken now.
///
/// This is the same [`probe_now`](omnion_ai_hub::health_store::probe_now) the background runner
/// calls, so the button and the tick cannot disagree about what a probe is. It runs the stored
/// provider's own connection test and records what that saw — a sample with the endpoint's own
/// words, not a synthetic "the button was pressed" row.
pub async fn probe_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<ProbeOutcome>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let known: Vec<String> = omnion_ai_hub::list_models(state.db().pool(), Some(id))
        .await?
        .into_iter()
        .map(|model| model.model_key)
        .collect();
    let report = omnion_ai_hub::test_provider(&provider, &known).await;

    let failing = report
        .steps
        .iter()
        .find(|step| matches!(step.status, StepStatus::Failed));
    let sample = omnion_ai_hub::health_store::NewSample {
        provider_id: id,
        ok: report.ok,
        // The report is a whole five-step test; its total is the honest cost of the probe, and a
        // sample that reported 0 ms would make every p95 a lie.
        latency_ms: report.total_ms.clamp(0, i32::MAX as i64) as i32,
        http_status: None,
        error: failing
            .and_then(|step| step.error.clone())
            .or_else(|| (!report.ok).then(|| report.summary.clone())),
    };

    let transition = omnion_ai_hub::health_store::probe_now(state.db().pool(), id, sample).await?;
    let summary = omnion_ai_hub::health_store::health_summary(state.db().pool(), id, 24).await?;

    // The status changed, so the event fires — the same trigger a background transition emits, so
    // an automation on "a provider went down" cannot tell the button from the tick.
    if let Some((from, to)) = &transition {
        let entry = NewAuditEntry::by_user(current.user.id, "ai.provider.health_changed")
            .organization(current.user.organization_id)
            .target("ai_provider", provider.id.to_string())
            .metadata(json!({
                "name": provider.name,
                "from": from.as_str(),
                "to": to.as_str(),
                "source": "manual_probe",
            }))
            .ip_address(address.as_text());
        omnion_audit::record(state.db().pool(), entry).await?;
    }

    let failing_step = report.failing_step.clone();
    Ok(Json(ProbeOutcome {
        provider_id: provider.id,
        ok: report.ok,
        latency_ms: report.total_ms,
        failing_step,
        transition: transition
            .as_ref()
            .map(|(from, to)| json!({ "from": from.as_str(), "to": to.as_str() })),
        summary,
        report,
    }))
}

/// What "Probe now" answers with: the sample's own outcome, the transition it caused (if any) and
/// the refreshed header the tab swaps in — no reload, because the caller already has everything.
#[derive(Debug, Serialize)]
pub struct ProbeOutcome {
    /// Provider that was probed.
    pub provider_id: Uuid,
    /// Whether the endpoint answered.
    pub ok: bool,
    /// How long the probe took.
    pub latency_ms: i64,
    /// The step that failed, when one did.
    pub failing_step: Option<String>,
    /// The status transition, when the status actually changed.
    pub transition: Option<serde_json::Value>,
    /// The header as it reads after the probe.
    pub summary: omnion_ai_hub::health_store::HealthSummary,
    /// The full five-step report.
    pub report: TestReport,
}

/// `GET /api/v1/ai/failover` — the chain as the router walks it right now.
pub async fn failover_chain(State(state): State<AppState>) -> Result<Json<FailoverView>, ApiError> {
    let chain = omnion_ai_hub::health_store::failover_preview(state.db().pool()).await?;
    Ok(Json(FailoverView {
        chain,
        // Every enabled provider, so the panel can offer a row the operator forgot to rank
        // instead of leaving it unreachable from the order editor.
        providers: omnion_ai_hub::health_store::enabled_providers(state.db().pool()).await?,
    }))
}

/// The chain preview and the membership it is drawn from.
#[derive(Debug, Serialize)]
pub struct FailoverView {
    /// The ordered chain.
    pub chain: Vec<omnion_ai_hub::health_store::FailoverEntry>,
    /// Every provider the chain may contain, in order.
    pub providers: Vec<Uuid>,
}

/// Body of `PUT /api/v1/ai/failover`.
#[derive(Debug, Deserialize)]
pub struct FailoverOrderBody {
    /// The provider ids, in the order a request should try them.
    pub provider_ids: Vec<Uuid>,
}

/// `PUT /api/v1/ai/failover` — persist the failover order.
///
/// The store rejects an empty list, a repeat and a stranger, so this handler does not re-check
/// them: a validation that exists twice is a validation that will disagree with itself. What the
/// handler adds is the audit entry and the answer, which is the chain as it now stands — the
/// client renders the server's order rather than its own guess at it.
pub async fn set_failover_order(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<FailoverOrderBody>,
) -> Result<Json<FailoverView>, ApiError> {
    omnion_ai_hub::health_store::set_failover_order(state.db().pool(), &body.provider_ids).await?;

    let view = FailoverView {
        chain: omnion_ai_hub::health_store::failover_preview(state.db().pool()).await?,
        providers: omnion_ai_hub::health_store::enabled_providers(state.db().pool()).await?,
    };

    let names: Vec<&str> = view.chain.iter().map(|entry| entry.name.as_str()).collect();
    let entry = NewAuditEntry::by_user(current.user.id, "ai.failover.reordered")
        .organization(current.user.organization_id)
        .target("ai_failover", "chain".to_owned())
        .metadata(json!({ "order": names }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(view))
}

/// Resolve a window key to `(label, hours)`; an unknown or missing key is the 24 h default.
///
/// The label is returned as a `&'static str` from the same table the hours came from, so a client
/// can never render a window the server did not actually apply.
fn resolve_window(window: Option<&str>) -> (&'static str, i64) {
    let key = window.unwrap_or("24h");
    WINDOW_CHOICES
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .copied()
        .unwrap_or(("24h", DEFAULT_WINDOW_HOURS))
}

// ---------------------------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------------------------

/// One frame of the chat stream, before it becomes an SSE event.
enum Frame {
    /// The router picked a pair; the answer is about to start.
    Start {
        provider: String,
        model: String,
        protocol: String,
    },
    /// A piece of the answer.
    Delta(String),
    /// The answer finished.
    Done {
        finish_reason: Option<String>,
        chars: usize,
        usage: Option<omnion_ai_hub::ChatUsage>,
    },
    /// The answer failed; `code` is stable, `message` is for a person.
    Failed { code: &'static str, message: String },
}

impl Frame {
    /// The SSE event a client reads.
    fn event(self) -> Result<Event, Infallible> {
        let event = match self {
            Self::Start {
                provider,
                model,
                protocol,
            } => Event::default().event("start").data(
                json!({ "provider": provider, "model": model, "protocol": protocol }).to_string(),
            ),
            Self::Delta(content) => Event::default()
                .event("delta")
                .data(json!({ "content": content }).to_string()),
            Self::Done {
                finish_reason,
                chars,
                usage,
            } => Event::default().event("done").data(
                json!({
                    "finish_reason": finish_reason,
                    "chars": chars,
                    "usage": usage.map(|usage| json!({
                        "prompt_tokens": usage.prompt_tokens,
                        "completion_tokens": usage.completion_tokens,
                        "total_tokens": usage.total_tokens,
                    })),
                })
                .to_string(),
            ),
            Self::Failed { code, message } => Event::default()
                .event("error")
                .data(json!({ "code": code, "message": message }).to_string()),
        };

        Ok(event)
    }
}

/// `POST /api/v1/ai/chat` — a streamed answer.
///
/// Everything that can be decided before the first byte (routing, permissions, the request
/// shape) is decided here and answers with a normal HTTP status. Everything after that travels
/// as `error` frames, because a stream that already started cannot take its status back.
pub async fn chat(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<ChatBody>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let mut messages = Vec::with_capacity(body.messages.len());
    for message in body.messages {
        let role = ChatRole::parse(&message.role)?;
        messages.push(ChatMessage {
            role,
            content: message.content,
        });
    }

    // Everything decidable before the first byte is decided here, with a normal HTTP status: a
    // stream asked of a model that cannot stream is refused with 400 and the model's key in the
    // message, rather than opening a stream that can only end in an error frame.
    let resolved = omnion_ai_hub::resolve_for(
        state.db().pool(),
        body.model.as_deref(),
        &[ModelCapability::Chat, ModelCapability::Streaming],
    )
    .await?;
    let request = ChatRequest {
        model: resolved.model.model_key.clone(),
        messages,
        temperature: body.temperature,
        max_tokens: body.max_tokens,
    };
    // The request shape is checked before the stream opens, so a bad one answers 400.
    omnion_ai_hub::validate_request(&request)?;

    // The chain this call may walk. A request that named `provider/model` is pinned: the plan
    // carries that one provider and no successor, so the walk below cannot move it — it is the
    // decision itself, not a flag the walk re-decides. A request that named only a task gets the
    // enabled providers in the order the Failover panel draws.
    let pinned = omnion_ai_hub::pinned_provider(state.db().pool(), body.model.as_deref()).await?;
    let all = omnion_ai_hub::store::failover_chain(state.db().pool()).await?;
    let providers = match omnion_ai_hub::plan(&all, pinned, omnion_ai_hub::Progress::Nothing) {
        omnion_ai_hub::Plan::Pinned { .. } => vec![resolved.provider.clone()],
        omnion_ai_hub::Plan::Chain { .. } | omnion_ai_hub::Plan::Exhausted { .. } => all,
    };

    let target = ProviderTarget::from_provider(&resolved.provider);
    let provider_id = resolved.provider.id;
    let model_key = resolved.model.model_key.clone();
    let model_id = resolved.id();
    let pool = state.db().pool().clone();
    let user_id = current.user.id;
    let organization_id = current.user.organization_id;
    let ip_address = address.as_text();

    let (frames, receiver) = mpsc::channel::<Frame>(STREAM_BUFFER);

    tokio::spawn(async move {
        // The failover walk (REQ-097, slice 3). A failure *before the first streamed byte* is
        // retried against the next provider in the chain — and only when the request named no
        // provider, because a pinned request puts exactly one entry in `candidates` and the walk
        // below therefore has nowhere to move to.
        let mut attempts: Vec<omnion_ai_hub::Attempt> = Vec::new();
        let mut current = target;
        let mut served_by = provider_id;
        let mut outcome = None;
        let started = Instant::now();

        loop {
            let attempt = omnion_ai_hub::Attempt {
                provider_id: current.id,
                provider_name: current.name.clone(),
                error: None,
            };

            // The first byte is the boundary failover may act on: after it the caller has seen
            // part of an answer, and a replay would be a second, different answer.
            let first_byte = Arc::new(AtomicBool::new(false));
            let mark = Arc::clone(&first_byte);
            // One channel per attempt, dropped when the attempt ends, so the pump below finishes
            // on its own and a substitute provider never inherits the failed one's deltas.
            let (attempt_deltas, mut watched) = mpsc::channel::<ChatEvent>(STREAM_BUFFER);
            let relay = frames.clone();
            let pump_attempt = tokio::spawn(async move {
                while let Some(event) = watched.recv().await {
                    let frame = match event {
                        ChatEvent::Start {
                            provider,
                            model,
                            protocol,
                        } => Frame::Start {
                            provider,
                            model,
                            protocol,
                        },
                        ChatEvent::Delta(content) => {
                            // Marked here, at the one place a byte is handed to a subscriber —
                            // the client may already have received it.
                            mark.store(true, Ordering::SeqCst);
                            Frame::Delta(content)
                        }
                    };
                    if relay.send(frame).await.is_err() {
                        break;
                    }
                }
            });

            let result = stream_chat(&current, &request, &attempt_deltas).await;
            // The provider stopped writing: close the channel so the pump ends and the flag holds
            // whatever this attempt actually delivered.
            drop(attempt_deltas);
            let _ = pump_attempt.await;
            let streamed = first_byte.load(Ordering::SeqCst);

            match result {
                Ok(answer) => {
                    attempts.push(omnion_ai_hub::Attempt {
                        error: None,
                        ..attempt
                    });
                    outcome = Some(answer);
                    break;
                }
                Err(error) => {
                    attempts.push(omnion_ai_hub::Attempt {
                        error: Some(error.to_string()),
                        ..attempt
                    });

                    let progress = if streamed {
                        omnion_ai_hub::Progress::AfterFirstByte
                    } else {
                        omnion_ai_hub::Progress::BeforeFirstByte
                    };
                    let substitute = if omnion_ai_hub::is_retryable(&error) {
                        omnion_ai_hub::next(&providers, &attempts, progress)
                    } else {
                        None
                    };

                    let Some(substitute) = substitute else {
                        // The chain is spent, or this request was never allowed to move. The
                        // caller gets the first provider's complaint with the later ones
                        // appended, because the first is the provider they asked for.
                        let final_error = omnion_ai_hub::final_error(&attempts).unwrap_or(error);
                        record_usage(
                            &pool,
                            &attempts,
                            &served_by,
                            &model_key,
                            None,
                            "error",
                            started.elapsed(),
                        )
                        .await;
                        let _ = frames
                            .send(Frame::Failed {
                                code: final_error.code(),
                                message: final_error.to_string(),
                            })
                            .await;
                        let entry = NewAuditEntry::by_user(user_id, "ai.chat.failed")
                            .organization(organization_id)
                            .target("ai_model", model_id.clone())
                            .metadata(json!({
                                "provider": current.name,
                                "model": model_key,
                                "error": final_error.to_string(),
                                "attempts": attempts.len(),
                            }))
                            .ip_address(ip_address.clone());
                        if let Err(error) = omnion_audit::record(&pool, entry).await {
                            tracing::warn!(%error, "the AI chat audit row could not be written");
                        }
                        return;
                    };

                    // Record the attempt that failed *and* announce the substitution, both
                    // before any byte of the new provider's answer reaches the caller.
                    record_usage(
                        &pool,
                        &attempts,
                        &attempt.provider_id,
                        &model_key,
                        None,
                        "error",
                        started.elapsed(),
                    )
                    .await;
                    announce_failover(
                        &pool,
                        &attempts,
                        &substitute,
                        &model_key,
                        user_id,
                        organization_id,
                        ip_address.as_deref(),
                    )
                    .await;

                    let Some(provider) =
                        omnion_ai_hub::find_provider(&pool, substitute.provider_id)
                            .await
                            .ok()
                            .flatten()
                    else {
                        // The substitute was removed between planning and dialling: there is
                        // nothing left to try, and the error the caller already has is the truth.
                        break;
                    };
                    served_by = provider.id;
                    current = ProviderTarget::from_provider(&provider);
                }
            }
        }

        let answer = outcome.expect("an answer or an early return above");
        // The counts the provider reported ride on the row that served the call, and nowhere
        // else: a failed attempt produced no answer and therefore spent no tokens.
        record_usage(
            &pool,
            &attempts,
            &served_by,
            &model_key,
            answer.usage.as_ref(),
            "ok",
            started.elapsed(),
        )
        .await;

        // `current` is the provider that actually answered, which after a substitution is *not*
        // the one the request was routed to. The caller is told the final provider, because "the
        // standby served this" is the fact the operator needs to see next to the answer.
        let metadata = json!({
            "provider": current.name,
            "model": model_key,
            "chars": answer.content.chars().count(),
            "finish_reason": answer.finish_reason,
            "substitutions": attempts.len().saturating_sub(1),
            "usage": answer.usage.as_ref().map(|usage| json!({
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            })),
        });

        let frame = Frame::Done {
            finish_reason: answer.finish_reason,
            chars: answer.content.chars().count(),
            usage: answer.usage,
        };
        let _ = frames.send(frame).await;

        let entry = NewAuditEntry::by_user(user_id, "ai.chat.completed")
            .organization(organization_id)
            .target("ai_model", model_id)
            .metadata(metadata)
            .ip_address(ip_address);
        if let Err(error) = omnion_audit::record(&pool, entry).await {
            tracing::warn!(%error, "the AI chat audit row could not be written");
        }
    });

    let stream = ReceiverStream::new(receiver).map(Frame::event);

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------------------------
// Failover bookkeeping
// ---------------------------------------------------------------------------------------------

/// Record the usage row of every attempt a chat walk made.
///
/// One row per attempt, not one per request: the Usage tab shows requests **and** errors per
/// provider, and a substitution is two provider-level facts — the one that failed and the one
/// that answered. A single row would make the failed provider look like it never served anything
/// and the substitute look like it served a request nobody made.
///
/// `reported` is the usage the answering provider actually sent. It rides on the **successful**
/// row only: the tokens belong to the answer, and a failed attempt produced none. Passing it here
/// is the whole difference between a Usage tab with numbers on it and a tab where every call
/// reads "unknown" — the counts arrive on the `done` frame, so dropping them here threw away the
/// only place they existed.
async fn record_usage(
    pool: &sqlx::PgPool,
    attempts: &[omnion_ai_hub::Attempt],
    served_by: &uuid::Uuid,
    model_key: &str,
    reported: Option<&omnion_ai_hub::ChatUsage>,
    outcome: &str,
    elapsed: std::time::Duration,
) {
    let latency_ms = i32::try_from(elapsed.as_millis()).unwrap_or(i32::MAX);
    for (index, attempt) in attempts.iter().enumerate() {
        // The last attempt is the one whose row carries the substitution; the earlier ones are
        // the failures that led to it.
        let substituted_from = if index + 1 == attempts.len() && outcome == "ok" {
            attempts.first().map(|first| first.provider_id)
        } else {
            None
        };
        let served = outcome == "ok";
        let row = omnion_ai_hub::health_store::NewUsage {
            provider_id: if served {
                *served_by
            } else {
                attempt.provider_id
            },
            model_key: (!model_key.is_empty()).then(|| model_key.to_owned()),
            task: "chat".to_owned(),
            outcome: if attempt.error.is_some() && !served {
                "error".to_owned()
            } else {
                outcome.to_owned()
            },
            http_status: None,
            // Counts belong to the answer, so they land on the row that served it and nowhere
            // else. A stream that reported none stays `None`, and `missing_usage` counts it —
            // which is the difference between "unknown" and a real zero.
            prompt_tokens: if served {
                reported.and_then(|usage| token_count(usage.prompt_tokens))
            } else {
                None
            },
            completion_tokens: if served {
                reported.and_then(|usage| token_count(usage.completion_tokens))
            } else {
                None
            },
            latency_ms,
            substituted_from,
            first_byte_at: None,
        };
        if let Err(error) = omnion_ai_hub::health_store::record_usage(pool, row).await {
            tracing::warn!(%error, "a provider usage row could not be written");
        }
    }
}

/// A reported token count as the `int` the column stores.
///
/// A provider's count arrives as `u64` and a column that cannot hold it must not turn a
/// five-billion-token run into a negative number, so the value saturates: a count beyond what
/// the column can hold is stored as the largest count it can hold, which is visibly wrong and
/// far better than a wrapped one.
fn token_count(tokens: Option<u64>) -> Option<i32> {
    tokens.map(|count| i32::try_from(count).unwrap_or(i32::MAX))
}

/// Announce a substitution: the requested provider, the one that took over, and why.
///
/// This is the `ai.provider.failover_used` event the request names, and it is written **before**
/// the substitute's first byte reaches the caller. A substitution the operator can only discover
/// afterwards, in a bill, is not a failover they can trust.
#[allow(clippy::too_many_arguments)]
async fn announce_failover(
    pool: &sqlx::PgPool,
    attempts: &[omnion_ai_hub::Attempt],
    substitute: &omnion_ai_hub::Attempt,
    model_key: &str,
    user_id: uuid::Uuid,
    organization_id: Option<uuid::Uuid>,
    ip_address: Option<&str>,
) {
    let requested = attempts
        .first()
        .map(|attempt| attempt.provider_name.clone())
        .unwrap_or_default();
    let reason = attempts
        .last()
        .and_then(|attempt| attempt.error.clone())
        .unwrap_or_default();

    let mut entry = NewAuditEntry::by_user(user_id, "ai.provider.failover_used")
        .target("ai_provider", substitute.provider_id.to_string())
        .metadata(json!({
            "requested_provider": requested,
            "substitute_provider": substitute.provider_name,
            "model": model_key,
            "task": "chat",
            "reason": reason,
        }));
    if let Some(organization_id) = organization_id {
        entry = entry.organization(organization_id);
    }
    if let Some(ip_address) = ip_address {
        entry = entry.ip_address(Some(ip_address.to_owned()));
    }
    if let Err(error) = omnion_audit::record(pool, entry).await {
        tracing::warn!(%error, "the failover event could not be written");
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Describe models against the providers they belong to, in the providers' own order.
fn models_in_provider_order(providers: &[Provider], models: &[AiModel]) -> Vec<ModelBody> {
    let mut described = Vec::with_capacity(models.len());
    for provider in providers {
        for model in models.iter().filter(|m| m.provider_id == provider.id) {
            described.push(ModelBody::build(provider, model));
        }
    }

    described
}
