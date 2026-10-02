//! `/api/v1/ai/skills` and `/api/v1/ai/agents/{id}/skills` — the skills registry (REQ-099,
//! slice 3).
//!
//! The registry is the one place in the agent runtime where an operator writes text that ends
//! up inside a model's context. Two consequences shape every route here:
//!
//! 1. **A skill never grants a tool.** `POST /ai/agents/{id}/skills` attaches a *relevance
//!    list*; the agent's own `tools` array is the grant. A skill that names `web.search`
//!    attached to an agent without it is accepted and *reported* — the response carries the
//!    mismatch so the Skills tab can say so — but the run's allow-list is unchanged. If this
//!    route could widen an agent's tools, a skill would be a privilege-escalation payload
//!    wearing the costume of a prompt fragment.
//!
//! 2. **Validation runs on the way in *and* on the way out.** `POST /ai/skills/{key}/validate`
//!    answers "would this definition be accepted?" for the form's Run validation button, and
//!    the write path calls the same [`skills::validate`] rather than trusting the caller to
//!    have asked first. The registry is also the place a checksum is recomputed: a client
//!    cannot send its own, because a client that could send its own digest could confirm a
//!    body nobody validated.
//!
//! The order the spec promises — "enabled skills in attached order" — is returned by the
//! agent-scoped route as an explicit `position` per row rather than left to the client to
//! infer from array position, because a drag-to-reorder UI and a `sort()` in the browser
//! disagree exactly when somebody is debugging which skill came first.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use omnion_ai_hub::run_store;
use omnion_ai_hub::skills::{self, NewSkill, SkillChanges};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// One registry row, as the table renders it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillView {
    /// The key — the identity a caller addresses the skill by.
    pub key: String,
    /// Display name.
    pub name: String,
    /// One line about what it is for.
    pub description: String,
    /// When the model should reach for it.
    pub when_to_use: String,
    /// The instruction body, sent whole so the detail drawer can show the real text rather
    /// than a summary of it.
    pub instructions: String,
    /// The tool keys it is relevant to.
    pub tools: Vec<String>,
    /// The manual version.
    pub version: i32,
    /// The digest of the definition, so the drawer can show it without a second request.
    pub checksum: String,
    /// `built_in` or `custom` — which decides whether Delete is offered at all.
    pub source: String,
    /// Whether the runtime will inject it.
    pub enabled: bool,
    /// How many agents hold it, for the "Used by" column.
    pub used_by: i64,
    /// Whether this row is a built-in (hence read-only for a definition change).
    pub built_in: bool,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

impl SkillView {
    /// Build a view, with the usage count already resolved.
    fn build(skill: &skills::Skill, used_by: i64) -> Self {
        Self {
            key: skill.key.clone(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            when_to_use: skill.when_to_use.clone(),
            instructions: skill.instructions.clone(),
            tools: skill.tools.clone(),
            version: skill.version,
            checksum: skill.checksum.clone(),
            source: skill.source.clone(),
            enabled: skill.enabled,
            used_by,
            built_in: skill.is_built_in(),
            updated_at: skill.updated_at,
        }
    }
}

/// One attachment, with the runtime's verdict on it.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentView {
    /// The registry key.
    pub key: String,
    /// Display name, or the bare key when the registry row is gone.
    pub name: String,
    /// Description, empty when stale.
    pub description: String,
    /// When to use it, empty when stale.
    pub when_to_use: String,
    /// Version, 0 when stale.
    pub version: i32,
    /// The tools it names.
    pub tools: Vec<String>,
    /// `built_in` / `custom`, empty when stale.
    pub source: String,
    /// The attachment order.
    pub position: i32,
    /// Whether the runtime would inject it.
    pub injected: bool,
    /// Why not, when it would not. `null` on an injected skill.
    pub withheld_reason: Option<&'static str>,
    /// A stable code for the reason, for a client that wants to branch.
    pub withheld_code: Option<&'static str>,
    /// Whether the row's own checksum still describes its body.
    pub checksum_ok: bool,
}

