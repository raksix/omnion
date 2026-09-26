//! The model router (docs/06-AI-HUB.md §4).
//!
//! v0 answers one question: **which provider and which model serve this request?** The rules are
//! small on purpose and entirely deterministic:
//!
//! 1. A request that names a model as `provider/model` — where `provider` is a connected
//!    provider's name — goes to that provider.
//! 2. A request that names a bare model key is looked up across the enabled providers, the
//!    installation's default provider first, then the others in name order. A model key may
//!    itself contain a slash (`meta-llama/Llama-3.1-8B-Instruct`), so a prefix only counts as a
//!    provider when a provider of that name is really connected.
//! 3. A request that names no model at all goes to the installation's default model.
//!
//! The task-aware routing of §4 (a cheap model for simple text, a long-context model for huge
//! documents) builds on this resolver; v0 keeps the shape it needs — one resolved
//! `(provider, model)` pair with every fact the caller needs to call it.

use sqlx::PgPool;

use crate::error::{AiHubError, Result};
use crate::model::{AiModel, Provider};
use crate::store;

/// The provider and the model one request resolves to.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Provider that serves the model.
    pub provider: Provider,
    /// Model to call.
    pub model: AiModel,
}

impl ResolvedModel {
    /// The `provider/model` identifier this pair answers to.
    #[must_use]
    pub fn id(&self) -> String {
        model_id(&self.provider, &self.model)
    }
}

/// The `provider/model` identifier of one pair.
#[must_use]
pub fn model_id(provider: &Provider, model: &AiModel) -> String {
    format!("{}/{}", provider.name, model.model_key)
}

/// Resolve the model one request addresses.
pub async fn resolve(pool: &PgPool, requested: Option<&str>) -> Result<ResolvedModel> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());

    match requested {
        None => default_model(pool).await,
        Some(value) => {
            if let Some((prefix, rest)) = value.split_once('/')
                && let Some(provider) = store::find_provider_by_name(pool, prefix).await?
            {
                let model = store::find_model_by_key(pool, provider.id, rest)
                    .await?
                    .ok_or(AiHubError::ModelNotFound)?;

                return ready(provider, model);
            }

            by_key(pool, value).await
        }
    }
}

/// The installation's default model, as the router sees it.
pub async fn default_model(pool: &PgPool) -> Result<ResolvedModel> {
    let model = store::find_default_model(pool)
        .await?
        .ok_or(AiHubError::NoDefaultModel)?;
    let provider = store::find_provider(pool, model.provider_id)
        .await?
        .ok_or(AiHubError::ProviderNotFound)?;

    ready(provider, model)
}

/// A bare model key, searched across the enabled providers.
async fn by_key(pool: &PgPool, model_key: &str) -> Result<ResolvedModel> {
    let providers = store::list_providers(pool).await?;
    let mut candidates: Vec<Provider> = providers.into_iter().filter(|p| p.enabled).collect();
    // The default provider is asked first, then the rest in name order.
    candidates.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    for provider in candidates {
        if let Some(model) = store::find_model_by_key(pool, provider.id, model_key).await?
            && model.enabled
        {
            return ready(provider, model);
        }
    }

    // Distinguish "the provider is switched off" from "no such model": both are real, and the
    // operator fixes them differently.
    for provider in store::list_providers(pool).await? {
        if provider.enabled {
            continue;
        }
        if let Some(model) = store::find_model_by_key(pool, provider.id, model_key).await?
            && model.enabled
        {
            return Err(AiHubError::ProviderDisabled(provider.name));
        }
    }

    Err(AiHubError::ModelNotFound)
}

/// Check that a resolved pair is usable, then hand it back.
fn ready(provider: Provider, model: AiModel) -> Result<ResolvedModel> {
    if !provider.enabled {
        return Err(AiHubError::ProviderDisabled(provider.name));
    }
    if !model.enabled {
        return Err(AiHubError::InvalidModel(format!(
            "\"{}\" is switched off",
            model.model_key
        )));
    }

    Ok(ResolvedModel { provider, model })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn provider(name: &str, enabled: bool, is_default: bool) -> Provider {
        Provider {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            protocol: "openai_compatible".to_owned(),
            base_url: "https://api.example.com/v1".to_owned(),
            api_key: None,
            enabled,
            is_default,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn model(key: &str, enabled: bool) -> AiModel {
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
            enabled,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_pair_names_itself() {
        let pair = ResolvedModel {
            provider: provider("Office AI", true, false),
            model: model("gpt-4o-mini", true),
        };
        assert_eq!(pair.id(), "Office AI/gpt-4o-mini");
    }

    #[test]
    fn a_disabled_provider_never_resolves() {
        let error = ready(provider("Off", false, false), model("m", true)).expect_err("refused");
        assert!(matches!(error, AiHubError::ProviderDisabled(_)));
        assert!(error.to_string().contains("Off"));
    }

    #[test]
    fn a_disabled_model_never_resolves() {
        let error = ready(provider("On", true, false), model("m", false)).expect_err("refused");
        assert!(matches!(error, AiHubError::InvalidModel(_)));
    }

    #[test]
    fn a_usable_pair_resolves() {
        let pair = ready(provider("On", true, true), model("m", true)).expect("usable");
        assert_eq!(pair.id(), "On/m");
    }
}
