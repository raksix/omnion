//! Omnion AI Hub.
//!
//! The platform's single door to AI (docs/06-AI-HUB.md): the providers an installation
//! connected, the models they serve, the router that picks one, and the client that speaks to
//! it. Everything else in Omnion asks the AI Hub for a model — it never learns which vendor
//! answered, and it never talks to a provider itself.
//!
//! v0 is the foundation the agents build on (docs/06 §5–§17): connect a provider (the
//! OpenAI-compatible protocol covers OpenAI, OpenCode, CommandCode, Ollama and a local vLLM
//! server — §1), register the models it serves with their capability metadata (§3), let the
//! router resolve a request to one pair (§4), and run a chat — streamed — through the platform
//! API. The agent runtime, tools, approvals, RAG and the cost manager arrive in later phases;
//! they call into this crate instead of re-inventing it.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod model;
pub mod router;
pub mod store;

pub use client::{
    ChatEvent, ChatMessage, ChatOutcome, ChatRequest, ChatRole, ChatUsage, ProviderTarget, chat,
    list_remote_models, stream_chat, validate_request,
};
pub use error::{AiHubError, Result};
pub use model::{
    AiModel, ApiKeyChange, DEFAULT_PROTOCOL, MAX_MODEL_KEY_LEN, MAX_NAME_LEN, ModelChanges,
    NewAiModel, NewProvider, Provider, ProviderChanges, ProviderSummary, SUPPORTED_PROTOCOLS,
    normalize_base_url, validate_model_key, validate_name, validate_protocol,
};
pub use router::{ResolvedModel, model_id, resolve};
pub use store::{
    create_provider, delete_provider, find_default_model, find_model, find_model_by_key,
    find_provider, find_provider_by_name, list_models, list_providers, replace_models,
    update_model, update_provider,
};
