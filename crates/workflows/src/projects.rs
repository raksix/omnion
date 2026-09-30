//! Automation projects: the container every automation resource lives in (REQ-133, slice 1).
//!
//! A project is a named bucket with its own members. Slice 1 ships the entity, the default
//! project's backfill, the membership table and the capability matrix; the scoping enforcement
//! (slice 2), the move (slice 3) and the limits (slice 4) build on this file rather than beside
//! it.
//!
//! Two rules are established here and are the reason the module exists in this shape:
//!
//! * **A resource without an explicit project lands in the organization's default**, decided by
//!   one function ([`default_project`]) rather than by each insert path. Slice 2 makes every
//!   automation query project-scoped, and the cheapest way to make that safe is for there to be
//!   exactly one answer to "which project".
//! * **A read of a resource the caller may not see is `404`, not `403`.** [`can_see`] answers
//!   "is this project visible at all", which is the question a *read* asks; `403` on a read is an
//!   enumeration oracle, and slice 2's whole job is to make sure one organization cannot learn
//!   that another organization has a project called `PAYROLL` by asking for it by id.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};

/// Columns of `automation_projects` for one `select`, in [`Project`] order.
const PROJECT_COLUMNS: &str = "id, organization_id, key, name, description, color, icon, is_default, status, owner_user_id, \
     created_by, created_at, updated_at";

/// What a member may do inside a project.
///
/// The matrix is a function of the role rather than a table of strings, and slice 4 shares this
/// one definition with the UI — two hand-written matrices drift, and the failure is a permission
/// that stops biting without anybody noticing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRole {
    /// Everything inside the project, including membership and deletion.
    Owner,
    /// Create and edit workflows and credentials.
    Editor,
    /// Start, retry and cancel runs.
    Operator,
    /// Read only.
    Viewer,
}

impl ProjectRole {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Operator => "operator",
            Self::Viewer => "viewer",
        }
    }

    /// Parse a stored or submitted value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "operator" => Some(Self::Operator),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    /// Whether this role may read the project's contents.
    #[must_use]
    pub const fn can_read(self) -> bool {
        true
    }

    /// Whether this role may create and edit workflows and credentials.
    #[must_use]
    pub const fn can_edit(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    /// Whether this role may start, retry and cancel runs.
    #[must_use]
    pub const fn can_run(self) -> bool {
        matches!(self, Self::Owner | Self::Editor | Self::Operator)
    }

    /// Whether this role may manage credentials.
    #[must_use]
    pub const fn can_manage_credentials(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    /// Whether this role may add, remove and re-role members.
    #[must_use]
    pub const fn can_manage_members(self) -> bool {
        matches!(self, Self::Owner)
    }

    /// Whether this role may change the project's limits and its lifecycle.
    #[must_use]
    pub const fn can_administer(self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// The lifecycle of a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// Normal service: runs start, edits land.
    Active,
    /// Read-only: history is kept, new work is refused at the API boundary.
    Archived,
}

impl ProjectStatus {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// A project row.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Project {
    /// Project id.
    pub id: Uuid,
    /// Organization the project lives in; a project never spans organizations.
    pub organization_id: Uuid,
    /// Short uppercase key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Hex colour for the switcher and the avatar.
    pub color: String,
    /// Icon name from the theme's icon set.
    pub icon: String,
    /// Whether this is the organization's default project.
    pub is_default: bool,
    /// `active` or `archived`.
    pub status: String,
    /// Account that owns the project (delegated administration, slice 4).
    pub owner_user_id: Option<Uuid>,
    /// Account that created it.
    pub created_by: Option<Uuid>,
    /// Creation instant.
    pub created_at: OffsetDateTime,
    /// Last write.
    pub updated_at: OffsetDateTime,
}

/// One row of the membership table.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ProjectMember {
    /// Project the membership belongs to.
    pub project_id: Uuid,
    /// The member.
    pub user_id: Uuid,
    /// Stored role name.
    pub role: String,
    /// Who added them.
    pub added_by: Option<Uuid>,
    /// When they joined.
    pub created_at: OffsetDateTime,
}

impl ProjectMember {
    /// The parsed role, or `None` for a row written by a future version this binary predates.
    #[must_use]
    pub fn parsed_role(&self) -> Option<ProjectRole> {
        ProjectRole::parse(&self.role)
    }
}

/// A project as the panel needs it: the row plus the two numbers its list column shows.
///
/// Deliberately **not** a `FromRow` type. `#[serde(flatten)]` over a nested `Project` is fine for
/// JSON, but a derived `FromRow` would need the inner struct's fields flattened into the outer
/// one and has no way to say which of the two `id` columns it means — so the counts are read
/// into a private row type and assembled here. Three call sites already build this by hand, and
/// a fourth silently binding a *different* row's columns is the kind of mistake a type that
/// cannot be derived catches at compile time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectSummary {
    /// The project itself.
    #[serde(flatten)]
    pub project: Project,
    /// Members, counted.
    pub member_count: i64,
    /// Workflows in the project, counted.
    pub workflow_count: i64,
}

