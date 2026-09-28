//! The store for task routes and feature overrides (REQ-098, slice 2).
//!
//! Two things live here that [`crate::routing`] deliberately does not do: reading the rows out
//! of PostgreSQL, and writing them back. The resolver is pure, so everything about *what* a
//! decision is lives in one testable place; everything about *where the rows are* lives here.
//!
//! # Why the reads are batched
//!
//! A resolution inherits through up to three scopes, and each scope holds seven tasks and any
//! number of feature pins. Loading them per scope and per task would be up to twenty-one queries
//! on the hot path of every AI request. [`load_maps`] therefore reads **one** query's worth of
//! routes and **one** of overrides, resolves each row's model from a single `ai_models` read,
//! and hands the resolver a fully built [`RoutingMaps`].
//!
//! # Why a write validates before it writes
//!
//! [`replace_task_map`] refuses a candidate that is disabled, of the wrong kind or missing a
//! required capability, naming the task, the candidate and the requirement. A route that stored
//! an unusable candidate would not fail loudly at write time — it would sit in the map looking
//! configured and only be discovered when a request silently degraded to a fallback, or refused
//! entirely. The panel's "Needs attention" badge is for rows that go stale *later* (a model
//! removed), not for rows that were never valid.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::{self, ModelFeatures};
use crate::error::{AiHubError, Result};
use crate::model::AiModel;
use crate::routing::{Candidate, FeatureOverride, RoutingMaps, Scope};
use crate::store;

const ROUTE_COLUMNS: &str = "id, organization_id, site_id, task, position, model_id, \
     requirements, updated_by, created_at, updated_at";

const OVERRIDE_COLUMNS: &str = "id, organization_id, site_id, feature, model_id, updated_by, \
     created_at, updated_at";

/// A raw route row, before its model is resolved.
#[derive(Debug, sqlx::FromRow)]
struct RouteRow {
    id: Uuid,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    task: String,
    position: i32,
    model_id: Option<Uuid>,
    requirements: Vec<String>,
}

/// A raw override row, before its model is resolved.
#[derive(Debug, sqlx::FromRow)]
struct OverrideRow {
    id: Uuid,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    feature: String,
    model_id: Uuid,
    updated_at: OffsetDateTime,
}

/// The scope a row belongs to, read back from its two nullable columns.
///
/// This is the inverse of [`Scope::columns`], and it is the one place that decides what a pair
/// of ids means — a row naming neither is an installation row, a row naming only an organization
/// is an organization row, and a row naming both is a site row. Deciding it in one function is
/// what stops a read from disagreeing with a write.
fn scope_of(organization_id: Option<Uuid>, site_id: Option<Uuid>) -> Scope {
    match (site_id, organization_id) {
        (Some(site), _) => Scope::Site(site),
        (None, Some(organization)) => Scope::Organization(organization),
        (None, None) => Scope::Installation,
    }
}

