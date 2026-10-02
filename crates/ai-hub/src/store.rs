//! The store: providers and models as rows.
//!
//! Two invariants are kept here rather than by the callers, because every path that writes
//! models has to honour them:
//!
//! * **At most one default provider** — the database enforces it with a partial unique index,
//!   and the store clears the previous default inside the same transaction.
//! * **A default model is always an enabled one** — whenever a model is switched off, removed,
//!   or its provider is deleted, the store repairs the default (promoting the oldest enabled
//!   model) or leaves the installation without one, never with a default that cannot answer.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::model::{
    AiModel, ApiKeyChange, ModelChanges, NewAiModel, NewProvider, Provider, ProviderChanges,
    normalize_base_url, validate_model_key, validate_name, validate_protocol,
};

/// Columns read back from `ai_providers`.
const PROVIDER_COLUMNS: &str = "id, name, protocol, base_url, api_key, enabled, is_default, \
     created_at, updated_at";

/// Columns read back from `ai_models`.
const MODEL_COLUMNS: &str = "id, provider_id, model_key, display_name, context_window, \
     supports_tools, supports_vision, supports_streaming, supports_embeddings, enabled, \
     is_default, created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------------------------

/// Every provider, in name order.
pub async fn list_providers(pool: &PgPool) -> Result<Vec<Provider>> {
    let sql = format!("select {PROVIDER_COLUMNS} from ai_providers order by lower(name), id");
    let providers: Vec<Provider> = sqlx::query_as(&sql).fetch_all(pool).await?;
    Ok(providers)
}

/// One provider by id.
pub async fn find_provider(pool: &PgPool, id: Uuid) -> Result<Option<Provider>> {
    let sql = format!("select {PROVIDER_COLUMNS} from ai_providers where id = $1");
    let provider: Option<Provider> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(provider)
}

/// One provider by name, case-insensitive.
pub async fn find_provider_by_name(pool: &PgPool, name: &str) -> Result<Option<Provider>> {
    let sql = format!("select {PROVIDER_COLUMNS} from ai_providers where lower(name) = lower($1)");
    let provider: Option<Provider> = sqlx::query_as(&sql)
        .bind(name.trim())
        .fetch_optional(pool)
        .await?;
    Ok(provider)
}

/// Connect a provider.
pub async fn create_provider(pool: &PgPool, new: NewProvider) -> Result<Provider> {
    validate_name(&new.name)?;
    validate_protocol(&new.protocol)?;
    let base_url = normalize_base_url(&new.base_url)?;
    let name = new.name.trim().to_owned();
    let api_key = new.api_key.filter(|key| !key.trim().is_empty());

    let mut tx = pool.begin().await?;
    if new.is_default {
        clear_default_provider(&mut tx).await?;
    }

    let sql = format!(
        "insert into ai_providers (name, protocol, base_url, api_key, enabled, is_default) \
         values ($1, $2, $3, $4, $5, $6) returning {PROVIDER_COLUMNS}"
    );
    let stored: Result<Provider> = sqlx::query_as(&sql)
        .bind(&name)
        .bind(&new.protocol)
        .bind(&base_url)
        .bind(api_key.as_deref())
        .bind(new.enabled)
        .bind(new.is_default)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| name_conflict(error, &name));

    let provider = stored?;
    tx.commit().await?;

    Ok(provider)
}