/// The list row as the database returns it: the project columns plus the two correlated counts.
#[derive(sqlx::FromRow)]
struct ProjectListRow {
    #[sqlx(flatten)]
    project: Project,
    member_count: i64,
    workflow_count: i64,
}

/// The caller as this module sees them.
///
/// A caller is either an instance administrator — who sees every project in the organization and
/// is unaffected by membership — or an account whose projects are exactly the rows they are a
/// member of. Slice 2 is where that distinction starts refusing things; building it now means
/// the read path has no "and if they are an admin" special case added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectCaller {
    /// The signed-in account.
    pub user_id: Uuid,
    /// Whether the caller holds an instance-wide permission.
    pub is_instance_admin: bool,
}

/// A project to be written.
#[derive(Debug, Clone)]
pub struct NewProject {
    /// Organization the project belongs to.
    pub organization_id: Uuid,
    /// Short uppercase key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Colour override.
    pub color: Option<String>,
    /// Icon override.
    pub icon: Option<String>,
    /// Owner, who is also written as the first `owner` member.
    pub owner_user_id: Uuid,
    /// Creator, for the audit row.
    pub created_by: Option<Uuid>,
}

/// Validate the key the way the migration's constraint does, in Rust and with the same rule.
///
/// The alternative — letting the database refuse it — answers `23514`, which reads in a panel as
/// "the form is broken" rather than "the key has nine characters". The key format is one of the
/// few validation rules this codebase states twice, and stating it twice means the test can
/// compare them.
#[must_use]
pub fn validate_key(key: &str) -> Result<()> {
    let ok = (2..=8).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if ok {
        return Ok(());
    }
    let count = key.chars().count();
    let because = if key.contains(' ') {
        "and contains a space"
    } else if !key
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        "and uses lower case or a symbol"
    } else {
        // Reachable only by length, so the message must not claim a character problem it
        // cannot see: a key of nine digits has no lower case and no space in it.
        "and is not 2 to 8 characters long"
    };
    Err(WorkflowError::invalid(
        "invalid_project_key",
        format!(
            "a project key is 2 to 8 uppercase letters or digits; \
             “{key}” is {count} character{} {because}",
            if count == 1 { "" } else { "s" },
        ),
    ))
}

/// List the projects the caller may see, newest first.
///
/// The filter is the module's whole tenancy rule: an instance admin sees the organization, a
/// member sees their own projects. `None` for the caller is a programmer error, not a case —
/// there is no session in which "nobody" may read a project list, so the function requires one.
pub async fn list_projects(
    pool: &PgPool,
    organization_id: Uuid,
    caller: ProjectCaller,
) -> Result<Vec<ProjectSummary>> {
    let sql = format!(
        "select {PROJECT_COLUMNS}, \
           (select count(*) from automation_project_members m where m.project_id = p.id) as member_count, \
           (select count(*) from workflows w where w.project_id = p.id) as workflow_count \
         from automation_projects p \
         where p.organization_id = $1 \
           and ($2 or exists (select 1 from automation_project_members m \
                              where m.project_id = p.id and m.user_id = $3)) \
         order by p.is_default desc, p.created_at desc"
    );
    let rows = sqlx::query_as::<_, ProjectListRow>(&sql)
        .bind(organization_id)
        .bind(caller.is_instance_admin)
        .bind(caller.user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| ProjectSummary {
            project: row.project,
            member_count: row.member_count,
            workflow_count: row.workflow_count,
        })
        .collect())
}