/// Load every map a request at `chain` can consult, plus the installation default.
///
/// `chain` is the inheritance chain (most specific first) so a site request does not read the
/// maps of an unrelated organization, and so a caller cannot widen its own scope by naming a
/// different one.
pub async fn load_maps(pool: &PgPool, chain: &[Scope]) -> Result<RoutingMaps> {
    let mut maps = RoutingMaps::empty();

    // 1. The models every route and pin may reference, in one read. A route row with a null
    //    `model_id` (the model was removed) still appears in the walk with a reason, so the
    //    route rows themselves must be read independently of the models.
    let models: Vec<AiModel> = store::list_models(pool, None).await?;
    let by_id: BTreeMap<Uuid, AiModel> = models
        .into_iter()
        .map(|model| (model.id, model))
        .collect();

    // 2. Route rows, ordered so a task's candidates arrive in position order. The ordering is
    //    the resolver's contract, not a presentation detail: an unordered read would make the
    //    chosen fallback depend on the query planner.
    let route_sql = format!(
        "select {ROUTE_COLUMNS} from ai_task_routes \
         where scope_key = any($1) order by scope_key, task, position"
    );
    let keys: Vec<String> = chain.iter().map(|scope| scope.key()).collect();
    let rows: Vec<RouteRow> = sqlx::query_as(&route_sql)
        .bind(&keys)
        .fetch_all(pool)
        .await?;

    for row in rows {
        let scope = scope_of(row.organization_id, row.site_id);
        let candidate = Candidate {
            id: row.id,
            task: row.task.clone(),
            position: row.position,
            model: row.model_id.and_then(|id| by_id.get(&id).cloned()),
            requirements: row.requirements,
            scope,
        };
        maps
            .routes
            .entry(scope)
            .or_default()
            .entry(row.task)
            .or_default()
            .push(candidate);
    }

    // 3. Feature pins, same reasoning.
    let override_sql = format!(
        "select {OVERRIDE_COLUMNS} from ai_feature_overrides \
         where scope_key = any($1) order by scope_key, feature"
    );
    let rows: Vec<OverrideRow> = sqlx::query_as(&override_sql)
        .bind(&keys)
        .fetch_all(pool)
        .await?;

    for row in rows {
        // A pin whose model vanished cannot happen: the foreign key is `on delete restrict`, so
        // the row is removed with the model. Skipping the impossible case is cheaper than a
        // second error variant that the API would never map to a status.
        let Some(model) = by_id.get(&row.model_id).cloned() else {
            continue;
        };
        let scope = scope_of(row.organization_id, row.site_id);
        maps.overrides.entry(scope).or_default().insert(
            row.feature.clone(),
            FeatureOverride {
                id: row.id,
                feature: row.feature,
                model,
                scope,
                updated_at: row.updated_at,
            },
        );
    }

    // 4. The default last: a request that a map answers never needs it, and loading it last
    //    means the resolver can stop before this row is read on the common path.
    maps.default_model = store::find_default_model(pool).await?;

    Ok(maps)
}

/// One task's candidate list as it is read for the panel.
#[derive(Debug, Clone)]
pub struct TaskRouteView {
    /// The task key.
    pub task: String,
    /// The one-line description the routing screen shows under the name.
    pub description: &'static str,
    /// The candidates, in position order.
    pub candidates: Vec<Candidate>,
    /// Whether this row came from a scope other than the one asked for (an inherited row).
    pub inherited: bool,
}

/// The whole task map of one scope, plus the pins that live at it.
#[derive(Debug, Clone)]
pub struct ScopeRoutes {
    /// The scope these rows belong to.
    pub scope: Scope,
    /// One entry per task, including tasks with no candidates.
    pub tasks: Vec<TaskRouteView>,
    /// The feature pins at this scope.
    pub overrides: Vec<FeatureOverride>,
    /// The scopes these rows inherited from, most specific first.
    pub chain: Vec<Scope>,
}

