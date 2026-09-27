//! `/api/v1/iam/policies` — the ABAC policy surface: the list, the builder's save and the dry run
//! (docs/requests/REQ-006, slice 4a; docs/07-IAM.md §11).
//!
//! A policy is evaluated after the roles have had their say: it can grant what RBAC did not (an
//! `allow` policy) and take away what RBAC granted (a `deny` policy), with the highest priority
//! deciding first and equal priorities resolving to deny. That reading lives in
//! `omnion-policy-engine`; this module is the API around it:
//!
//! * reading needs `iam.policies.read`, saving and removing need `iam.policies.manage`;
//! * **every save stores a version** (`policy_versions`), so the History tab shows what changed
//!   and when;
//! * `POST /iam/policies/{id}/test` is a dry run over sample attributes — it touches nothing and
//!   therefore needs only the read key. It accepts an unsaved draft (`policy` in the body) so the
//!   builder can try a policy before it exists, and answers with the leaf-by-leaf evaluation and
//!   the verdict the organization's enabled policies would reach with this policy in place.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_permissions::catalogue;
use omnion_permissions::policies::{self, PolicyDraft, PolicyRecord};
use omnion_policy_engine::{Attributes, Condition, PolicyEffect, PolicySet, targets};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Default priority: the middle of the documented 0–1000 range.
fn default_priority() -> i32 {
    500
}

/// A policy without conditions; the shape the table itself defaults to.
fn default_conditions() -> Value {
    json!({ "all": [] })
}

fn default_true() -> bool {
    true
}

/// Query of the list route: platform accounts name the organization, tenant accounts are theirs.
#[derive(Debug, Deserialize)]
pub struct PoliciesQuery {
    /// Organization to read; required for an account without a primary one.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a create or update.
#[derive(Debug, Deserialize)]
pub struct PolicyRequest {
    /// Name the reader sees.
    pub name: String,
    /// What the policy is for.
    #[serde(default)]
    pub description: String,
    /// `allow` or `deny`.
    pub effect: String,
    /// Higher wins; equal priority resolves to deny.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// The condition tree.
    #[serde(default = "default_conditions")]
    pub conditions: Value,
    /// Permission keys, exact or with `*`.
    #[serde(default)]
    pub target_permissions: Vec<String>,
    /// Whether the policy is active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Organization to create in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a dry run.
#[derive(Debug, Deserialize)]
pub struct PolicyTestRequest {
    /// The permission key to test against.
    pub permission: String,
    /// Sample attributes the conditions read (defaults to `{}`).
    #[serde(default)]
    pub attributes: Value,
    /// An unsaved draft to try instead of the stored policy.
    #[serde(default)]
    pub policy: Option<PolicyRequest>,
}

/// Turn a request into a draft, refusing a shape the engine cannot read.
fn draft_of(body: &PolicyRequest) -> Result<PolicyDraft, ApiError> {
    let effect = PolicyEffect::parse(body.effect.trim()).ok_or_else(|| {
        ApiError::bad_request("invalid_policy", "effect must be \"allow\" or \"deny\"")
    })?;
    let conditions = Condition::parse(&body.conditions)
        .map_err(|message| ApiError::bad_request("invalid_policy", message))?;

    let mut target_permissions: Vec<String> = Vec::new();
    for pattern in &body.target_permissions {
        let pattern = pattern.trim().to_owned();
        if !target_permissions.contains(&pattern) {
            target_permissions.push(pattern);
        }
    }

    Ok(PolicyDraft {
        name: body.name.trim().to_owned(),
        description: body.description.trim().to_owned(),
        effect,
        priority: body.priority,
        conditions,
        target_permissions,
        enabled: body.enabled,
    })
}

/// The stored shape the panel reads.
fn policy_json(record: &PolicyRecord) -> Value {
    json!({
        "id": record.id,
        "organization_id": record.organization_id,
        "name": record.name,
        "description": record.description,
        "effect": record.effect.as_str(),
        "priority": record.priority,
        "conditions": record.conditions.to_json(),
        "target_permissions": record.target_permissions,
        "enabled": record.enabled,
        "version": record.version,
        "created_at": record.created_at.format(&Rfc3339).unwrap_or_default(),
        "updated_at": record.updated_at.format(&Rfc3339).unwrap_or_default(),
    })
}

/// Load a policy and refuse it to an account from another organization.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    policy_id: Uuid,
) -> Result<PolicyRecord, ApiError> {
    let record = policies::find(state.db().pool(), policy_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "policy_not_found",
                "no such policy",
            )
        })?;

    if current.user.organization_id.is_some() {
        ensure_same_organization(current, Some(record.organization_id))?;
    }

    Ok(record)
}