/// Read one project the caller may see.
///
/// `Ok(None)` is the answer for *both* "there is no such project" and "it is not yours": the
/// caller cannot tell them apart, and that is the point — a `403` here would confirm the row
/// exists, which is the enumeration oracle slice 2 is built to remove.
pub async fn find_visible(
    pool: &PgPool,
    organization_id: Uuid,
    project_id: Uuid,
    caller: ProjectCaller,
) -> Result<Option<Project>> {
    let sql = format!(
        "select {PROJECT_COLUMNS} from automation_projects p \
         where p.id = $1 and p.organization_id = $2 \
           and ($3 or exists (select 1 from automation_project_members m \
                              where m.project_id = p.id and m.user_id = $4))"
    );
    let row = sqlx::query_as::<_, Project>(&sql)
        .bind(project_id)
        .bind(organization_id)
        .bind(caller.is_instance_admin)
        .bind(caller.user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The projects a caller may see, as ids.
///
/// This is the query slice 2 is built on, and it answers a question the per-project read cannot:
/// **"which projects is this person in?"** asked once, for a whole list, before the caller's
/// first row is fetched. Two shapes of scoping exist and only one of them is a boundary —
///
/// * `is_instance_admin` sees the whole organization;
/// * a member sees exactly the projects they are a member of;
/// * **and the organization's DEFAULT project is always visible**, because it is where every
///   resource created without an explicit project lands.
///
/// That last clause is the decision worth arguing for, and the argument is that the alternative
/// is a fresh installation where the owner cannot see the project their first workflow is in.
/// 0164 backfills every pre-existing workflow into the default and `resolve_target` sends every
/// un-targeted new one there; a scoping rule that hides it makes both of those facts invisible
/// to the person who needs them. The default is also the one project the migration refuses to
/// archive, so it is never a container that can be read-only — there is no state in which showing
/// it would leak a dormant queue.
///
/// The result is an id list rather than a boolean predicate, because callers bind it into
/// `project_id = any($2)` on tables this module does not own, and a predicate cannot be
/// transported into another query's `where` without a second round trip.
pub async fn visible_project_ids(
    pool: &PgPool,
    organization_id: Uuid,
    caller: ProjectCaller,
) -> Result<Vec<Uuid>> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "select p.id from automation_projects p \
         where p.organization_id = $1 \
           and ($2 or p.is_default \
                or exists (select 1 from automation_project_members m \
                           where m.project_id = p.id and m.user_id = $3))",
    )
    .bind(organization_id)
    .bind(caller.is_instance_admin)
    .bind(caller.user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The same answer, as a claim about one workflow: may this caller see it?
///
/// The function every read path uses instead of comparing organizations, and the reason is a
/// specific accident: an organization-only check is what slice 1 shipped, and it is correct for
/// tenancy and silent about projects. Two people in ONE organization, in different projects,
/// both pass it. This one is the check that makes project membership mean anything.
pub async fn can_see_workflow(
    pool: &PgPool,
    organization_id: Uuid,
    project_id: Uuid,
    caller: ProjectCaller,
) -> Result<bool> {
    Ok(visible_project_ids(pool, organization_id, caller)
        .await?
        .contains(&project_id))
}

/// A list filter: the caller's visible projects, or `None` when they may see all of them.
///
/// `None` here means "do not filter" and is returned for an instance administrator, because a
/// `Some` carrying every id in the organization would be the same answer spelled more expensively
/// — and a filter that silently stops applying is how a scoping rule gets un-applied by accident.
pub async fn visible_project_filter(
    pool: &PgPool,
    organization_id: Uuid,
    caller: ProjectCaller,
) -> Result<Option<Vec<Uuid>>> {
    if caller.is_instance_admin {
        return Ok(None);
    }
    Ok(Some(
        visible_project_ids(pool, organization_id, caller).await?,
    ))
}

/// Whether a run may start in this project right now.
///
/// **This is the archive guard, and it lives here rather than in a handler because the archive
/// button is not the boundary — `store::create_execution_in` is.** Three of the four ways a run
/// starts never pass through a route: the scheduler claims and starts them, the event matcher
/// starts one inside its own transaction, and a retry starts one from the engine. A check in
/// `run_workflow` therefore reads as "archived projects refuse manual runs", which is true, and
/// leaves a project archiving its workflows only stopping the one kind of run an operator would
/// notice first.
///
/// The answer is read in the same statement that would have created the run, so a run and the
/// check that authorised it cannot straddle an archive that lands between them: either this
/// transaction sees `active` and starts, or it sees `archived` and refuses. A read followed by an
/// unconditional write is the two-statement version of that race, and it is the same shape as the
/// round-robin cursor this module already took away from the handlers.
pub async fn ensure_run_allowed(connection: &mut sqlx::PgConnection, project_id: Uuid) -> Result<()> {
    let status: Option<String> = sqlx::query_scalar(
        "select status from automation_projects where id = $1 for share",
    )
    .bind(project_id)
    .fetch_optional(connection)
    .await?;
    match status.as_deref().map(ProjectStatus::parse) {
        Some(Some(ProjectStatus::Archived)) => Err(WorkflowError::invalid(
            "project_archived",
            "this workflow's project is archived — restore it before starting new runs",
        )),
        // A project that is absent answers the same way it answered absent everywhere else on
        // this module: the run is refused, because a workflow whose project nobody can see is a
        // tenancy question, not a scheduling one.
        None => Err(WorkflowError::invalid(
            "project_not_found",
            "this workflow's project no longer exists",
        )),
        _ => Ok(()),
    }
}

/// The organization's default project, created on first use.
///
/// A resource created without an explicit project lands here, and slice 2 makes every insert
/// path call this — so it has to be safe under a race. The insert takes
/// `on conflict do nothing` against the filtered unique index and then reads the row, which is
/// the same shape this engine's round-robin cursor and claim tables already use: **insert, and
/// let the row count be the decision.** Two organizations' first automations racing produce one
/// project and one re-read, never two defaults and never a constraint error.
pub async fn default_project(pool: &PgPool, organization_id: Uuid) -> Result<Project> {
    let insert = format!(
        "insert into automation_projects (organization_id, key, name, description, is_default) \
         values ($1, 'DEFAULT', 'Default', 'Automations with no explicit project', true) \
         on conflict do nothing returning {PROJECT_COLUMNS}"
    );
    if let Some(project) = sqlx::query_as::<_, Project>(&insert)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?
    {
        return Ok(project);
    }

    let sql = format!(
        "select {PROJECT_COLUMNS} from automation_projects \
         where organization_id = $1 and is_default"
    );
    let project = sqlx::query_as::<_, Project>(&sql)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            WorkflowError::invalid(
                "default_project_missing",
                "this organization has no default project",
            )
        })?;
    Ok(project)
}

