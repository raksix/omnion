//! Role bindings: attaching a role to a subject at a scope.
//!
//! People, groups and machine identities share one table, so a group membership grants exactly
//! the way a personal binding does: resolving a person's bindings loads their own rows **and**
//! the rows of every group they belong to. Which of those rows applies to a request is decided
//! by one matcher — [`Scope::applies_to`] — shared by the guard, the members tab and the
//! simulator, so a verdict can never come from two different readings of the same scope.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{PermissionsError, Result};
use crate::groups;
use crate::model::{NewBinding, NewSubjectBinding, ResourceContext, RoleBinding, Scope, Subject};
use crate::roles;

/// Column list for every binding query.
const BINDING_COLUMNS: &str = "id, role_id, subject_type, subject_id, user_id, scope_type, \
     organization_id, site_id, resource_type, resource_id, granted_by, expires_at, revoked_at, \
     created_at";

/// Row shape of `role_bindings`; the scope columns fold into [`Scope`], the subject columns into
/// [`Subject`].
#[derive(sqlx::FromRow)]
struct BindingRow {
    id: Uuid,
    role_id: Uuid,
    subject_type: String,
    subject_id: Uuid,
    user_id: Option<Uuid>,
    scope_type: String,
    organization_id: Option<Uuid>,
    site_id: Option<Uuid>,
    resource_type: Option<String>,
    resource_id: Option<String>,
    granted_by: Option<Uuid>,
    expires_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl BindingRow {
    fn into_binding(self) -> Result<RoleBinding> {
        let scope = Scope::from_parts(
            &self.scope_type,
            self.organization_id,
            self.site_id,
            self.resource_type.as_deref(),
            self.resource_id.as_deref(),
        )?;
        Ok(RoleBinding {
            id: self.id,
            role_id: self.role_id,
            subject: Subject::from_parts(&self.subject_type, self.subject_id)?,
            user_id: self.user_id,
            scope,
            granted_by: self.granted_by,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
        })
    }
}

/// Assign a role to a subject at a scope.
///
/// Fails with [`PermissionsError::AlreadyBound`] when the same role is already bound to the same
/// subject at the same scope — including through a temporary binding that has not expired yet.
/// Expired rows are retired first, so a temporary role can be granted again afterwards.
pub async fn grant_subject(pool: &PgPool, new: NewSubjectBinding) -> Result<RoleBinding> {
    retire_expired(pool, &new).await?;

    // `user_id` is filled only for personal bindings and stays beside the subject columns for one
    // release (expand-then-contract, 0016_iam_subjects.sql).
    let user_id = match new.subject {
        Subject::User(id) => Some(id),
        _ => None,
    };

    let sql = format!(
        "insert into role_bindings (role_id, user_id, subject_type, subject_id, scope_type, \
         organization_id, site_id, resource_type, resource_id, granted_by, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning {BINDING_COLUMNS}"
    );

    let row: BindingRow = sqlx::query_as(&sql)
        .bind(new.role_id)
        .bind(user_id)
        .bind(new.subject.subject_type())
        .bind(new.subject.id())
        .bind(new.scope.scope_type())
        .bind(new.scope.organization_id())
        .bind(new.scope.site_id())
        .bind(new.scope.resource_type())
        .bind(new.scope.resource_id())
        .bind(new.granted_by)
        .bind(new.expires_at)
        .fetch_one(pool)
        .await
        .map_err(map_binding_error)?;

    row.into_binding()
}

/// Assign a role to an account (the shape every pre-subject caller speaks).
pub async fn grant(pool: &PgPool, new: NewBinding) -> Result<RoleBinding> {
    grant_subject(
        pool,
        NewSubjectBinding {
            role_id: new.role_id,
            subject: Subject::User(new.user_id),
            scope: new.scope,
            granted_by: new.granted_by,
            expires_at: new.expires_at,
        },
    )
    .await
}

/// Mark the expired rows of one (role, subject, scope) combination as revoked.
///
/// The uniqueness index keeps working on `revoked_at is null`, so an expired temporary role
/// must be retired before the same combination can be granted again.
async fn retire_expired(pool: &PgPool, new: &NewSubjectBinding) -> Result<()> {
    sqlx::query(
        "update role_bindings set revoked_at = now() \
         where role_id = $1 and subject_type = $2 and subject_id = $3 and scope_type = $4 \
           and organization_id is not distinct from $5 \
           and site_id is not distinct from $6 \
           and resource_id is not distinct from $7 \
           and revoked_at is null \
           and expires_at is not null and expires_at <= now()",
    )
    .bind(new.role_id)
    .bind(new.subject.subject_type())
    .bind(new.subject.id())
    .bind(new.scope.scope_type())
    .bind(new.scope.organization_id())
    .bind(new.scope.site_id())
    .bind(new.scope.resource_id())
    .execute(pool)
    .await?;

    Ok(())
}

/// Assign a role unless the subject already holds it at that scope (the seed path).
pub async fn grant_if_missing(pool: &PgPool, new: NewBinding) -> Result<Option<RoleBinding>> {
    match grant(pool, new).await {
        Ok(binding) => Ok(Some(binding)),
        Err(PermissionsError::AlreadyBound) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Revoke a binding. Returns `true` when a live binding was revoked.
pub async fn revoke(pool: &PgPool, binding_id: Uuid) -> Result<bool> {
    let revoked = sqlx::query(
        "update role_bindings set revoked_at = now() where id = $1 and revoked_at is null",
    )
    .bind(binding_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(revoked)
}

/// Revoke every live binding a subject holds (deleting a group or a machine identity).
pub async fn revoke_for_subject(pool: &PgPool, subject: Subject) -> Result<u64> {
    let revoked = sqlx::query(
        "update role_bindings set revoked_at = now() \
         where subject_type = $1 and subject_id = $2 and revoked_at is null",
    )
    .bind(subject.subject_type())
    .bind(subject.id())
    .execute(pool)
    .await?
    .rows_affected();

    Ok(revoked)
}

/// Every binding of one subject, in any state (live ones first, newest first).
pub async fn bindings_of_subject(pool: &PgPool, subject: Subject) -> Result<Vec<RoleBinding>> {
    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings \
         where subject_type = $1 and subject_id = $2 \
         order by (revoked_at is null) desc, created_at desc, id desc"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql)
        .bind(subject.subject_type())
        .bind(subject.id())
        .fetch_all(pool)
        .await?;

    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// Every binding that speaks for a subject: their own rows, plus — for a person — the rows of
/// every group they belong to. All states are returned; callers filter with
/// [`RoleBinding::is_active_at`] and [`Scope::applies_to`].
pub async fn bindings_for(pool: &PgPool, subject: Subject) -> Result<Vec<RoleBinding>> {
    let mut bindings = bindings_of_subject(pool, subject).await?;

    if let Subject::User(user_id) = subject {
        let group_ids = groups::member_groups(pool, user_id).await?;
        if !group_ids.is_empty() {
            let sql = format!(
                "select {BINDING_COLUMNS} from role_bindings \
                 where subject_type = 'group' and subject_id = any($1) \
                 order by created_at desc, id desc"
            );
            let rows: Vec<BindingRow> = sqlx::query_as(&sql)
                .bind(&group_ids)
                .fetch_all(pool)
                .await?;
            for row in rows {
                bindings.push(row.into_binding()?);
            }
        }
    }

    bindings.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(bindings)
}

/// Live bindings of a subject that apply to `context` — the resolution the guard runs.
pub async fn active_for_context(
    pool: &PgPool,
    subject: Subject,
    context: &ResourceContext,
) -> Result<Vec<RoleBinding>> {
    let now = OffsetDateTime::now_utc();
    let bindings = bindings_for(pool, subject).await?;
    Ok(bindings
        .into_iter()
        .filter(|binding| binding.is_active_at(now) && binding.scope.applies_to(context))
        .collect())
}

/// Every binding of an account, live ones first (the account's own rows only).
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<RoleBinding>> {
    bindings_of_subject(pool, Subject::User(user_id)).await
}

/// How many live bindings carry a role (docs/07-IAM.md §20: the last Owner cannot be
/// removed).
pub async fn count_live_for_role(pool: &PgPool, role_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings \
         where role_id = $1 and revoked_at is null and (expires_at is null or expires_at > now())",
    )
    .bind(role_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Every binding carrying a role — live ones first, then the ones that ran out.
pub async fn list_for_role(pool: &PgPool, role_id: Uuid) -> Result<Vec<RoleBinding>> {
    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings where role_id = $1 \
         order by (revoked_at is null) desc, created_at desc, id desc"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql).bind(role_id).fetch_all(pool).await?;
    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// Filter of the binding list screen.
#[derive(Debug, Clone, Default)]
pub struct BindingFilter {
    /// Only bindings that belong to this organization (platform bindings always included).
    pub organization_id: Option<Uuid>,
    /// Only bindings of this subject.
    pub subject: Option<Subject>,
    /// Only bindings carrying this role.
    pub role_id: Option<Uuid>,
    /// Only bindings that apply right now.
    pub live_only: bool,
    /// Page size.
    pub limit: i64,
}

/// List bindings for the administration screen.
pub async fn list(pool: &PgPool, filter: &BindingFilter) -> Result<Vec<RoleBinding>> {
    let subject_type = filter.subject.map(Subject::subject_type);
    let subject_id = filter.subject.map(Subject::id);

    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings \
         where ($1::uuid is null or organization_id = $1 or scope_type = 'global') \
           and ($2::text is null or subject_type = $2) \
           and ($3::uuid is null or subject_id = $3) \
           and ($4::uuid is null or role_id = $4) \
           and ($5::boolean is false \
                or (revoked_at is null and (expires_at is null or expires_at > now()))) \
         order by (revoked_at is null and (expires_at is null or expires_at > now())) desc, \
                  created_at desc, id desc \
         limit $6"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql)
        .bind(filter.organization_id)
        .bind(subject_type)
        .bind(subject_id)
        .bind(filter.role_id)
        .bind(filter.live_only)
        .bind(filter.limit.clamp(1, 500))
        .fetch_all(pool)
        .await?;

    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// Live bindings that run out within the next `days` days — the overview's reminder list.
pub async fn expiring_within(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    days: i64,
) -> Result<Vec<RoleBinding>> {
    let sql = format!(
        "select {BINDING_COLUMNS} from role_bindings \
         where revoked_at is null and expires_at is not null \
           and expires_at > now() and expires_at <= now() + ($1 || ' days')::interval \
           and ($2::uuid is null or organization_id = $2 or scope_type = 'global') \
         order by expires_at asc, id asc"
    );

    let rows: Vec<BindingRow> = sqlx::query_as(&sql)
        .bind(days.clamp(1, 90))
        .bind(organization_id)
        .fetch_all(pool)
        .await?;

    rows.into_iter().map(BindingRow::into_binding).collect()
}

/// Validate a binding before it is written: the role must exist and may only be used inside
/// its own organization, the subject must exist, and a finer scope must name a target that
/// exists and belongs to the scope's organization.
pub async fn validate_subject(pool: &PgPool, new: &NewSubjectBinding) -> Result<()> {
    let role = roles::find_role(pool, new.role_id)
        .await?
        .ok_or(PermissionsError::RoleNotFound)?;

    if let (Some(role_org), Some(scope_org)) = (role.organization_id, new.scope.organization_id())
        && role_org != scope_org
    {
        return Err(PermissionsError::InvalidBinding(
            "the role belongs to another organization".to_owned(),
        ));
    }

    validate_scope_target(pool, &new.scope).await?;
    validate_subject_exists(pool, new.subject).await
}

/// Validate the target a scope names (site, department, module, resource).
pub async fn validate_scope_target(pool: &PgPool, scope: &Scope) -> Result<()> {
    match scope {
        Scope::Global | Scope::Organization { .. } => Ok(()),
        Scope::Site {
            organization_id,
            site_id,
        } => {
            let site_organization: Option<Uuid> =
                sqlx::query_scalar("select organization_id from sites where id = $1")
                    .bind(site_id)
                    .fetch_optional(pool)
                    .await?;
            match site_organization {
                None => Err(PermissionsError::InvalidBinding("unknown site".to_owned())),
                Some(site_organization)
                    if organization_id
                        .is_some_and(|organization_id| organization_id != site_organization) =>
                {
                    Err(PermissionsError::InvalidBinding(
                        "the site belongs to another organization".to_owned(),
                    ))
                }
                Some(_) => Ok(()),
            }
        }
        Scope::Department { department, .. } => {
            if department.trim().is_empty() {
                return Err(PermissionsError::InvalidBinding(
                    "a department scope needs a department name".to_owned(),
                ));
            }
            Ok(())
        }
        Scope::Module { module, .. } => {
            if module.trim().is_empty() {
                return Err(PermissionsError::InvalidBinding(
                    "a module scope needs a module name".to_owned(),
                ));
            }
            Ok(())
        }
        Scope::Resource { resource_id, .. } => {
            if resource_id.trim().is_empty() {
                return Err(PermissionsError::InvalidBinding(
                    "a resource scope needs a pattern".to_owned(),
                ));
            }
            Ok(())
        }
    }
}

/// `true` when the subject exists.
pub async fn validate_subject_exists(pool: &PgPool, subject: Subject) -> Result<()> {
    let exists: bool = match subject {
        Subject::User(id) => {
            sqlx::query_scalar("select exists (select 1 from users where id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?
        }
        Subject::Group(id) => {
            sqlx::query_scalar("select exists (select 1 from groups where id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?
        }
        Subject::ServiceAccount(id) => {
            sqlx::query_scalar("select exists (select 1 from service_accounts where id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?
        }
    };

    if exists {
        return Ok(());
    }

    Err(PermissionsError::InvalidBinding(match subject {
        Subject::User(_) => "unknown account".to_owned(),
        Subject::Group(_) => "unknown group".to_owned(),
        Subject::ServiceAccount(_) => "unknown service account".to_owned(),
    }))
}

/// Validate a personal binding (the pre-subject shape).
pub async fn validate(pool: &PgPool, new: &NewBinding) -> Result<()> {
    validate_subject(
        pool,
        &NewSubjectBinding {
            role_id: new.role_id,
            subject: Subject::User(new.user_id),
            scope: new.scope.clone(),
            granted_by: new.granted_by,
            expires_at: new.expires_at,
        },
    )
    .await
}

/// Mark every expired binding of every subject as revoked (the sweep the sign-in path runs).
///
/// Retiring the rows keeps the uniqueness index honest, so an expired temporary role can be
/// granted again, and the members tab still shows it — now as revoked rather than merely past
/// its window.
pub async fn retire_all_expired(pool: &PgPool) -> Result<u64> {
    let retired = sqlx::query(
        "update role_bindings set revoked_at = now() \
         where revoked_at is null and expires_at is not null and expires_at <= now()",
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(retired)
}

fn map_binding_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            PermissionsError::AlreadyBound
        }
        sqlx::Error::Database(ref db_err) if db_err.is_foreign_key_violation() => {
            PermissionsError::InvalidBinding("unknown role or subject".to_owned())
        }
        other => PermissionsError::Database(other),
    }
}