/// Change a provider.
pub async fn update_provider(
    pool: &PgPool,
    id: Uuid,
    changes: ProviderChanges,
) -> Result<Provider> {
    let name = match changes.name {
        Some(name) => {
            validate_name(&name)?;
            Some(name.trim().to_owned())
        }
        None => None,
    };
    let base_url = match changes.base_url {
        Some(base_url) => Some(normalize_base_url(&base_url)?),
        None => None,
    };

    let mut tx = pool.begin().await?;
    if changes.is_default == Some(true) {
        clear_default_provider(&mut tx).await?;
    }

    let sql = format!(
        "update ai_providers set name = coalesce($2, name), base_url = coalesce($3, base_url), \
         api_key = case when $4 then $5 else api_key end, enabled = coalesce($6, enabled), \
         is_default = coalesce($7, is_default), updated_at = now() \
         where id = $1 returning {PROVIDER_COLUMNS}"
    );
    let (replace_key, api_key) = match changes.api_key {
        ApiKeyChange::Keep => (false, None),
        ApiKeyChange::Set(key) => (true, Some(key)),
        ApiKeyChange::Clear => (true, None),
    };

    let stored: Result<Option<Provider>> = sqlx::query_as(&sql)
        .bind(id)
        .bind(name.as_deref())
        .bind(base_url.as_deref())
        .bind(replace_key)
        .bind(api_key.as_deref())
        .bind(changes.enabled)
        .bind(changes.is_default)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| name_conflict(error, name.as_deref().unwrap_or("")));

    let Some(provider) = stored? else {
        return Err(AiHubError::ProviderNotFound);
    };

    // Switching a provider off must not leave the installation pointing at it.
    if !provider.enabled || provider.is_default {
        repair_default_model(&mut tx).await?;
    }

    tx.commit().await?;
    Ok(provider)
}

/// Remove a provider and every model it serves.
pub async fn delete_provider(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;

    let deleted = sqlx::query("delete from ai_providers where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(AiHubError::ProviderNotFound);
    }

    repair_default_model(&mut tx).await?;
    tx.commit().await?;

    Ok(())
}

/// Clear the installation's default provider.
async fn clear_default_provider(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("update ai_providers set is_default = false, updated_at = now() where is_default")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

/// Every model, of one provider when `provider_id` is given, in provider and key order.
pub async fn list_models(pool: &PgPool, provider_id: Option<Uuid>) -> Result<Vec<AiModel>> {
    let sql = format!(
        "select {MODEL_COLUMNS} from ai_models \
         where ($1::uuid is null or provider_id = $1) order by model_key, id"
    );
    let models: Vec<AiModel> = sqlx::query_as(&sql)
        .bind(provider_id)
        .fetch_all(pool)
        .await?;
    Ok(models)
}

/// One model by id.
pub async fn find_model(pool: &PgPool, id: Uuid) -> Result<Option<AiModel>> {
    let sql = format!("select {MODEL_COLUMNS} from ai_models where id = $1");
    let model: Option<AiModel> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(model)
}

/// One model by its wire key on one provider.
pub async fn find_model_by_key(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
) -> Result<Option<AiModel>> {
    let sql =
        format!("select {MODEL_COLUMNS} from ai_models where provider_id = $1 and model_key = $2");
    let model: Option<AiModel> = sqlx::query_as(&sql)
        .bind(provider_id)
        .bind(model_key.trim())
        .fetch_optional(pool)
        .await?;
    Ok(model)
}

/// The installation's default model, if it has an enabled one.
pub async fn find_default_model(pool: &PgPool) -> Result<Option<AiModel>> {
    let sql = format!(
        "select {MODEL_COLUMNS} from ai_models where is_default and enabled \
         order by created_at, model_key limit 1"
    );
    let model: Option<AiModel> = sqlx::query_as(&sql).fetch_optional(pool).await?;
    Ok(model)
}

/// Make a provider's model set agree with a list.
///
/// Keys that are not in the list are removed, keys that are keep their metadata unless the list
/// carries a new value, and the default model is repaired afterwards. Returns the provider's
/// models as they are stored now.
pub async fn replace_models(
    pool: &PgPool,
    provider_id: Uuid,
    models: Vec<NewAiModel>,
) -> Result<Vec<AiModel>> {
    let mut keys: Vec<String> = Vec::with_capacity(models.len());
    for model in &models {
        validate_model_key(&model.model_key)?;
        let key = model.model_key.trim().to_owned();
        if keys.contains(&key) {
            return Err(AiHubError::InvalidModel(format!(
                "\"{key}\" is listed twice"
            )));
        }
        keys.push(key);
    }

    let mut tx = pool.begin().await?;
    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from ai_providers where id = $1)")
            .bind(provider_id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(AiHubError::ProviderNotFound);
    }

    sqlx::query("delete from ai_models where provider_id = $1 and not (model_key = any($2))")
        .bind(provider_id)
        .bind(&keys)
        .execute(&mut *tx)
        .await?;

    let insert = format!(
        "insert into ai_models (provider_id, model_key, display_name, context_window, \
         supports_tools, supports_vision, supports_streaming, supports_embeddings) \
         values ($1, $2, $3, $4, coalesce($5, false), coalesce($6, false), \
         coalesce($7, true), coalesce($8, false)) \
         on conflict (provider_id, model_key) do update set \
         display_name = coalesce(excluded.display_name, ai_models.display_name), \
         context_window = coalesce(excluded.context_window, ai_models.context_window), \
         supports_tools = coalesce($5, ai_models.supports_tools), \
         supports_vision = coalesce($6, ai_models.supports_vision), \
         supports_streaming = coalesce($7, ai_models.supports_streaming), \
         supports_embeddings = coalesce($8, ai_models.supports_embeddings), \
         updated_at = now() \
         returning {MODEL_COLUMNS}"
    );

    let mut stored: Vec<AiModel> = Vec::with_capacity(models.len());
    for model in models {
        let row: AiModel = sqlx::query_as(&insert)
            .bind(provider_id)
            .bind(model.model_key.trim())
            .bind(model.display_name.as_deref())
            .bind(model.context_window)
            .bind(model.supports_tools)
            .bind(model.supports_vision)
            .bind(model.supports_streaming)
            .bind(model.supports_embeddings)
            .fetch_one(&mut *tx)
            .await?;
        stored.push(row);
    }

    repair_default_model(&mut tx).await?;
    tx.commit().await?;

    stored.sort_by(|left, right| left.model_key.cmp(&right.model_key));
    Ok(stored)
}