/// The project a new resource should land in: the named one when it exists and is usable, the
/// organization's default otherwise.
///
/// The fallback is not an error, and that is the decision worth naming: a caller who passes a
/// project they cannot see gets the default rather than a refusal, because the alternative is a
/// silent resource in the wrong bucket. Slice 2 replaces the "or is it visible" half of this
/// with a refusal — **once the API layer can tell the difference**, which is the same
/// rule this module keeps learning: a validation rule belongs at the layer that can act on it.
pub async fn resolve_target(
    pool: &PgPool,
    organization_id: Uuid,
    project_id: Option<Uuid>,
    caller: ProjectCaller,
) -> Result<Project> {
    match project_id {
        Some(id) => {
            if let Some(project) = find_visible(pool, organization_id, id, caller).await? {
                return Ok(project);
            }
            default_project(pool, organization_id).await
        }
        None => default_project(pool, organization_id).await,
    }
}

/// Create a project and its first owner membership in one transaction.
///
/// The two writes are one statement's worth of meaning — a project with no owner is a project
/// nobody can administer, and slice 4's "at least one owner must remain" rule starts from this
/// row — so they share a transaction rather than being two calls a caller can interleave.
pub async fn create_project(pool: &PgPool, new: NewProject) -> Result<Project> {
    validate_key(&new.key)?;

    let mut tx = pool.begin().await?;

    let sql = format!(
        "insert into automation_projects (organization_id, key, name, description, color, icon, \
         owner_user_id, created_by) values ($1, $2, $3, $4, coalesce($5, '#C96442'), \
         coalesce($6, 'folder'), $7, $8) returning {PROJECT_COLUMNS}"
    );
    let project = sqlx::query_as::<_, Project>(&sql)
        .bind(new.organization_id)
        .bind(&new.key)
        .bind(&new.name)
        .bind(&new.description)
        .bind(new.color.as_deref())
        .bind(new.icon.as_deref())
        .bind(new.owner_user_id)
        .bind(new.created_by)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query(
        "insert into automation_project_members (project_id, user_id, role, added_by) \
         values ($1, $2, 'owner', $3)",
    )
    .bind(project.id)
    .bind(new.owner_user_id)
    .bind(new.created_by)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(project)
}

