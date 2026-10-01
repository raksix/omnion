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

pub mod agent;
pub mod agent_sdk;
pub mod approvals;
pub mod catalog;
pub mod catalogue;
pub mod change_sets;
pub mod ops_binding;
pub mod client;
pub mod connection_test;
pub mod cost;
pub mod decision_store;
pub mod error;
pub mod failover;
pub mod guard_checkpoint;
pub mod guard_data;
pub mod guard_remap;
pub mod guard_store;
pub mod guardrails;
pub mod health;
pub mod health_store;
pub mod identity;
pub mod loop_engine;
pub mod model;
pub mod proposal;
pub mod protocol;
pub mod provider_model;
pub mod registry;
pub mod resolve_path;
pub mod router;
pub mod routing;
pub mod routing_store;
pub mod run_store;
pub mod schema;
pub mod skills;
pub mod store;
pub mod telemetry;
pub mod tool_calls;
pub mod tool_exec;
pub mod tools;
pub mod workspace;

pub use catalog::{
    CapabilityFilter, CatalogEntry, CatalogQuery, CatalogSort, LONG_CONTEXT_TOKENS,
    MICROS_CEILING_PER_MTOK, MICROS_FLOOR_PER_MTOK, MODEL_FEATURES, ModelFeatures, ModelPrice,
    ModelUsage, PRICE_STALE_DAYS, PriceHalf, PriceSource, PriceView, ROUTE_REQUIREMENTS,
    ROUTING_TASKS, TOKENS_PER_MTOK, can_serve_task, catalog_entry, cmp_price,
    feature_description, long_context_minimum, model_features, price_age_days, price_is_stale,
    requirement_capability, requirement_refusal_reason, routing_tasks, task_description,
    task_refusal_reason, validate_feature, validate_price, validate_requirement, validate_task,
};
pub use client::{
    ChatEvent, ChatMessage, ChatOutcome, ChatRequest, ChatRole, ChatToolCall, ChatUsage,
    ProviderTarget, ToolSpec, chat, list_remote_models, stream_chat, validate_request,
};
pub use connection_test::{StepStatus, TestReport, TestStep, run_test as test_provider};
pub use error::{AiHubError, Result};
pub use loop_engine::{
    CancelHandle, Message as AgentMessage, Model, ModelAnswer, ModelError, Outcome as RunOutcome,
    Persist, RequestedCall, RunOptions, Runtime, SINK_CAPACITY, ScriptedModel, Sink,
    run as run_agent, run_with as run_agent_with,
};
pub use agent_sdk::{AgentSdk, AgentSdkBuilder};
pub use tools::{
    AllowList, DenyReason, Execution, FnTool, Tool, ToolOutcome, ToolRegistry, ToolSummary,
    decide as decide_tool_call, event_for as tool_event,
};
pub use failover::{
    Attempt, Plan, Progress, chain_of, final_error, is_retryable, next, pinned_provider, plan,
};
pub use identity::{
    AiIdentity, GrantEffect, IdentityChanges, MAX_DESCRIPTION_CHARS as IDENTITY_DESCRIPTION_CHARS,
    MAX_KEY_CHARS as IDENTITY_KEY_CHARS, MAX_NAME_CHARS as IDENTITY_NAME_CHARS, NewIdentity,
    Resolution, ResolutionReason, resolve as resolve_tool_grant,
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
pub use provider_model::{ProviderModel, SharedModel};
pub use resolve_path::{Resolved, resolve_and_record};
pub use router::{ResolvedModel, model_id, resolve, resolve_for};
pub use routing::{
    Candidate, Decision, DecisionSource, FeatureOverride, RULES, ResolveRequest, ResolvedCandidate,
    RoutingMaps, Scope, WalkEntry, WalkStep, check_feature, check_requirement, check_task, decide,
    requirement_rows, scope_label, task_rows, unknown_feature, unknown_requirement, unknown_task,
};
pub use store::{
    apply_discovery, create_provider, delete_provider, discovery_diff, failover_chain,
    find_default_model, find_model, find_model_by_key, find_provider, find_provider_by_name,
    list_models, list_providers, record_health, replace_models, update_model, update_provider,
};
pub use routing_store::{
    CandidateInput, ScopeRoutes, TaskRouteView, clear_task_map, features, load_maps,
    organization_of_site, read_scope, replace_task_map, requirements, set_override,
};
pub use decision_store::{
    Answer, DecisionContext, DecisionFilter, DecisionPage, DecisionRow, NewDecision,
    RouteDecision, answer_position, decision_id_of, export_rows, last_per_task, list, prune,
    read_one, record,
};
