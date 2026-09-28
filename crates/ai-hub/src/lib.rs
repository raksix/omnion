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
pub mod connection_test;
pub mod error;
pub mod failover;
pub mod health;
pub mod health_store;
pub mod model;
pub mod protocol;
pub mod router;
pub mod store;

pub use client::{
    ChatEvent, ChatMessage, ChatOutcome, ChatRequest, ChatRole, ChatUsage, ProviderTarget, chat,
    list_remote_models, stream_chat, validate_request,
};
pub use connection_test::{StepStatus, TestReport, TestStep, run_test as test_provider};
pub use error::{AiHubError, Result};
pub use failover::{
    Attempt, Plan, Progress, Routing, chain_of, final_error, is_retryable, next, plan,
};
pub use model::{
    AiModel, ApiKeyChange, DEFAULT_PROTOCOL, DiscoveryAction, DiscoveryDiff, DiscoveryLine,
    HEALTH_STATUSES, MAX_MODEL_KEY_LEN, MAX_NAME_LEN, MAX_PRIORITY, MAX_RETRIES_CEILING,
    MAX_TIMEOUT_MS, MIN_PRIORITY, MIN_TIMEOUT_MS, ModelCapability, ModelChanges, NewAiModel,
    NewProvider, PROVIDER_KINDS, Provider, ProviderChanges, ProviderSummary, SUPPORTED_PROTOCOLS,
    diff_discovery, normalize_base_url, require_capability, validate_kind, validate_model_key,
    validate_name, validate_priority, validate_protocol, validate_retries, validate_timeout,
    validate_token_limits,
};
pub use protocol::{ProtocolAdapter, ProtocolInfo, StreamPiece, adapter_for, protocol_infos};
pub use router::{ResolvedModel, model_id, resolve, resolve_for};
pub use store::{
    apply_discovery, create_provider, delete_provider, discovery_diff, failover_chain,
    find_default_model, find_model, find_model_by_key, find_provider, find_provider_by_name,
    list_models, list_providers, record_health, replace_models, update_model, update_provider,
};