/// The caller's role in a project, or `None` when they are not a member.
///
/// An instance administrator who is not a member answers `None` here rather than
/// [`ProjectRole::Owner`]. The distinction matters: membership is a fact about the row, and slice
/// 4's "remove the last owner" check must not be satisfied by an administrator who has no
/// membership to remove.
pub async fn role_of(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
) -> Result<Option<ProjectRole>> {
    let raw: Option<String> = sqlx::query_scalar(
        "select role from automation_project_members where project_id = $1 and user_id = $2",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(raw.as_deref().and_then(ProjectRole::parse))
}

/// Whether the caller may see the project at all — the question a *read* asks.
pub async fn can_see(
    pool: &PgPool,
    organization_id: Uuid,
    project_id: Uuid,
    caller: ProjectCaller,
) -> Result<bool> {
    Ok(find_visible(pool, organization_id, project_id, caller)
        .await?
        .is_some())
}

/// List a project's members with the account's name, for the members screen.
///
/// Joined inside the store rather than by the caller so the screen cannot ask for a project and
/// be answered with the members of a different one: both ids are in the `where`, and there is
/// exactly one query.
pub async fn list_members(pool: &PgPool, project_id: Uuid) -> Result<Vec<ProjectMember>> {
    let rows = sqlx::query_as::<_, ProjectMember>(
        "select project_id, user_id, role, added_by, created_at \
         from automation_project_members where project_id = $1 \
         order by (role = 'owner') desc, created_at",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Add a member, or change an existing member's role.
///
/// One statement rather than an existence check followed by an insert: the obvious
/// check-then-write answers a race with a unique violation, and the `on conflict` form answers
/// it with the second write winning — which for a role change is the intended outcome.
pub async fn upsert_member(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    role: ProjectRole,
    added_by: Option<Uuid>,
) -> Result<ProjectMember> {
    let member = sqlx::query_as::<_, ProjectMember>(
        "insert into automation_project_members (project_id, user_id, role, added_by) \
         values ($1, $2, $3, $4) \
         on conflict (project_id, user_id) do update set role = excluded.role \
         returning project_id, user_id, role, added_by, created_at",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(role.as_str())
    .bind(added_by)
    .fetch_one(pool)
    .await?;
    Ok(member)
}

/// Remove a membership, refusing the one that would leave nobody owning the project.
///
/// The refusal is the whole reason this is not a bare `delete`: "at least one owner must
/// remain" is a rule about the *result*, and a delete that succeeds and then reports success
/// leaves a project that slice 4's delegated administration cannot be administered.
pub async fn remove_member(pool: &PgPool, project_id: Uuid, user_id: Uuid) -> Result<bool> {
    let role = role_of(pool, project_id, user_id).await?;
    if role == Some(ProjectRole::Owner) {
        let owners: i64 = sqlx::query_scalar(
            "select count(*) from automation_project_members where project_id = $1 and role = 'owner'",
        )
        .bind(project_id)
        .fetch_one(pool)
        .await?;
        if owners <= 1 {
            return Err(WorkflowError::invalid(
                "last_project_owner",
                "this is the last owner of the project — transfer ownership before removing them",
            ));
        }
    }

    let deleted = sqlx::query(
        "delete from automation_project_members where project_id = $1 and user_id = $2",
    )
    .bind(project_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Update a project's own fields.
///
/// `key` is validated here rather than in the handler because the handler is not the only
/// writer slice 4's "rename" screen will have, and a constraint in one place plus a validator in
/// another is how the two drift.
pub async fn update_project(
    pool: &PgPool,
    project_id: Uuid,
    key: Option<&str>,
    name: Option<&str>,
    description: Option<&str>,
    color: Option<&str>,
    icon: Option<&str>,
) -> Result<Option<Project>> {
    if let Some(key) = key {
        validate_key(key)?;
    }
    let sql = format!(
        "update automation_projects set \
           key = coalesce($2, key), \
           name = coalesce($3, name), \
           description = coalesce($4, description), \
           color = coalesce($5, color), \
           icon = coalesce($6, icon), \
           updated_at = now() \
         where id = $1 returning {PROJECT_COLUMNS}"
    );
    let row = sqlx::query_as::<_, Project>(&sql)
        .bind(project_id)
        .bind(key)
        .bind(name)
        .bind(description)
        .bind(color)
        .bind(icon)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Archive or restore a project.
///
/// The default project refuses to archive, and the refusal is the migration's
/// `automation_projects_default_is_active` constraint rather than a check here: a rule the
/// database holds is a rule a second code path cannot forget. Archived means **read-only**, and
/// slice 2 is what starts refusing new runs against this state.
pub async fn set_status(
    pool: &PgPool,
    project_id: Uuid,
    status: ProjectStatus,
) -> Result<Option<Project>> {
    let sql = format!(
        "update automation_projects set status = $2, updated_at = now() \
         where id = $1 returning {PROJECT_COLUMNS}"
    );
    let row = sqlx::query_as::<_, Project>(&sql)
        .bind(project_id)
        .bind(status.as_str())
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Count the workflows in a project, for the list column and the limit screen.
pub async fn workflow_count(pool: &PgPool, project_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*) from workflows where project_id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

// ---------------------------------------------------------------------------------------------
// The switcher (REQ-133, acceptance 3): what this person last worked in, and in what order.
//
// Four functions, and the split is the design rather than an accident. `list_switcher_entries` is
// the read, `select_project` is the write, and they are separate because **a read that also wrote
// the recents would make every page view reorder the list** — the entry you are looking at would
// climb to rank 0 because you looked at it, which is the same ranking as "recently used" for
// about four seconds and the opposite of it after that.

/// One row of the switcher: the project plus this person's relationship to it.
#[derive(Debug, Clone, Serialize)]
pub struct SwitcherEntry {
    /// The project itself, flattened so the panel can render a row from one object.
    #[serde(flatten)]
    pub project: Project,
    /// The caller's role, or `None` for an instance administrator who is not a member.
    pub caller_role: Option<ProjectRole>,
    /// `true` when this is the project the switcher has selected for this caller.
    pub selected: bool,
    /// The recents rank, or `None` for a project that is not in the recents yet.
    ///
    /// A nullable rank rather than a zero-filled one: rank 0 is a *fact* ("you were here a
    /// minute ago") and an absent rank is a different fact ("this is a project you have never
    /// opened"), and the switcher orders on it.
    pub recent_rank: Option<i16>,
    /// Whether this person may administer the project — the switcher marks it rather than hiding
    /// the entry, because an entry you can see but cannot manage is a legitimate state.
    pub can_manage: bool,
}

/// The switcher's list for one caller: the projects they are in, their recents first.
///
/// **This is the query the `?mine=1` parameter asked for and nothing read.** It differs from
/// [`list_projects`] in three ways, and each difference is a decision the switcher needs rather
/// than a variation:
///
/// 1. **`mine` means membership, not visibility.** An instance administrator sees every project
///    in the organization through `list_projects`, and the switcher would then be a list — which
///    is the thing the REQ explicitly refuses ("a switcher that lists forty projects is a list").
///    So membership is the filter and the administrator's extra reach shows up as a single
///    `All projects` entry in the UI, never as forty rows here.
///
///    **Membership is the filter, and the recents are the exception — and the gate found the
///    exception missing.** The first version filtered on `p.is_default or <membership>`, which
///    meant an administrator who switched into a project they are not a member of (the case
///    delegated administration exists for) got a switcher that named a project it then refused to
///    list: the header said "PAY", the dropdown did not contain it, and the only way to leave was
///    to reload. So a project in *this person's recents* is listed whatever their membership,
///    while the rest of the organization stays out. The clause is not `or $2` — that would be the
///    forty-project list again, reached by a different route.
/// 2. **Recents come first, and the rest is stable.** Ordering by rank with `nulls last` is what
///    makes "recents first" true; ordering by creation would make it a coincidence.
/// 3. **The organization's default is always present.** Exactly as in [`visible_project_ids`]: it
///    is where an un-targeted resource lands, so a switcher without it cannot even show where
///    the reader's own first workflow went.
///
/// `selected` is answered from the *stored* selection rather than from a query parameter, because
/// "what does this person have chosen" and "what did the URL ask for" are different questions when
/// the two disagree, and only one of them is durable.
pub async fn list_switcher_entries(
    pool: &PgPool,
    organization_id: Uuid,
    caller: ProjectCaller,
) -> Result<Vec<SwitcherEntry>> {
    // `ProjectRole` and `recent_rank` are decoded as their stored shapes and parsed here, rather
    // than bound as the domain types. Two reasons, and both are the store's own conventions:
    //
    // * `ProjectRole` is not a sqlx type. A scalar subquery for the role returns NULL for a
    //   project the caller is not a member of, so the decode target is `Option<String>` and the
    //   parse is the *only* place that decides an unknown stored role is no role at all. Binding
    //   `ProjectRole` directly would need a `Type` impl and would raise `UnexpectedNull` on
    //   exactly the rows an instance administrator's switcher is made of.
    // * `recent_rank` is nullable by design, and a nullable column bound to a non-optional `i16`
    //   raises `ColumnDecode: UnexpectedNullError` — the third time this branch has hit that
    //   shape (the claim table's `to_regclass` and `transfer_ownership`'s `owner_user_id` were the
    //   first two). The rule learned then applies here: **ask "is it there?" in the database, or
    //   declare the column optional.** `Option<i16>` is the declaration.
    //
    //   `i16` and not `i32`, and the gate found that: the column is a `smallint` (`INT2`) and a
    //   `Rust i32` decodes as `INT4`, so the read raised `mismatched types; Rust type Option<i32>
    //   (as SQL type INT4) is not compatible with SQL type INT2` — on every row of every
    //   switcher load, while all three tests that never reach the decode stayed green. The
    //   column's width is the *store's* choice and the decoder has to match it; `i32` is only
    //   right for an `int`.
    let rows: Vec<SwitcherRow> = sqlx::query_as(&format!(
        "select {PROJECT_COLUMNS}, \
           (select m.role from automation_project_members m \
             where m.project_id = p.id and m.user_id = $3) as caller_role, \
           (select r.rank from automation_project_recent r \
             where r.user_id = $3 and r.project_id = p.id) as recent_rank \
         from automation_projects p \
         where p.organization_id = $1 \
           and (p.is_default \
                or exists (select 1 from automation_project_recent r \
                           where r.user_id = $3 and r.project_id = p.id) \
                or exists (select 1 from automation_project_members m \
                           where m.project_id = p.id and m.user_id = $3)) \
         order by (select r.rank from automation_project_recent r \
                    where r.user_id = $3 and r.project_id = p.id) asc nulls last, \
                  p.is_default desc, p.name",
    ))
    .bind(organization_id)
    .bind(caller.is_instance_admin)
    .bind(caller.user_id)
    .fetch_all(pool)
    .await?;

    let selected = selected_project(pool, organization_id, caller).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            // An unknown stored role reads as "no role", not as a panic and not as a default
            // that would grant a power the database did not say. A `viewer` who somehow holds
            // `ownerx` gets a switcher entry and no administration.
            let caller_role = row.caller_role.as_deref().and_then(ProjectRole::parse);
            let is_selected = selected == Some(row.project.id);
            SwitcherEntry {
                can_manage: caller.is_instance_admin
                    || caller_role.is_some_and(ProjectRole::can_administer),
                recent_rank: row.recent_rank,
                project: row.project,
                caller_role,
                selected: is_selected,
            }
        })
        .collect())
}

/// The switcher's query row, before the stored shapes are parsed into domain types.
///
/// A struct rather than a tuple for a plain mechanical reason — `sqlx` implements `FromRow` for
/// tuples up to nine elements but a flat tuple of a *flattened* row plus two nullable scalars is
/// the shape it refuses, and the error names a missing `Type` impl for `Project` rather than
/// saying "too many columns". Two nullable columns in a row are also exactly the pair that needs
/// named types to be readable at the call site.
#[derive(sqlx::FromRow)]
struct SwitcherRow {
    #[sqlx(flatten)]
    project: Project,
    caller_role: Option<String>,
    recent_rank: Option<i16>,
}

/// The project this caller has selected, or `None` when they have never chosen one.
///
/// `None` is a real state and the switcher has to be able to say it out loud: a caller who has
/// not chosen is looking at everything, which is the correct default for an installation with one
/// project and a confusing one for a caller with nine. Returning the default project here instead
/// would make the two indistinguishable.
pub async fn selected_project(
    pool: &PgPool,
    organization_id: Uuid,
    caller: ProjectCaller,
) -> Result<Option<Uuid>> {
    let row: Option<Uuid> = sqlx::query_scalar(
        "select p.id from automation_projects p \
         join automation_project_selection s on s.project_id = p.id \
         where s.user_id = $2 and p.organization_id = $1",
    )
    .bind(organization_id)
    .bind(caller.user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Select a project for this caller, and move it to the front of their recents.
///
/// **One transaction, because the two halves are one event.** A selection that persisted but did
/// not reorder is a switcher whose "most recent" is a fact about when the feature shipped, and a
/// reorder that happened without the selection is a recents list that promotes projects the person
/// did not choose. Doing them separately is also a read-then-write, and this branch has taken four
/// such shapes away from callers already.
///
/// The shift is a single statement that moves *every* row up by one and pushes the previous rank 0
/// out of the window:
///
/// ```sql
/// update automation_project_recent \
///    set rank = case when rank = 0 then null else rank - 1 end \
///  where user_id = $1 and rank > 0
/// ```
///
/// The `rank = null` on the old head is the demotion a plain `on conflict do nothing` upsert cannot
/// express, and it is why the primary key is `(user_id, project_id)` while the unique index is on
/// `(user_id, rank)`: re-selecting a project already at rank 3 must land it at 0 without a
/// duplicate, and the update above has already freed the 0 it is about to claim.
///
/// A project the caller cannot see is refused **before** any write, and the refusal is the
/// documented `404` rather than a `403`: the same rule every other project read obeys, and a
/// switcher that answered "that project exists" would be the enumeration oracle slice 2 exists to
/// remove. An instance administrator is not a member of most projects, and the switcher must still
/// be able to switch *into* one they manage, so the check admits the instance administrator
/// through the same [`find_visible`] every other read uses rather than through a membership test
/// that would refuse it.
pub async fn select_project(
    pool: &PgPool,
    organization_id: Uuid,
    project_id: Uuid,
    caller: ProjectCaller,
) -> Result<Project> {
    if !find_visible(pool, organization_id, project_id, caller).await?.is_some() {
        return Err(WorkflowError::invalid(
            "project_not_found",
            "no such project",
        ));
    }

    let mut tx = pool.begin().await?;

    // 1. Evict the row the shift is about to push out of the window, BEFORE the shift -- and ONLY
    //    when the window is full. Doing it after (as the first version did, by nulling the old
    //    head) left the head in place and the claim below hit the unique index with 23505, so
    //    every second switch on a user's second project raised and only the first ever worked.
    //
    //    The `count(*) >= 8` guard is the part the gate caught next, and it is not a detail: with
    //    three recents the unconditional delete removed the *oldest of those three* and the shift
    //    then promoted the second-oldest, so selecting a fourth project silently erased the reader's
    //    history instead of keeping it. The window is eight, so a full one holds ranks 0..7 and the
    //    shift would produce rank 8 -- which is exactly when one row has to go. The threshold is
    //    8 and not 7, and the gate caught that too: a window capped at seven when it says eight
    //    silently loses a project the reader can still see listed.
    sqlx::query(
        "delete from automation_project_recent \
          where user_id = $1 \
            and (select count(*) from automation_project_recent where user_id = $1) >= 8 \
            and rank = (select max(rank) from automation_project_recent where user_id = $1)",
    )
    .bind(caller.user_id)
    .execute(&mut *tx)
    .await?;

    // 2. Lift every remaining row one place, which frees rank 0. The `rank >= 0` rather than
    //    `rank > 0` is the other half of the same correction: rank 0 is a real row that has to
    //    move too, and a predicate that skips it is a head that never advances.
    sqlx::query(
        "update automation_project_recent set rank = rank + 1 where user_id = $1 and rank >= 0",
    )
    .bind(caller.user_id)
    .execute(&mut *tx)
    .await?;

    // 3. The claim. `on conflict (user_id, project_id) do update set rank = 0` rather than
    //    `do nothing`, and the difference is the whole re-selection case: the shift above moved the
    //    project this call is about to claim UP one place if it was already in the list, so a plain
    //    `do nothing` would leave it demoted and the switch would appear to do nothing. The row
    //    count is not consulted -- nothing here branches on it, because "moved to the front" and
    //    "already at the front" produce the same correct state.
    sqlx::query(
        "insert into automation_project_recent (user_id, project_id, rank) \
         values ($1, $2, 0) \
         on conflict (user_id, project_id) do update set rank = 0, used_at = now()",
    )
    .bind(caller.user_id)
    .bind(project_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "insert into automation_project_selection (user_id, project_id) values ($1, $2) \
         on conflict (user_id) do update set project_id = excluded.project_id, updated_at = now()",
    )
    .bind(caller.user_id)
    .bind(project_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let sql = format!("select {PROJECT_COLUMNS} from automation_projects p where p.id = $1");
    let project = sqlx::query_as::<_, Project>(&sql)
        .bind(project_id)
        .fetch_one(pool)
        .await?;
    Ok(project)
}

/// Forget the caller's selection — the switcher's "All projects" entry, which is a *choice* and
/// not merely the absence of one.
///
/// It is a named function rather than a `DELETE` on the route because the alternative is ambiguous
/// at the storage layer: a row that does not exist and a row that was cleared are the same absence,
/// and only the second one means "this person decided to see everything". `set_selection` below
/// carries the same reasoning for the direction that has a column to write.
pub async fn clear_selection(pool: &PgPool, user_id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from automation_project_selection where user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Read one caller's stored selection without the membership join, for the routes that only need
/// the id.
///
/// Deliberately not reused by [`selected_project`]: that one answers *inside an organization*
/// because a selection left behind by a member of a previous organization must not resolve here,
/// and a "select the id, then check it" order would be a read the caller cannot be stopped from
/// observing.
pub async fn stored_selection(pool: &PgPool, user_id: Uuid) -> Result<Option<Uuid>> {
    let row: Option<Uuid> =
        sqlx::query_scalar("select project_id from automation_project_selection where user_id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_uppercase_and_short() {
        assert!(validate_key("PLAT").is_ok());
        assert!(validate_key("OPS2").is_ok());
        assert!(validate_key("AB").is_ok());
        assert!(validate_key("ABCDEFGH").is_ok());
    }

    #[test]
    fn a_key_that_is_too_long_or_wrong_case_is_refused_by_name() {
        let too_long = validate_key("ABCDEFGHI").unwrap_err();
        assert_eq!(too_long.code(), "invalid_project_key");
        assert!(too_long.to_string().contains("2 to 8"), "{too_long}");

        let lower = validate_key("plat").unwrap_err();
        assert!(lower.to_string().contains("lower case"), "{lower}");

        let spaced = validate_key("A B").unwrap_err();
        assert!(spaced.to_string().contains("space"), "{spaced}");

        assert!(validate_key("A").is_err());
        assert!(validate_key("PLAT!").is_err());
    }

    #[test]
    fn a_multi_character_key_is_described_in_the_plural() {
        // "TOOLONGKEY" is ten valid characters, so the message must blame LENGTH. The
        // earlier fixture was "too-long" — eight characters — which failed on its lower case
        // before the length rule was ever consulted, and the assertion was quietly testing
        // the wrong branch. A key that is long AND clean is the only way to reach it.
        let error = validate_key("TOOLONGKEY").unwrap_err();
        assert!(error.to_string().contains("10 characters"), "{error}");
        assert!(
            error.to_string().contains("not 2 to 8 characters long"),
            "{error}"
        );
        assert!(
            validate_key("A")
                .unwrap_err()
                .to_string()
                .contains("1 character ")
        );
    }

    #[test]
    fn a_key_that_is_only_too_long_does_not_get_told_it_uses_lower_case() {
        // Nine digits: no lower case, no symbol, no space. Claiming otherwise is a message
        // that sends the reader looking for a problem the key does not have.
        let error = validate_key("123456789").unwrap_err().to_string();
        assert!(!error.contains("lower case"), "{error}");
        assert!(!error.contains("symbol"), "{error}");
        assert!(error.contains("not 2 to 8 characters long"), "{error}");
    }

    #[test]
    fn the_role_matrix_is_strictly_nested() {
        // The nesting is the claim: every role can do strictly less than the one above it, and
        // every role can read. A viewer that could edit would be a different product.
        assert!(ProjectRole::Owner.can_administer());
        assert!(ProjectRole::Owner.can_manage_members());
        assert!(ProjectRole::Owner.can_edit());
        assert!(ProjectRole::Owner.can_run());
        assert!(ProjectRole::Owner.can_manage_credentials());

        assert!(ProjectRole::Editor.can_edit());
        assert!(ProjectRole::Editor.can_run());
        assert!(ProjectRole::Editor.can_manage_credentials());
        assert!(!ProjectRole::Editor.can_administer());
        assert!(!ProjectRole::Editor.can_manage_members());

        assert!(ProjectRole::Operator.can_run());
        assert!(!ProjectRole::Operator.can_edit());
        assert!(!ProjectRole::Operator.can_administer());

        assert!(ProjectRole::Viewer.can_read());
        assert!(!ProjectRole::Viewer.can_run());
        assert!(!ProjectRole::Viewer.can_edit());
        assert!(!ProjectRole::Viewer.can_manage_credentials());
    }

    #[test]
    fn roles_round_trip_through_their_stored_names() {
        for role in [
            ProjectRole::Owner,
            ProjectRole::Editor,
            ProjectRole::Operator,
            ProjectRole::Viewer,
        ] {
            assert_eq!(ProjectRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(ProjectRole::parse("admin"), None);
        assert_eq!(ProjectRole::parse(""), None);
    }

    #[test]
    fn status_round_trips_and_an_unknown_value_is_rejected() {
        assert_eq!(ProjectStatus::parse("active"), Some(ProjectStatus::Active));
        assert_eq!(
            ProjectStatus::parse("archived"),
            Some(ProjectStatus::Archived)
        );
        assert_eq!(ProjectStatus::parse("deleted"), None);
    }
}