/// Change one model (switch it on or off, make it the default).
pub async fn update_model(pool: &PgPool, id: Uuid, changes: ModelChanges) -> Result<AiModel> {
    let mut tx = pool.begin().await?;

    let current: Option<AiModel> = sqlx::query_as(&format!(
        "select {MODEL_COLUMNS} from ai_models where id = $1 for update"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Err(AiHubError::ModelNotFound);
    };

    let enabled = changes.enabled.unwrap_or(current.enabled);
    let mut is_default = changes.is_default.unwrap_or(current.is_default);
    if !enabled {
        is_default = false;
    }

    if changes.is_default == Some(true) {
        if !enabled {
            return Err(AiHubError::InvalidModel(
                "a switched-off model cannot be the default".to_owned(),
            ));
        }
        sqlx::query("update ai_models set is_default = false, updated_at = now() where is_default")
            .execute(&mut *tx)
            .await?;
    }

    let sql = format!(
        "update ai_models set enabled = $2, is_default = $3, updated_at = now() \
         where id = $1 returning {MODEL_COLUMNS}"
    );
    let _stored: AiModel = sqlx::query_as(&sql)
        .bind(id)
        .bind(enabled)
        .bind(is_default)
        .fetch_one(&mut *tx)
        .await?;

    repair_default_model(&mut tx).await?;
    tx.commit().await?;

    // The repair may have promoted another model; read this one back so the caller sees the
    // stored truth.
    let sql = format!("select {MODEL_COLUMNS} from ai_models where id = $1");
    let fresh: AiModel = sqlx::query_as(&sql).bind(id).fetch_one(pool).await?;

    Ok(fresh)
}

/// Keep the default model honest: clear defaults that are switched off, promote the oldest
/// enabled model when the installation has none, and leave it without a default when it has no
/// enabled model at all.
async fn repair_default_model(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query(
        "update ai_models set is_default = false, updated_at = now() \
         where is_default and not enabled",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "update ai_models set is_default = true, updated_at = now() where id = ( \
           select id from ai_models where enabled \
           and not exists (select 1 from ai_models where is_default) \
           order by created_at, model_key limit 1)",
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Turn a unique-violation on the provider name into the API's own error.
fn name_conflict(error: sqlx::Error, name: &str) -> AiHubError {
    if let sqlx::Error::Database(ref database) = error
        && database.constraint() == Some("ai_providers_name_key")
    {
        return AiHubError::ProviderNameTaken(name.to_owned());
    }

    AiHubError::Database(error)
}
