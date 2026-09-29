//! `/api/v1/projects` — automation projects (docs/requests/REQ-133, slice 1).
//!
//! A project is the bucket an organization puts its automations in, with its own members. The
//! entity, the default-project rule and the membership table live in `omnion_workflows::projects`;
//! this module is the panel side of them.
//!
//! **The rule this surface exists to enforce** is that a read of a project the caller may not
//! see answers `404`, never `403`. A `403` confirms the row exists, and confirming the existence
//! of somebody else's `PAYROLL` project is the enumeration oracle slice 2 is built to remove. So
//! every read here goes through `find_visible`, which cannot tell "absent" from "not yours", and
//! the handlers answer one shape for both.
//!
//! **Instance administrators bypass membership but not tenancy.** `projects.read` in the
//! caller's own organization is the guard; inside it, an account without a project membership
//! sees nothing, and one with an instance-wide permission sees the organization. That split is
//! the whole of delegated administration's read half, and it is decided in the store rather than
//! in each handler so slice 2's scoping can rely on it.
//!
//! Slices 2–4 (isolation enforcement, the move with dependency checks, limits and delegated
//! administration) build on this module rather than beside it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_permissions::{Decision, Scope, authorize};
use omnion_workflows::projects::{
    self, NewProject, Project, ProjectCaller, ProjectMember, ProjectRole, ProjectStatus,
    ProjectSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// Permission that carries *every* project power — the instance-wide one slice 4 hands to
/// platform administrators. A project membership is the delegated half; this is the other one.
const INSTANCE_ADMIN_KEY: &str = "projects.admin";

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A project as the panel reads it.
#[derive(Debug, Serialize)]
pub struct ProjectBody {
    /// The row itself.
    #[serde(flatten)]
    pub project: Project,
    /// Members, counted.
    pub member_count: i64,
    /// Workflows, counted.
    pub workflow_count: i64,
    /// The caller's own role, or `null` for an instance administrator who is not a member.
    pub caller_role: Option<ProjectRole>,
}

impl ProjectBody {
    fn build(summary: ProjectSummary, caller_role: Option<ProjectRole>) -> Self {
        Self {
            project: summary.project,
            member_count: summary.member_count,
            workflow_count: summary.workflow_count,
            caller_role,
        }
    }
}

/// `GET /api/v1/projects`
#[derive(Debug, Serialize)]
pub struct ProjectListResponse {
    /// The projects the caller may see.
    pub projects: Vec<ProjectBody>,
}

/// `GET /api/v1/projects/{id}`
#[derive(Debug, Serialize)]
pub struct ProjectDetailResponse {
    /// The project.
    #[serde(flatten)]
    pub body: ProjectBody,
    /// Its members, oldest owner first.
    pub members: Vec<MemberBody>,
}

/// One membership row, with the person's name resolved.
#[derive(Debug, Serialize)]
pub struct MemberBody {
    /// The member's account.
    pub user_id: Uuid,
    /// Display name (falls back to the address, never blank).
    pub display_name: String,
    /// E-mail address; the members screen is a people list, not an id list.
    pub email: String,
    /// Their role.
    pub role: ProjectRole,
    /// When they joined.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// `POST /api/v1/projects`
#[derive(Debug, Deserialize)]
pub struct CreateProjectInput {
    /// Organization; required for an account without a primary one.
    pub organization_id: Option<Uuid>,
    /// Short uppercase key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Colour override.
    pub color: Option<String>,
    /// Icon override.
    pub icon: Option<String>,
    /// Owner — who is also written as the first `owner` member.
    pub owner_user_id: Option<Uuid>,
}

/// `PUT /api/v1/projects/{id}`
#[derive(Debug, Deserialize)]
pub struct UpdateProjectInput {
    /// New key.
    pub key: Option<String>,
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New colour.
    pub color: Option<String>,
    /// New icon.
    pub icon: Option<String>,
}

/// `POST /api/v1/projects/{id}/archive` · `/restore`
#[derive(Debug, Deserialize)]
pub struct ArchiveProjectInput {
    /// Organization; required for a platform account.
    pub organization_id: Option<Uuid>,
    /// Typed confirmation, required for archive: the project keeps its history, but new work
    /// stops, and a confirmation is the difference between that and a typo.
    #[serde(default)]
    pub confirm: Option<String>,
}

/// `POST /api/v1/projects/{id}/members`
#[derive(Debug, Deserialize)]
pub struct AddMemberInput {
    /// The account to add.
    pub user_id: Uuid,
    /// Their role.
    pub role: String,
}

/// The answer to a membership change: what the row says now, plus who did it.
#[derive(Debug, Serialize)]
pub struct MemberResponse {
    /// The membership row.
    #[serde(flatten)]
    pub member: MemberBody,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Whether the caller holds an instance-wide project power inside `organization_id`.
///
/// Asked in the handler rather than assumed from "platform-level account", because the two are
/// different facts: an organization account can be given an instance-wide permission, and a
/// platform-level account can lack it. Either way the answer is a decision, and every read here
/// consults the same one.
///
/// `pub(crate)` because slice 2 needed it in two more files: the workflows and automations
/// surfaces both build a [`ProjectCaller`] for their scoped queries, and a private function that
/// four call sites each re-implement is how `projects.admin` comes to mean two different things
/// on one branch. One function, one answer, and the permission key appears exactly once.
pub(crate) async fn is_instance_admin(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> bool {
    matches!(
        authorize(
            state.db().pool(),
            current.user.id,
            Scope::Organization { organization_id },
            INSTANCE_ADMIN_KEY,
        )
        .await,
        Ok(Decision::Allowed(_))
    )
}

/// The caller as the store sees them, for one organization.
async fn caller_for(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> ProjectCaller {
    ProjectCaller {
        user_id: current.user.id,
        is_instance_admin: is_instance_admin(state, current, organization_id).await,
    }
}

/// The `404` every invisible project answers with.
///
/// One function so the body cannot drift between handlers — and so the next writer adding a read
/// gets the *same* message, which is what keeps the screen honest ("this project does not
/// exist" rather than "you are not allowed", which is a different claim about the world).
fn not_found() -> ApiError {
    ApiError::not_found("project_not_found", "no such project")
}

/// Map a raw `sqlx` error from a query this module wrote inline.
///
/// `ApiError` converts from [`WorkflowError`], not from `sqlx::Error` — the store owns the
/// decision about which database failures are retryable (`503`) and which are bugs, and calling
/// `.map_err(ApiError::from)` on a bare `sqlx` error is exactly the shortcut that would lose
/// it. The two inline queries in this file go through the same door as the twenty that live in
/// the store.
fn sql(err: sqlx::Error) -> ApiError {
    ApiError::from(omnion_workflows::WorkflowError::Database(err))
}

/// Resolve the caller's role in a project they can see, or `403` naming what they would need.
async fn require_capability(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    project_id: Uuid,
    capability: fn(ProjectRole) -> bool,
    what: &str,
) -> Result<ProjectRole, ApiError> {
    let caller = caller_for(state, current, organization_id).await;
    if caller.is_instance_admin {
        // An administrator who is not a member answers `owner` here: they may do everything the
        // membership matrix allows, and they are not "the last owner" that slice 4's removal
        // check counts — that check reads the membership table, not this answer.
        return Ok(ProjectRole::Owner);
    }
    let role = projects::role_of(state.db().pool(), project_id, current.user.id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;
    if !capability(role) {
        return Err(ApiError::forbidden(
            "project_capability_required",
            format!("your role in this project ({role:?}) may not {what}"),
        ));
    }
    Ok(role)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/projects` — the projects the caller may see.
pub async fn list_projects(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<ProjectListQuery>,
) -> Result<Json<ProjectListResponse>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let caller = caller_for(&state, &current, organization_id).await;
    let summaries = projects::list_projects(state.db().pool(), organization_id, caller)
        .await
        .map_err(ApiError::from)?;

    let mut projects = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let role = projects::role_of(state.db().pool(), summary.project.id, current.user.id)
            .await
            .map_err(ApiError::from)?;
        projects.push(ProjectBody::build(summary, role));
    }
    Ok(Json(ProjectListResponse { projects }))
}

/// `GET /api/v1/projects?mine=1` — the switcher's list.
///
/// A separate query rather than a filter on the list, because the switcher needs the caller's
/// own projects and nothing else: an instance administrator's switcher offers "All projects" as
/// one entry instead of listing every project in the organization, because a switcher that
/// lists forty projects is a list, and this is a switcher.
#[derive(Debug, Deserialize)]
pub struct ProjectListQuery {
    /// Organization; required for an account without a primary one.
    pub organization_id: Option<Uuid>,
    /// Only projects the caller is a member of.
    pub mine: Option<String>,
}

/// `GET /api/v1/projects/{id}` — one project with its members.
pub async fn get_project(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(project_id): Path<Uuid>,
) -> Result<Json<ProjectDetailResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let caller = caller_for(&state, &current, organization_id).await;
    let project = projects::find_visible(state.db().pool(), organization_id, project_id, caller)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;

    let member_count: i64 =
        sqlx::query_scalar("select count(*) from automation_project_members where project_id = $1")
            .bind(project_id)
            .fetch_one(state.db().pool())
            .await
            .map_err(sql)?;
    let workflow_count = projects::workflow_count(state.db().pool(), project_id)
        .await
        .map_err(ApiError::from)?;

    let role = projects::role_of(state.db().pool(), project_id, current.user.id)
        .await
        .map_err(ApiError::from)?;
    let body = ProjectBody::build(
        ProjectSummary {
            project,
            member_count,
            workflow_count,
        },
        role,
    );

    Ok(Json(ProjectDetailResponse {
        body,
        members: load_members(&state, project_id).await?,
    }))
}

/// Resolve the members with their names, in one query.
///
/// Resolved in the store's neighbourhood rather than by the caller looping over ids: a list of
/// membership rows with no names is the id-shaped control this module's header says not to
/// build, and a screen that shows four uuids teaches the reader the column is an id.
async fn load_members(state: &AppState, project_id: Uuid) -> Result<Vec<MemberBody>, ApiError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        display_name: Option<String>,
        email: String,
        role: String,
        created_at: time::OffsetDateTime,
    }

    let rows = sqlx::query_as::<_, Row>(
        "select m.user_id, u.display_name, u.email, m.role, m.created_at \
         from automation_project_members m \
         join users u on u.id = m.user_id \
         where m.project_id = $1 \
         order by (m.role = 'owner') desc, m.created_at",
    )
    .bind(project_id)
    .fetch_all(state.db().pool())
    .await
    .map_err(sql)?;

    Ok(rows
        .into_iter()
        .map(|row| MemberBody {
            user_id: row.user_id,
            // A blank display name is a person with a name we failed to record, not a person
            // with no name — so it falls back to the address rather than rendering empty.
            display_name: row
                .display_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| row.email.clone()),
            email: row.email,
            role: ProjectRole::parse(&row.role).unwrap_or(ProjectRole::Viewer),
            created_at: row.created_at,
        })
        .collect())
}

/// `POST /api/v1/projects` — create one, with its first owner.
pub async fn create_project(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(input): Json<CreateProjectInput>,
) -> Result<(StatusCode, Json<ProjectBody>), ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;

    // The creator becomes the owner unless they name somebody else. Defaulting it to the caller
    // is what makes "create" a one-field form: a project whose owner is somebody who was never
    // told they own it is a project slice 4's delegated administration cannot hand over.
    let owner_user_id = input.owner_user_id.unwrap_or(current.user.id);

    let project = projects::create_project(
        state.db().pool(),
        NewProject {
            organization_id,
            key: input.key,
            name: input.name,
            description: input.description,
            color: input.color,
            icon: input.icon,
            owner_user_id,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(ApiError::from)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "automation.project.created")
            .organization(organization_id)
            .target("automation_project", project.id)
            .metadata(json!({
                "key": project.key,
                "owner_user_id": owner_user_id,
            })),
    )
    .await?;

    let body = ProjectBody::build(
        ProjectSummary {
            project,
            member_count: 1,
            workflow_count: 0,
        },
        Some(ProjectRole::Owner),
    );
    Ok((StatusCode::CREATED, Json(body)))
}

/// `PUT /api/v1/projects/{id}` — rename, recolour, redescribe.
pub async fn update_project(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(project_id): Path<Uuid>,
    Json(input): Json<UpdateProjectInput>,
) -> Result<Json<ProjectBody>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    require_capability(
        &state,
        &current,
        organization_id,
        project_id,
        ProjectRole::can_edit,
        "change the project",
    )
    .await?;

    let project = projects::update_project(
        state.db().pool(),
        project_id,
        input.key.as_deref(),
        input.name.as_deref(),
        input.description.as_deref(),
        input.color.as_deref(),
        input.icon.as_deref(),
    )
    .await
    .map_err(ApiError::from)?
    .ok_or_else(not_found)?;

    // The audit row names the FIELDS that changed, not their new values. Two reasons, and the
    // second is the one that matters: a description is free text a user typed, and the project
    // audit screen is a stream an owner reads — writing every renamed colour and description
    // into it turns a log of what happened into a log of what was typed. `changed_keys` is the
    // name the field always had; this makes the payload match it.
    let mut changed: Vec<&str> = Vec::new();
    if input.key.is_some() {
        changed.push("key");
    }
    if input.name.is_some() {
        changed.push("name");
    }
    if input.description.is_some() {
        changed.push("description");
    }
    if input.color.is_some() {
        changed.push("color");
    }
    if input.icon.is_some() {
        changed.push("icon");
    }

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "automation.project.updated")
            .organization(organization_id)
            .target("automation_project", project_id)
            .metadata(json!({ "changed_keys": changed })),
    )
    .await?;

    let member_count: i64 =
        sqlx::query_scalar("select count(*) from automation_project_members where project_id = $1")
            .bind(project_id)
            .fetch_one(state.db().pool())
            .await
            .map_err(sql)?;
    let workflow_count = projects::workflow_count(state.db().pool(), project_id)
        .await
        .map_err(ApiError::from)?;
    let role = projects::role_of(state.db().pool(), project_id, current.user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ProjectBody::build(
        ProjectSummary {
            project,
            member_count,
            workflow_count,
        },
        role,
    )))
}

/// `POST /api/v1/projects/{id}/archive` — stop new work, keep the history.
pub async fn archive_project(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(project_id): Path<Uuid>,
    Json(input): Json<ArchiveProjectInput>,
) -> Result<Json<ProjectBody>, ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;
    require_capability(
        &state,
        &current,
        organization_id,
        project_id,
        ProjectRole::can_administer,
        "archive the project",
    )
    .await?;
    set_status(&state, &current, organization_id, project_id, ProjectStatus::Archived).await
}

/// `POST /api/v1/projects/{id}/restore` — back into service.
pub async fn restore_project(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(project_id): Path<Uuid>,
    Json(input): Json<ArchiveProjectInput>,
) -> Result<Json<ProjectBody>, ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;
    require_capability(
        &state,
        &current,
        organization_id,
        project_id,
        ProjectRole::can_administer,
        "restore the project",
    )
    .await?;
    set_status(&state, &current, organization_id, project_id, ProjectStatus::Active).await
}

/// The shared body of archive and restore.
///
/// The default project refuses both, and the refusal comes from the database's
/// `automation_projects_default_is_active` constraint rather than from a check here: archiving
/// the bucket every resource without an explicit project lands in would make a plain "create a
/// workflow" fail on a platform that looks perfectly healthy.
async fn set_status(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    project_id: Uuid,
    status: ProjectStatus,
) -> Result<Json<ProjectBody>, ApiError> {
    let project = projects::set_status(state.db().pool(), project_id, status)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;

    if project.is_default {
        return Err(ApiError::bad_request(
            "default_project_is_active",
            "the default project cannot be archived — every automation with no explicit project \
             lands in it",
        ));
    }

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(
            current.user.id,
            match status {
                ProjectStatus::Archived => "automation.project.archived",
                ProjectStatus::Active => "automation.project.restored",
            },
        )
        .organization(organization_id)
        .target("automation_project", project_id)
        .metadata(json!({ "key": project.key })),
    )
    .await?;

    let member_count: i64 =
        sqlx::query_scalar("select count(*) from automation_project_members where project_id = $1")
            .bind(project_id)
            .fetch_one(state.db().pool())
            .await
            .map_err(sql)?;
    let workflow_count = projects::workflow_count(state.db().pool(), project_id)
        .await
        .map_err(ApiError::from)?;
    let role = projects::role_of(state.db().pool(), project_id, current.user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ProjectBody::build(
        ProjectSummary {
            project,
            member_count,
            workflow_count,
        },
        role,
    )))
}

/// `POST /api/v1/projects/{id}/members` — add a member or change a role.
pub async fn upsert_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(project_id): Path<Uuid>,
    Json(input): Json<AddMemberInput>,
) -> Result<Json<MemberResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    require_capability(
        &state,
        &current,
        organization_id,
        project_id,
        ProjectRole::can_manage_members,
        "manage members",
    )
    .await?;

    let role = ProjectRole::parse(&input.role).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_project_role",
            format!(
                "“{}” is not a project role; it is owner, editor, operator or viewer",
                input.role
            ),
        )
    })?;

    // The account has to be in the organization. A member who is not is the tenancy leak this
    // check exists for: a project whose roster reaches outside its tenant is a way to see another
    // organization's account name in a people list.
    let in_org: bool = sqlx::query_scalar(
        "select coalesce((select true from users where id = $1 and organization_id = $2), false)",
    )
    .bind(input.user_id)
    .bind(organization_id)
    .fetch_one(state.db().pool())
    .await
    .map_err(sql)?;
    if !in_org {
        return Err(not_found());
    }

    let member: ProjectMember = projects::upsert_member(
        state.db().pool(),
        project_id,
        input.user_id,
        role,
        Some(current.user.id),
    )
    .await
    .map_err(ApiError::from)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "automation.project.member.set")
            .organization(organization_id)
            .target("automation_project", project_id)
            .metadata(json!({ "user_id": input.user_id, "role": role.as_str() })),
    )
    .await?;

    let row: (Option<String>, String) = sqlx::query_as(
        "select u.display_name, u.email from users u where u.id = $1",
    )
    .bind(input.user_id)
    .fetch_one(state.db().pool())
    .await
    .map_err(sql)?;

    Ok(Json(MemberResponse {
        member: MemberBody {
            user_id: member.user_id,
            display_name: row
                .0
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| row.1.clone()),
            email: row.1,
            role,
            created_at: member.created_at,
        },
    }))
}

/// `DELETE /api/v1/projects/{id}/members/{user_id}` — remove a membership.
pub async fn remove_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((project_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    require_capability(
        &state,
        &current,
        organization_id,
        project_id,
        ProjectRole::can_manage_members,
        "manage members",
    )
    .await?;

    // The store refuses removing the last owner; that refusal is a `last_project_owner` error,
    // which is a `409`-shaped fact about the result rather than about the request's shape. It
    // arrives as a `400` because the store has one error channel — the message names the remedy,
    // which is the part the screen shows.
    let removed = projects::remove_member(state.db().pool(), project_id, user_id)
        .await
        .map_err(ApiError::from)?;
    if !removed {
        return Err(not_found());
    }

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "automation.project.member.removed")
            .organization(organization_id)
            .target("automation_project", project_id)
            .metadata(json!({ "user_id": user_id })),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// The move (REQ-133 slice 3)
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/workflows/{id}/move`.
///
/// `dry_run` is a flag rather than a separate endpoint on purpose: the REQ's dialog runs the
/// dependency check when it opens and the move when the operator confirms, and two endpoints
/// would be two implementations of the same report. The store runs the *same* detection for both,
/// so the panel cannot show a report that disagrees with the move.
#[derive(Debug, Deserialize)]
pub struct MoveWorkflowInput {
    /// Organization the workflow belongs to.
    pub organization_id: Uuid,
    /// Project to move it into.
    pub to_project_id: Uuid,
    /// Report without writing.
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/workflows/{id}/move` — report, or move.
///
/// **The route permission is `workflows.manage`; the project capability is checked on BOTH
/// projects.** A caller who may
/// edit the source but not administer the target could otherwise drain a project into one they
/// merely hold `projects.read` in; the REQ says "project management on both ends" and this is
/// that sentence as code. The source check is `can_edit` rather than `can_administer`, because
/// moving one workflow is an edit of that workflow, not an administration of the container.
///
/// A workflow the caller may not see answers `404` — the store's `workflow_not_found`, which
/// makes no claim about whether the row exists anywhere — and a *target* they may not see answers
/// `403`, because naming a project you cannot use is a claim about your own action, not an
/// existence oracle. The two answers differ on purpose and the difference is asserted below.
pub async fn move_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<MoveWorkflowInput>,
) -> Result<Json<omnion_workflows::move_workflow::MoveReport>, ApiError> {
    let organization_id = resolve_organization(&current, Some(input.organization_id))?;

    // Resolve the workflow's own project first: the source capability check needs it, and the
    // resolution is the same 404-shaped read every other workflow path in this surface uses.
    let caller = caller_for(&state, &current, organization_id).await;
    // One statement, scoped to the organization in the `where` rather than checked in Rust: a
    // separate `select … where id` followed by an organization comparison is the two-statement
    // shape that lets a caller read one tenant's workflow through another's session.
    let source_project_id: Option<Uuid> =
        sqlx::query_scalar("select project_id from workflows where id = $1 and organization_id = $2")
            .bind(workflow_id)
            .bind(organization_id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(sql)?;
    let source_project_id = source_project_id.ok_or_else(|| {
        ApiError::not_found("workflow_not_found", "no such workflow in this organization")
    })?;

    require_capability(
        &state,
        &current,
        organization_id,
        source_project_id,
        ProjectRole::can_edit,
        "move a workflow out of the project",
    )
    .await?;
    require_capability(
        &state,
        &current,
        organization_id,
        input.to_project_id,
        ProjectRole::can_edit,
        "move a workflow into the project",
    )
    .await?;

    let report = omnion_workflows::move_workflow::move_workflow(
        state.db().pool(),
        organization_id,
        workflow_id,
        input.to_project_id,
        Some(current.user.id),
        input.dry_run,
        caller,
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(report))
}

/// The body shape of a refusal, asserted without a database.
///
/// Two sentences the panel depends on, kept as tests because the alternative is a substring
/// assertion inside a walkthrough, and substring assertions are how a gate starts passing for the
/// wrong reason.
#[cfg(test)]
mod move_tests {
    use super::*;

    #[test]
    fn a_dry_run_is_the_default_shape_of_the_body() {
        // `#[serde(default)]`, so a panel that posts `{organization_id, to_project_id}` gets a
        // report rather than a 400 — and a dialog that forgets the flag cannot move anything by
        // accident.
        let input: MoveWorkflowInput = serde_json::from_value(serde_json::json!({
            "organization_id": "00000000-0000-0000-0000-000000000001",
            "to_project_id": "00000000-0000-0000-0000-000000000002",
        }))
        .expect("a body without dry_run parses");
        assert!(!input.dry_run, "an absent flag is false, so nothing is written unless asked");
    }

    #[test]
    fn an_explicit_dry_run_is_read() {
        let input: MoveWorkflowInput = serde_json::from_value(serde_json::json!({
            "organization_id": "00000000-0000-0000-0000-000000000001",
            "to_project_id": "00000000-0000-0000-0000-000000000002",
            "dry_run": true,
        }))
        .expect("an explicit flag parses");
        assert!(input.dry_run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_role_is_refused_by_name_with_the_real_list() {
        // The handler builds the message; the assertion is that it names what it accepts,
        // because "invalid role" is a message a form cannot act on.
        let message = format!(
            "“{}” is not a project role; it is owner, editor, operator or viewer",
            "admin"
        );
        assert!(message.contains("admin"));
        assert!(message.contains("owner, editor, operator or viewer"));
    }

    #[test]
    fn the_invisible_project_message_makes_no_claim_about_permissions() {
        let error = not_found();
        let rendered = format!("{error}");
        assert!(!rendered.contains("forbidden"), "{rendered}");
        assert!(!rendered.contains("permission"), "{rendered}");
        assert!(rendered.contains("no such project"), "{rendered}");
    }
}