impl AttachmentView {
    fn build(entry: &skills::AttachedSkill) -> Self {
        let skill = &entry.skill;
        Self {
            key: skill.key.clone(),
            name: if skill.name.is_empty() {
                skill.key.clone()
            } else {
                skill.name.clone()
            },
            description: skill.description.clone(),
            when_to_use: skill.when_to_use.clone(),
            version: skill.version,
            tools: skill.tools.clone(),
            source: skill.source.clone(),
            position: entry.position,
            injected: entry.withheld.is_none(),
            withheld_reason: entry.withheld.map(skills::Withheld::reason),
            withheld_code: entry.withheld.map(skills::Withheld::code),
            checksum_ok: !skill.checksum.is_empty() && skills::checksum_matches(skill),
        }
    }
}

/// The whole Skills tab payload: what is attached, in order, and what would actually reach a
/// prompt.
///
/// `prompt_block` is returned so the tab can show the *assembled* text rather than a
/// reconstruction of it. A panel that shows each skill separately can be right about all of
/// them and still display an order the runtime does not use.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSkillsView {
    /// The agent.
    pub agent_id: Uuid,
    /// Every attachment, injected or not, in runtime order.
    pub skills: Vec<AttachmentView>,
    /// The prompt text the runtime would add, or `null` when nothing is injected.
    pub prompt_block: Option<String>,
    /// The skills that are attached but withheld, as bare keys.
    pub withheld: Vec<String>,
}

/// The registry list.
#[derive(Debug, Clone, Serialize)]
pub struct SkillsList {
    /// This organization's skills plus the built-ins.
    pub skills: Vec<SkillView>,
}

/// Query flags for the registry list.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListQuery {
    /// Only enabled rows — what the attach picker wants.
    #[serde(default)]
    pub enabled_only: bool,
}

/// The body of a create.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateBody {
    /// The key. Immutable afterwards, like an agent's.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// When to reach for it.
    #[serde(default)]
    pub when_to_use: String,
    /// The body.
    pub instructions: String,
    /// Tool keys it is relevant to.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Whether it starts enabled.
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// `true` — serde's `default` for a body that simply omits the flag.
fn yes() -> bool {
    true
}

/// The body of an update. Every field optional; a `null` is not a change, because `Option<T>`
/// cannot tell "absent" from "explicitly null" and a skill's description is a string that
/// legitimately becomes empty.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateBody {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New when-to-use note.
    pub when_to_use: Option<String>,
    /// New body.
    pub instructions: Option<String>,
    /// New tool relevance list.
    pub tools: Option<Vec<String>>,
    /// An explicit version bump.
    pub version: Option<i32>,
    /// Enable or disable.
    pub enabled: Option<bool>,
}

/// The body of an attach.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachBody {
    /// The skill key to attach.
    pub skill_key: String,
}

/// The body of a reorder — the whole list, because a drag produces a list.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderBody {
    /// The keys, in the order they should be injected.
    pub skills: Vec<String>,
}

/// The body of a validation request.
#[derive(Debug, Clone, Deserialize)]
pub struct ValidateBody {
    /// The key being checked.
    #[serde(default)]
    pub key: String,
    /// The name.
    #[serde(default)]
    pub name: String,
    /// The description.
    #[serde(default)]
    pub description: String,
    /// The when-to-use note.
    #[serde(default)]
    pub when_to_use: String,
    /// The body.
    #[serde(default)]
    pub instructions: String,
    /// The tool keys.
    #[serde(default)]
    pub tools: Vec<String>,
    /// A checksum the caller expects, to check against a row it read.
    #[serde(default)]
    pub expected_checksum: Option<String>,
}

/// `GET /api/v1/ai/skills` — the registry.
pub async fn list_skills_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Query(list): Query<ListQuery>,
) -> Result<Json<SkillsList>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let rows = skills::list_skills(state.db().pool(), organization, list.enabled_only).await?;
    let usage = skills::usage_counts(state.db().pool(), organization).await?;
    Ok(Json(SkillsList {
        skills: rows
            .iter()
            .map(|row| SkillView::build(row, usage.get(&row.key).copied().unwrap_or_default()))
            .collect(),
    }))
}

/// `GET /api/v1/ai/skills/{key}` — one definition.
pub async fn get_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<Json<SkillView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(skill) = skills::get_skill(state.db().pool(), organization, &key).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "skill.not_found",
            format!("no skill `{key}` in this registry"),
        ));
    };
    let usage = skills::usage_counts(state.db().pool(), organization).await?;
    Ok(Json(SkillView::build(
        &skill,
        usage.get(&skill.key).copied().unwrap_or_default(),
    )))
}