/// List the organization's policies.
pub async fn list_policies(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<PoliciesQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let records = policies::list(state.db().pool(), organization_id).await?;

    Ok(Json(json!({
        "organization_id": organization_id,
        "policies": records.iter().map(policy_json).collect::<Vec<_>>(),
    })))
}

/// Create a policy.
pub async fn create_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<PolicyRequest>,
) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let draft = draft_of(&body)?;
    let saved =
        policies::create(state.db().pool(), organization_id, current.user.id, &draft).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.policy.created")
            .target("policy", saved.id.to_string())
            .metadata(json!({
                "name": saved.name,
                "effect": saved.effect.as_str(),
                "priority": saved.priority,
                "target_permissions": saved.target_permissions,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.policy_changed")
            .organization(Some(organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "policy_id": saved.id,
                "action": "created",
                "name": saved.name,
                "effect": saved.effect.as_str(),
                "priority": saved.priority,
                "version": saved.version,
            })),
    )
    .await;

    Ok((axum::http::StatusCode::CREATED, Json(policy_json(&saved))))
}

/// One policy, with its version.
pub async fn get_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(policy_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let record = load(&state, &current, policy_id).await?;
    Ok(Json(policy_json(&record)))
}

/// Save a policy: the version moves forward and the old state stays in history.
pub async fn update_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(policy_id): Path<Uuid>,
    Json(body): Json<PolicyRequest>,
) -> Result<Json<Value>, ApiError> {
    let before = load(&state, &current, policy_id).await?;
    let draft = draft_of(&body)?;
    let after = policies::update(state.db().pool(), policy_id, current.user.id, &draft).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.policy.updated")
            .target("policy", policy_id.to_string())
            .metadata(json!({
                "name": after.name,
                "version": after.version,
                "before": {
                    "effect": before.effect.as_str(),
                    "priority": before.priority,
                    "enabled": before.enabled,
                    "target_permissions": before.target_permissions,
                },
                "after": {
                    "effect": after.effect.as_str(),
                    "priority": after.priority,
                    "enabled": after.enabled,
                    "target_permissions": after.target_permissions,
                },
            }))
            .ip_address(address.as_text())
            .organization(Some(after.organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.policy_changed")
            .organization(Some(after.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "policy_id": after.id,
                "action": "updated",
                "name": after.name,
                "effect": after.effect.as_str(),
                "priority": after.priority,
                "version": after.version,
            })),
    )
    .await;

    Ok(Json(policy_json(&after)))
}

