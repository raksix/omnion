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

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_ai_hub::{
    AiHubError, AiModel, ApiKeyChange, ChatEvent, ChatMessage, ChatRequest, ChatRole, ModelChanges,
    NewAiModel, NewProvider, Provider, ProviderChanges, ProviderTarget, resolve, stream_chat,
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
    /// Base URL.
    pub base_url: String,
    /// Whether a key is stored.
    pub has_api_key: bool,
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
            base_url: provider.base_url.clone(),
            has_api_key: provider.has_api_key(),
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
    /// Whether the model is enabled.
    pub enabled: bool,
    /// Whether it is the installation's default model.
    pub is_default: bool,
    /// `provider/model` — how the router addresses this pair.
    pub model_id: String,
    /// When it was registered.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
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
            enabled: model.enabled,
            is_default: model.is_default,
            model_id: omnion_ai_hub::model_id(provider, model),
            created_at: model.created_at,
            updated_at: model.updated_at,
        }
    }
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

/// Response of a provider model sync.
#[derive(Debug, Serialize)]
pub struct DiscoverResponse {
    /// Provider that was asked.
    pub provider_id: Uuid,
    /// Its name.
    pub provider_name: String,
    /// Model keys it reported.
    pub models: Vec<String>,
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
            } => NewAiModel {
                model_key: key,
                display_name,
                context_window,
                supports_tools,
                supports_vision,
                supports_streaming,
                supports_embeddings,
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
    /// Base URL, version segment included.
    pub base_url: String,
    /// Key to authenticate with; absent for a local endpoint that wants none.
    pub api_key: Option<String>,
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
#[derive(Debug, Deserialize)]
pub struct UpdateModelBody {
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// `true` makes it the installation's default model.
    pub is_default: Option<bool>,
}

/// Query of `GET /ai/models`.
#[derive(Debug, Deserialize)]
pub struct ModelQuery {
    /// Narrow the list to one provider.
    pub provider_id: Option<Uuid>,
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
            base_url: body.base_url,
            api_key: body.api_key,
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

/// `GET /api/v1/ai/models` — the model registry.
pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ModelQuery>,
) -> Result<Json<ModelListResponse>, ApiError> {
    let providers = omnion_ai_hub::list_providers(state.db().pool()).await?;
    let models = omnion_ai_hub::list_models(state.db().pool(), query.provider_id).await?;

    Ok(Json(ModelListResponse {
        models: models_in_provider_order(&providers, &models),
    }))
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
pub async fn discover_provider_models(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<DiscoverResponse>, ApiError> {
    let provider = omnion_ai_hub::find_provider(state.db().pool(), id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    let target = ProviderTarget::from_provider(&provider);
    let models = omnion_ai_hub::list_remote_models(&target).await?;

    Ok(Json(DiscoverResponse {
        provider_id: provider.id,
        provider_name: provider.name,
        models,
    }))
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
        }))
        .ip_address(address.as_text());
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(ModelBody::build(&provider, &model)))
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

    let resolved = resolve(state.db().pool(), body.model.as_deref()).await?;
    let request = ChatRequest {
        model: resolved.model.model_key.clone(),
        messages,
        temperature: body.temperature,
        max_tokens: body.max_tokens,
    };
    // The request shape is checked before the stream opens, so a bad one answers 400.
    omnion_ai_hub::validate_request(&request)?;

    let target = ProviderTarget::from_provider(&resolved.provider);
    let provider_name = resolved.provider.name.clone();
    let model_key = resolved.model.model_key.clone();
    let model_id = resolved.id();
    let pool = state.db().pool().clone();
    let user_id = current.user.id;
    let organization_id = current.user.organization_id;
    let ip_address = address.as_text();

    let (frames, receiver) = mpsc::channel::<Frame>(STREAM_BUFFER);

    tokio::spawn(async move {
        let (deltas, mut delta_receiver) = mpsc::channel::<ChatEvent>(STREAM_BUFFER);
        let relay = frames.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = delta_receiver.recv().await {
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
                    ChatEvent::Delta(content) => Frame::Delta(content),
                };
                if relay.send(frame).await.is_err() {
                    break;
                }
            }
        });

        let outcome = stream_chat(&target, &request, &deltas).await;
        drop(deltas);
        let _ = pump.await;

        let (action, code, metadata) = match &outcome {
            Ok(outcome) => (
                "ai.chat.completed",
                None,
                json!({
                    "provider": provider_name,
                    "model": model_key,
                    "chars": outcome.content.chars().count(),
                    "finish_reason": outcome.finish_reason,
                    "usage": outcome.usage.as_ref().map(|usage| json!({
                        "prompt_tokens": usage.prompt_tokens,
                        "completion_tokens": usage.completion_tokens,
                        "total_tokens": usage.total_tokens,
                    })),
                }),
            ),
            Err(error) => (
                "ai.chat.failed",
                Some(error.code()),
                json!({
                    "provider": provider_name,
                    "model": model_key,
                    "error": error.to_string(),
                }),
            ),
        };

        let frame = match outcome {
            Ok(outcome) => Frame::Done {
                finish_reason: outcome.finish_reason,
                chars: outcome.content.chars().count(),
                usage: outcome.usage,
            },
            Err(error) => Frame::Failed {
                code: code.unwrap_or("internal_error"),
                message: error.to_string(),
            },
        };
        let _ = frames.send(frame).await;

        let entry = NewAuditEntry::by_user(user_id, action)
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