/// `POST /api/v1/ai/skills` — register a custom definition.
pub async fn create_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Json(body): Json<CreateBody>,
) -> Result<(axum::http::StatusCode, Json<SkillView>), ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let draft = NewSkill {
        organization_id: Some(organization),
        key: body.key,
        name: body.name,
        description: body.description,
        when_to_use: body.when_to_use,
        instructions: body.instructions,
        tools: body.tools,
        source: String::from("custom"),
        enabled: body.enabled,
        created_by: Some(current.user.id),
    };
    // `None` — the installation has no tool catalogue to validate against yet (REQ-100 owns
    // the registry). Passing an empty slice instead would make *every* tool key unknown and
    // the skill form unusable; the honest answer is that the rule is deferred, not satisfied.
    let skill = skills::create_skill(state.db().pool(), &draft, None).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(SkillView::build(&skill, 0)),
    ))
}

/// `PATCH /api/v1/ai/skills/{key}` — change a definition, or enable/disable a built-in.
pub async fn update_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<SkillView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let changes = SkillChanges {
        name: body.name,
        description: body.description,
        when_to_use: body.when_to_use,
        instructions: body.instructions,
        tools: body.tools,
        version: body.version,
        enabled: body.enabled,
    };
    // A built-in may only be enabled or disabled, so the definition fields are dropped before
    // the store sees them — otherwise a caller would get a 403 for a body that only tried to
    // re-send what it already had.
    let is_built_in = skills::get_skill(state.db().pool(), organization, &key)
        .await?
        .map(|row| row.is_built_in())
        .unwrap_or(false);
    let changes = if is_built_in {
        SkillChanges {
            name: None,
            description: None,
            when_to_use: None,
            instructions: None,
            tools: None,
            version: None,
            enabled: changes.enabled,
        }
    } else {
        changes
    };
    let Some(skill) = skills::update_skill(state.db().pool(), organization, &key, &changes, None).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "skill.not_found",
            format!("no skill `{key}` in this registry"),
        ));
    };
    let usage = skills::usage_counts(state.db().pool(), organization).await?;
    Ok(Json(SkillView::build(
        &skill,
        usage.get(&skill.key).copied().unwrap_or_default(),
    )))
}

/// `DELETE /api/v1/ai/skills/{key}` — remove a custom skill.
pub async fn delete_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    // A built-in is refused with a 403 rather than a 404: the row exists, it is simply not the
    // caller's to remove, and a 404 would tell them it had vanished.
    if let Some(row) = skills::get_skill(state.db().pool(), organization, &key).await?
        && row.is_built_in()
    {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "skill.built_in",
            "a built-in skill can be disabled, but not deleted",
        ));
    }
    if skills::delete_skill(state.db().pool(), organization, &key).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "skill.not_found",
            format!("no custom skill `{key}` in this registry"),
        ))
    }
}

/// `POST /api/v1/ai/skills/{key}/validate` — "would this definition be accepted?"
///
/// The form's Run validation button. It writes nothing, and it runs the same
/// [`skills::validate`] the write path runs, so a green answer here is not a hopeful guess —
/// it is the same function's verdict. When the caller sends an expected checksum, the
/// response also reports whether a row it is editing still matches what it read, which is the
/// "stale" case the Skills tab renders as a warning.
pub async fn validate_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
    body: Option<Json<ValidateBody>>,
) -> Result<Json<skills::Validation>, ApiError> {
    let _ = resolve_organization(&current, scope_query.organization_id)?;
    // An absent body is a *legitimate* call: "validate the key alone" is the first thing the
    // form does before the operator has typed anything else, and refusing it would make the
    // Run validation button useless on a fresh form.
    let body = match body {
        Some(Json(body)) => body,
        _ => ValidateBody {
            key: key.clone(),
            name: String::new(),
            description: String::new(),
            when_to_use: String::new(),
            instructions: String::new(),
            tools: Vec::new(),
            expected_checksum: None,
        },
    };
    let draft = NewSkill {
        organization_id: None,
        key: if body.key.is_empty() { key } else { body.key },
        name: body.name,
        description: body.description,
        when_to_use: body.when_to_use,
        instructions: body.instructions,
        tools: body.tools,
        source: String::from("custom"),
        enabled: true,
        created_by: None,
    };
    let verdict = skills::validate(&draft, None);
    // The caller's expectation is checked against the verdict's own digest, so the response
    // answers "is what I have still true" without a second hash implementation in the client.
    // Computed *before* the verdict is moved into the response.
    let expected = body.expected_checksum;
    let matched = expected
        .as_deref()
        .is_some_and(|want| want.eq_ignore_ascii_case(&verdict.checksum));
    Ok(Json(skills::Validation {
        valid: verdict.valid,
        problems: verdict.problems,
        checksum: verdict.checksum,
        checksum_matched: matched,
    }))
}