/// Remove a policy (its versions follow it).
pub async fn delete_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(policy_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let stored = load(&state, &current, policy_id).await?;
    let removed = policies::delete(state.db().pool(), policy_id).await?;
    if !removed {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "policy_not_found",
            "no such policy",
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.policy.deleted")
            .target("policy", policy_id.to_string())
            .metadata(json!({ "name": stored.name }))
            .ip_address(address.as_text())
            .organization(Some(stored.organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.policy_changed")
            .organization(Some(stored.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "policy_id": policy_id,
                "action": "deleted",
                "name": stored.name,
            })),
    )
    .await;

    Ok(Json(json!({ "deleted": true })))
}

/// The recorded versions of a policy, newest first.
pub async fn list_policy_versions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(policy_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let stored = load(&state, &current, policy_id).await?;
    let versions = policies::versions(state.db().pool(), policy_id).await?;

    Ok(Json(json!({
        "policy_id": stored.id,
        "current_version": stored.version,
        "versions": versions
            .iter()
            .map(|version| json!({
                "version": version.version,
                "effect": version.effect.as_str(),
                "priority": version.priority,
                "conditions": version.conditions.to_json(),
                "target_permissions": version.target_permissions,
                "enabled": version.enabled,
                "created_at": version.created_at.format(&Rfc3339).unwrap_or_default(),
                "changed_by": version.changed_by,
            }))
            .collect::<Vec<_>>(),
    })))
}

/// Dry run: what would this policy do, and would it decide?
///
/// Reads nothing into the store and writes nothing — a builder may test an unsaved draft as often
/// as it likes. The answer carries the leaf-by-leaf evaluation so the editor can highlight the
/// conditions that matched, plus the verdict the organization's enabled policies would reach with
/// this policy in place.
pub async fn test_policy(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(policy_id): Path<Uuid>,
    Json(body): Json<PolicyTestRequest>,
) -> Result<Json<Value>, ApiError> {
    let stored = load(&state, &current, policy_id).await?;

    if !catalogue::is_known(body.permission.trim()) {
        return Err(ApiError::bad_request(
            "invalid_policy",
            format!("unknown permission {:?}", body.permission),
        ));
    }
    let permission = body.permission.trim();

    // A draft overrides the stored policy for this run; without one the stored policy answers.
    let (effect, priority, conditions, target_permissions, enabled, is_draft) = match &body.policy {
        Some(draft) => {
            let draft = draft_of(draft)?;
            (
                draft.effect,
                draft.priority,
                draft.conditions,
                draft.target_permissions,
                draft.enabled,
                true,
            )
        }
        None => (
            stored.effect,
            stored.priority,
            stored.conditions.clone(),
            stored.target_permissions.clone(),
            stored.enabled,
            false,
        ),
    };

    // Sample attributes: what the caller supplied, plus the two request facts the engine always
    // knows — the action and the organization.
    let mut sample: Map<String, Value> = body.attributes.as_object().cloned().unwrap_or_default();
    sample
        .entry("action".to_owned())
        .or_insert_with(|| json!(permission));
    sample
        .entry("organization.id".to_owned())
        .or_insert_with(|| json!(stored.organization_id));
    let attributes = Attributes::new(sample);

    let targeted = target_permissions
        .iter()
        .any(|pattern| targets(pattern, permission));
    let conditions_satisfied = conditions.satisfied(&attributes);
    let applies = enabled && targeted && conditions_satisfied;

    let leaves = policies::explain_conditions(&conditions, &attributes);

    // The full set, with this policy in its draft shape, so the answer includes whether it wins.
    let mut set: Vec<omnion_policy_engine::Policy> =
        policies::active(state.db().pool(), stored.organization_id)
            .await?
            .into_iter()
            .filter(|policy| policy.id != stored.id)
            .map(|policy| policy.to_policy())
            .collect();
    set.push(omnion_policy_engine::Policy {
        id: stored.id,
        name: stored.name.clone(),
        effect,
        priority,
        enabled,
        target_permissions: target_permissions.clone(),
        conditions: conditions.clone(),
    });
    let decision = PolicySet::new(set).decide(permission, &attributes);

    Ok(Json(json!({
        "policy_id": stored.id,
        "policy_name": stored.name,
        "permission": permission,
        "draft": is_draft,
        "enabled": enabled,
        "targeted": targeted,
        "conditions_satisfied": conditions_satisfied,
        "applies": applies,
        "effect": effect.as_str(),
        "decides": decision
            .as_ref()
            .is_some_and(|winner| winner.policy_id == stored.id),
        "decision": decision.as_ref().map(|winner| json!({
            "effect": winner.effect.as_str(),
            "policy_id": winner.policy_id,
            "policy_name": winner.policy_name,
            "priority": winner.priority,
        })),
        "trace": leaves.iter().map(|leaf| json!({
            "path": leaf.path,
            "attribute": leaf.attribute,
            "operator": leaf.operator,
            "expected": leaf.expected,
            "resolved": leaf.resolved,
            "satisfied": leaf.satisfied,
        })).collect::<Vec<_>>(),
        "attributes": attributes.as_map(),
        "note": if applies {
            "The policy applies to this request. Saved, it decides unless a policy of higher priority speaks too."
        } else if !enabled {
            "The policy is disabled, so it changes nothing."
        } else if !targeted {
            "The policy does not target this permission."
        } else {
            "The conditions did not hold with these attributes."
        },
    })))
}