/// Read the task map of one scope, with inherited rows marked.
///
/// Every task appears in the answer even when nothing is configured: the routing screen renders
/// a row per task with a primary select, and a screen that only listed configured tasks would
/// make an empty installation look like a filtered view rather than an empty one.
pub async fn read_scope(pool: &PgPool, scope: Scope, organization_id: Option<Uuid>) -> Result<ScopeRoutes> {
    let chain = scope.chain(organization_id);
    let maps = load_maps(pool, &chain).await?;

    let tasks = catalog::routing_tasks()
        .into_iter()
        .map(|entry: ModelFeatures| {
            // The row shown is the most specific one that has candidates; a task configured only
            // at the installation level shows that row, marked as inherited, so the operator can
            // see both what is in force and where it came from.
            let own = maps.candidates(&entry.key, scope);
            let (candidates, inherited) = if own.is_empty() {
                chain
                    .iter()
                    .find_map(|candidate_scope| {
                        let rows = maps.candidates(&entry.key, *candidate_scope);
                        if rows.is_empty() {
                            None
                        } else {
                            Some((rows.to_vec(), *candidate_scope != scope))
                        }
                    })
                    .unwrap_or_else(|| (Vec::new(), false))
            } else {
                (own.to_vec(), false)
            };

            TaskRouteView {
                description: catalog::task_description(&entry.key),
                task: entry.key,
                candidates,
                inherited,
            }
        })
        .collect();

    let overrides = chain
        .iter()
        .flat_map(|candidate_scope| {
            maps.overrides
                .get(candidate_scope)
                .map(|features| features.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();

    Ok(ScopeRoutes {
        scope,
        tasks,
        overrides,
        chain,
    })
}

/// One candidate as an operator writes it.
#[derive(Debug, Clone)]
pub struct CandidateInput {
    /// The model to put in this slot.
    pub model_id: Uuid,
    /// Capabilities the task's requests must have.
    pub requirements: Vec<String>,
}

/// Replace the candidate list of one task at one scope.
///
/// The whole list is replaced rather than merged: a PUT that only added a primary would leave
/// a fallback the operator can neither see nor remove from the panel, and "what is in this map"
/// has to be readable from the map itself.
///
/// The validation is the acceptance criterion by name — *"a routing PUT naming a disabled or
/// capability-incompatible model is refused with the task, candidate and requirement in the
/// message"* — and it runs **before** the write, so a rejected map leaves the previous one
/// intact rather than half-applied.
pub async fn replace_task_map(
    pool: &PgPool,
    scope: Scope,
    organization_id: Option<Uuid>,
    task: &str,
    candidates: &[CandidateInput],
    updated_by: Option<Uuid>,
) -> Result<()> {
    let task = catalog::validate_task(task)?;

    let Some((column_organization, column_site)) = scope.columns(organization_id) else {
        return Err(AiHubError::InvalidModel(
            "this site has no organization, so a site-scoped route cannot be written".to_owned(),
        ));
    };

    // Validate every candidate against the *stored* model, not against a payload, so a caller
    // cannot claim a capability the registry does not carry.
    let models: Vec<AiModel> = store::list_models(pool, None).await?;
    let mut resolved: Vec<Option<AiModel>> = Vec::with_capacity(candidates.len());

    for (index, candidate) in candidates.iter().enumerate() {
        let model = models
            .iter()
            .find(|model| model.id == candidate.model_id)
            .ok_or_else(|| {
                AiHubError::InvalidModel(format!(
                    "the {task} route names a model that is not in the registry (candidate {})",
                    index + 1
                ))
            })?
            .clone();

        if !model.enabled {
            return Err(AiHubError::InvalidModel(format!(
                "the {task} route cannot use \"{}\": it is switched off",
                model.model_key
            )));
        }

        if let Some(reason) = catalog::task_refusal_reason(task, &model) {
            return Err(AiHubError::InvalidModel(format!(
                "the {task} route cannot use \"{}\": {reason}",
                model.model_key
            )));
        }

        for requirement in &candidate.requirements {
            catalog::validate_requirement(requirement)?;
            if let Some(reason) = catalog::requirement_refusal_reason(requirement, &model) {
                return Err(AiHubError::InvalidModel(format!(
                    "the {task} route cannot use \"{}\": {reason}",
                    model.model_key
                )));
            }
        }

        resolved.push(Some(model));
    }

    // The delete and the insert share one transaction: a map that lost its old primary and did
    // not gain a new one is worse than a rejected write, and a half-applied map is the state
    // nobody can explain from the panel.
    let mut tx = pool.begin().await?;

    sqlx::query(
        "delete from ai_task_routes where scope_key = $1 and task = $2",
    )
    .bind(scope.key())
    .bind(task)
    .execute(&mut *tx)
    .await?;

    for (index, (candidate, model)) in candidates.iter().zip(resolved.iter()).enumerate() {
        sqlx::query(
            "insert into ai_task_routes \
             (scope_key, organization_id, site_id, task, position, model_id, requirements, updated_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(scope.key())
        .bind(column_organization)
        .bind(column_site)
        .bind(task)
        .bind(index as i32 + 1)
        .bind(model.as_ref().map(|model| model.id))
        .bind(&candidate.requirements)
        .bind(updated_by)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Delete the candidate list of one task at one scope.
pub async fn clear_task_map(
    pool: &PgPool,
    scope: Scope,
    task: &str,
) -> Result<()> {
    let task = catalog::validate_task(task)?;
    sqlx::query("delete from ai_task_routes where scope_key = $1 and task = $2")
        .bind(scope.key())
        .bind(task)
        .execute(pool)
        .await?;
    Ok(())
}

/// Pin or unpin one feature at one scope.
///
/// Returns the id of the pin when one is now in force, or `None` when the feature is left
/// unpinned at this scope. A caller that needs to distinguish "removed one that existed" from
/// "there was nothing to remove" reads the scope back — the endpoint re-reads it either way, so
/// inventing a second status here would be a fact nothing consumes.
pub async fn set_override(
    pool: &PgPool,
    scope: Scope,
    organization_id: Option<Uuid>,
    feature: &str,
    model_id: Option<Uuid>,
    updated_by: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let feature = catalog::validate_feature(feature)?;

    let Some((column_organization, column_site)) = scope.columns(organization_id) else {
        return Err(AiHubError::InvalidModel(
            "this site has no organization, so a site-scoped override cannot be written".to_owned(),
        ));
    };

    let Some(model_id) = model_id else {
        // Removing a pin is a delete, not an upsert with a null model: the column is
        // `not null` on purpose, because a pin that names no model is not a pin and would
        // render as an empty cell the operator has to guess about.
        sqlx::query("delete from ai_feature_overrides where scope_key = $1 and feature = $2")
            .bind(scope.key())
            .bind(feature)
            .execute(pool)
            .await?;
        return Ok(None);
    };

    let models: Vec<AiModel> = store::list_models(pool, None).await?;
    let model = models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| {
            AiHubError::InvalidModel(format!(
                "the {feature} override names a model that is not in the registry"
            ))
        })?
        .clone();

    if !model.enabled {
        return Err(AiHubError::InvalidModel(format!(
            "the {feature} override cannot use \"{}\": it is switched off",
            model.model_key
        )));
    }

    let row: (Uuid,) = sqlx::query_as(
        "insert into ai_feature_overrides \
         (scope_key, organization_id, site_id, feature, model_id, updated_by) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (scope_key, feature) do update \
         set model_id = excluded.model_id, \
             updated_by = excluded.updated_by, \
             updated_at = now() \
         returning id",
    )
    .bind(scope.key())
    .bind(column_organization)
    .bind(column_site)
    .bind(feature)
    .bind(model_id)
    .bind(updated_by)
    .fetch_one(pool)
    .await?;

    Ok(Some(row.0))
}

/// The organization a site belongs to.
///
/// The API never queries the database directly, so this lives beside the other reads: a handler
/// that ran its own SQL would need its own error mapping, and the tenancy answer it needs is the
/// same one the row's own foreign key asserts.
pub async fn organization_of_site(pool: &PgPool, site_id: Uuid) -> Result<Option<Uuid>> {
    // `sites.organization_id` is NOT NULL, so the column is read as a plain `Option<Uuid>`
    // (absent row → None) rather than a nested option. A site with a null organization cannot
    // exist, and flattening it here would turn that impossibility into a `None` the caller
    // would have to guess the meaning of.
    let row: Option<(Uuid,)> = sqlx::query_as("select organization_id from sites where id = $1")
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(organization,)| organization))
}

/// The known feature keys with their descriptions, for the override form.
#[must_use]
pub fn features() -> Vec<ModelFeatures> {
    catalog::model_features()
}

/// The known requirement keys, for the routing screen's chips.
#[must_use]
pub fn requirements() -> &'static [&'static str] {
    catalog::ROUTE_REQUIREMENTS
}