/// `GET /api/v1/ai/agents/{id}/skills` — the Skills tab payload.
pub async fn list_agent_skills_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
) -> Result<Json<AgentSkillsView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(agent) = run_store::get_agent(state.db().pool(), organization, id).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "agent.not_found",
            "no such agent",
        ));
    };
    let assembly = skills::assemble(state.db().pool(), organization, agent.id).await?;
    let attached = skills::list_agent_skills(state.db().pool(), organization, agent.id).await?;
    Ok(Json(AgentSkillsView {
        agent_id: agent.id,
        skills: attached.iter().map(AttachmentView::build).collect(),
        prompt_block: assembly.prompt_block(),
        withheld: assembly
            .withheld
            .iter()
            .map(|entry| entry.skill.key.clone())
            .collect(),
    }))
}

/// `POST /api/v1/ai/agents/{id}/skills` — attach one skill.
///
/// The response carries the mismatch list explicitly rather than refusing the attach. A skill
/// that says "use web.search" on an agent that cannot call it is *allowed to exist* — the
/// operator may be about to grant the tool — but the tab has to be able to say so out loud,
/// because the alternative is a run that mysteriously ignores half its instructions.
pub async fn attach_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
    Json(body): Json<AttachBody>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(agent) = run_store::get_agent(state.db().pool(), organization, id).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "agent.not_found",
            "no such agent",
        ));
    };
    let entry = skills::attach_skill(
        state.db().pool(),
        organization,
        agent.id,
        &body.skill_key,
        Some(current.user.id),
    )
    .await?;
    let unreachable = entry
        .skill
        .tools
        .iter()
        .filter(|tool| !agent.tools.contains(tool))
        .cloned()
        .collect::<Vec<_>>();
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "skill": AttachmentView::build(&entry),
            "tools_not_in_agent": unreachable,
        })),
    ))
}

/// `PUT /api/v1/ai/agents/{id}/skills` — replace the whole order.
pub async fn set_agent_skills_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(id): Path<Uuid>,
    Json(body): Json<OrderBody>,
) -> Result<Json<AgentSkillsView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(agent) = run_store::get_agent(state.db().pool(), organization, id).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "agent.not_found",
            "no such agent",
        ));
    };
    // Only enabled, existing skills are accepted. A reorder that could re-attach a disabled
    // or deleted skill would make the "disabled is never injected" guarantee depend on which
    // control the operator used.
    for key in &body.skills {
        let Some(row) = skills::get_skill(state.db().pool(), organization, key).await? else {
            return Err(ApiError::bad_request(
                "skill.not_found",
                format!("no skill `{key}` in this registry"),
            ));
        };
        if !row.enabled {
            return Err(ApiError::bad_request(
                "skill.disabled",
                format!("the skill `{key}` is disabled; enable it before attaching it"),
            ));
        }
    }
    skills::set_agent_skills(state.db().pool(), agent.id, &body.skills).await?;
    let assembly = skills::assemble(state.db().pool(), organization, agent.id).await?;
    let attached = skills::list_agent_skills(state.db().pool(), organization, agent.id).await?;
    Ok(Json(AgentSkillsView {
        agent_id: agent.id,
        skills: attached.iter().map(AttachmentView::build).collect(),
        prompt_block: assembly.prompt_block(),
        withheld: assembly
            .withheld
            .iter()
            .map(|entry| entry.skill.key.clone())
            .collect(),
    }))
}

/// `DELETE /api/v1/ai/agents/{id}/skills/{key}` — detach one skill.
pub async fn detach_skill_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path((id, key)): Path<(Uuid, String)>,
) -> Result<axum::http::StatusCode, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(agent) = run_store::get_agent(state.db().pool(), organization, id).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "agent.not_found",
            "no such agent",
        ));
    };
    if skills::detach_skill(state.db().pool(), agent.id, &key).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "skill.not_attached",
            format!("the skill `{key}` is not attached to this agent"),
        ))
    }
}
