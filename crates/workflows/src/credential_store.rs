//! The credential store: every read and write `/api/v1/credentials` performs (REQ-087, slice 2).
//!
//! Three things are load-bearing here, and each is a place the obvious implementation is
//! quietly wrong:
//!
//! 1. **Usage is derived from the graph, in this file, in one statement.** A node names a
//!    credential by key in its `params`, so "who uses this credential" is a
//!    `jsonb_array_elements(graph -> 'nodes')` probe — not a counter column. A counter is
//!    wrong the moment a person moves a node in the canvas, and a stale counter is worse than
//!    none: it makes the delete guard refuse forever, or lets a delete through that should not
//!    have been. Deriving it means the guard's answer is correct at the instant it is asked.
//! 2. **The delete guard is inside the same transaction as the delete.** Checking usage and
//!    then deleting in two round trips is a race a workflow save can win: the save lands, the
//!    delete lands, and the graph now points at nothing. `for update` on the credential row
//!    plus the usage count inside one transaction closes it, and the forced path is a separate
//!    function so a forced delete is always a deliberate call.
//! 3. **A secret is never written here.** There is no function in this module that takes a
//!    secret's plaintext. The only write that can attach one goes through
//!    [`attach_secret`], which stores an *opaque handle* the encrypted store gave us
//!    (REQ-125). The rule "no plaintext secret is stored in the workflows schema" is
//!    therefore not a review checklist item — there is no parameter a secret could arrive in.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::credentials::{
    CREDENTIAL_COLUMNS, Credential, CredentialUsage, CredentialUsageReport, Health, ListQuery,
    NewCredential, SCOPES, SHARINGS, Settings,
};
use crate::error::{Result, WorkflowError};
use crate::registry::CredentialDefinition;

/// Columns of `workflow_node_packages` for one `select`.
pub const PACKAGE_COLUMNS: &str = "id, organization_id, key, version, source, checksum, \
     permissions, enabled, installed_at, removed_at";

/// One installed node package.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct NodePackage {
    /// Package id.
    pub id: Uuid,
    /// Organization that installed it.
    pub organization_id: Uuid,
    /// Package key, e.g. `n8n-nodes-base`.
    pub key: String,
    /// The installed version.
    pub version: String,
    /// `bundled`, `marketplace` or `local`.
    pub source: String,
    /// Content checksum, as the installer computed it.
    pub checksum: String,
    /// The permissions the package asked for, as stored JSON.
    pub permissions: serde_json::Value,
    /// Whether its nodes are available.
    pub enabled: bool,
    /// When it was installed.
    pub installed_at: OffsetDateTime,
    /// When it was removed; `None` while it is live.
    pub removed_at: Option<OffsetDateTime>,
}

/// A package row to be written.
#[derive(Debug, Clone)]
pub struct NewNodePackage {
    /// Organization that installs it.
    pub organization_id: Uuid,
    /// Package key.
    pub key: String,
    /// Version being installed.
    pub version: String,
    /// `bundled`, `marketplace` or `local`.
    pub source: String,
    /// Content checksum.
    pub checksum: String,
    /// Requested permissions, as JSON.
    pub permissions: serde_json::Value,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Refuse a `scope` or `sharing` value the panel has no control for.
///
/// The panel's two selects and the migration's check constraints are the same two lists; this
/// is the third copy, and a value that reaches the database past it is a row the panel cannot
/// render a chip for.
pub fn check_vocabulary(scope: &str, sharing: &str) -> Result<()> {
    if !SCOPES.contains(&scope) {
        return Err(WorkflowError::CredentialScopeDenied {
            field: "scope",
            value: scope.to_string(),
            allowed: SCOPES.to_vec(),
        });
    }
    if !SHARINGS.contains(&sharing) {
        return Err(WorkflowError::CredentialScopeDenied {
            field: "sharing",
            value: sharing.to_string(),
            allowed: SHARINGS.to_vec(),
        });
    }
    Ok(())
}

/// Resolve a credential type from the registry, or refuse by name.
pub fn require_type(raw: &str) -> Result<&'static CredentialDefinition> {
    crate::registry::find_credential_type(raw.trim())
        .ok_or_else(|| WorkflowError::CredentialTypeUnknown(raw.trim().to_string()))
}

/// Whether a key is one a graph may name.
///
/// The rule is deliberately narrow — lowercase, digits, `_` and `-`, 1–64 characters, starting
/// with a letter or digit — because a key travels through JSON, through a URL and through an
/// export file (REQ-094). A key with a space or a quote in it is a key that will eventually be
/// one of the three places a workflow fails to load.
fn key_is_valid(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
}

// ---------------------------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------------------------

/// Insert a credential.
///
/// The settings object is rebuilt from the type's definition rather than stored as sent, so
/// the write is incapable of persisting a secret field even if a caller tried: by this point
/// the payload has already been through [`Settings::build`], which refuses one.
pub async fn insert_credential(pool: &PgPool, new: NewCredential) -> Result<Credential> {
    check_vocabulary(&new.scope, &new.sharing)?;
    require_type(&new.r#type)?;
    if !key_is_valid(&new.key) {
        return Err(WorkflowError::CredentialInvalid(format!(
            "{:?} is not a usable credential key — use 1–64 characters of a–z, 0–9, '_' or '-', \\
             starting with a letter or a digit",
            new.key
        )));
    }

    let sql = format!(
        "insert into workflow_credentials (organization_id, key, name, type, scope, sharing, \
         secret_ref, settings, owner_user_id, health, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'untested', $10) returning {CREDENTIAL_COLUMNS}"
    );
    let credential: Credential = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(&new.key)
        .bind(&new.name)
        .bind(&new.r#type)
        .bind(&new.scope)
        .bind(&new.sharing)
        .bind(&new.secret_ref)
        .bind(&new.settings)
        .bind(new.created_by)
        .bind(new.created_by)
        .fetch_one(pool)
        .await?;
    Ok(credential)
}

/// Read one credential, scoped to its organization.
pub async fn get_credential(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<Credential>> {
    let sql = format!(
        "select {CREDENTIAL_COLUMNS} from workflow_credentials \
         where organization_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Read one credential by the key a graph names.
pub async fn get_credential_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<Credential>> {
    let sql = format!(
        "select {CREDENTIAL_COLUMNS} from workflow_credentials \
         where organization_id = $1 and key = $2"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// List credentials, filtered.
pub async fn list_credentials(
    pool: &PgPool,
    organization_id: Uuid,
    query: &ListQuery,
) -> Result<Vec<Credential>> {
    let limit = query.limit.clamp(1, ListQuery::MAX_LIMIT);
    let sql = format!(
        "select {CREDENTIAL_COLUMNS} from workflow_credentials \
         where organization_id = $1 \
           and ($2::text is null or lower(name) like lower('%' || $2 || '%') \
                                 or lower(key) like lower('%' || $2 || '%')) \
           and ($3::text is null or type = $3) \
           and ($4::text is null or scope = $4) \
           and ($5::text is null or health = $5) \
           and ($6::text is null or sharing = $6) \
           and ($7::uuid is null or owner_user_id = $7) \
         order by name, id limit $8"
    );
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(search)
        .bind(query.r#type.as_deref())
        .bind(query.scope.as_deref())
        .bind(query.health.as_deref())
        .bind(query.sharing.as_deref())
        .bind(query.owner_user_id)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// The fields a `PATCH` may change.
///
/// An explicit struct rather than a `serde_json::Value`, because the one field that must never
/// be patchable — the secret — is then not a field here at all, and the compiler is what
/// guarantees it.
#[derive(Debug, Clone, Default)]
pub struct CredentialUpdate {
    /// New display name.
    pub name: Option<String>,
    /// New scope.
    pub scope: Option<String>,
    /// New sharing.
    pub sharing: Option<String>,
    /// New owner.
    pub owner_user_id: Option<Option<Uuid>>,
    /// Replacement non-secret settings; merged over the stored ones.
    pub settings: Option<Settings>,
}

/// Apply a partial update and return the row as it now stands.
///
/// Returns the row even when nothing changed: a `PATCH` that asks for the value it already has
/// is a success, and reporting a 409 for it teaches the panel that saving is unreliable.
pub async fn update_credential(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    update: CredentialUpdate,
) -> Result<Option<Credential>> {
    if let Some(scope) = update.scope.as_deref() {
        if !SCOPES.contains(&scope) {
            return Err(WorkflowError::CredentialScopeDenied {
                field: "scope",
                value: scope.to_string(),
                allowed: SCOPES.to_vec(),
            });
        }
    }
    if let Some(sharing) = update.sharing.as_deref() {
        if !SHARINGS.contains(&sharing) {
            return Err(WorkflowError::CredentialScopeDenied {
                field: "sharing",
                value: sharing.to_string(),
                allowed: SHARINGS.to_vec(),
            });
        }
    }

    let sql = format!(
        "update workflow_credentials set \
           name = coalesce($3, name), \
           scope = coalesce($4, scope), \
           sharing = coalesce($5, sharing), \
           owner_user_id = case when $6 then $7 else owner_user_id end, \
           settings = coalesce($8, settings), \
           updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(update.name)
        .bind(update.scope)
        .bind(update.sharing)
        .bind(update.owner_user_id.is_some())
        .bind(update.owner_user_id.flatten())
        .bind(update.settings.map(|s| s.value().clone()))
        .fetch_optional(pool)
        .await?)
}

/// Record the result of a test hook.
///
/// The health value is the hook's *outcome*, not the caller's opinion: there is no way to
/// write `ok` for a test that failed, which is what keeps the panel's chip from drifting away
/// from what a run would experience.
pub async fn record_test_outcome(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    outcome: &crate::credentials::TestOutcome,
) -> Result<Option<Credential>> {
    let sql = format!(
        "update workflow_credentials set health = $3, health_checked_at = now(), \
           health_detail = $4, updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(outcome.health.as_str())
        .bind(&outcome.detail)
        .fetch_optional(pool)
        .await?)
}

/// Attach a secret handle — the only write that can connect a credential to its secret.
///
/// The parameter is a *handle*, not a value: the encrypted store (REQ-125) hands back a ref
/// and keeps the payload. The health resets to `untested` because a credential whose secret
/// was just replaced has never been used in its new form, and claiming otherwise would show
/// the reader a green chip over a key nobody has called anything with.
pub async fn attach_secret(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    secret_ref: &str,
) -> Result<Option<Credential>> {
    let sql = format!(
        "update workflow_credentials set secret_ref = $3, health = 'untested', \
           health_checked_at = null, health_detail = null, oauth_expires_at = null, \
           updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(secret_ref)
        .fetch_optional(pool)
        .await?)
}

/// Note that a node resolved this credential.
pub async fn mark_used(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflow_credentials set last_used_at = now() \
                  where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a credential as needing a human: an OAuth refresh failed.
pub async fn mark_needs_reauth(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    reason: &str,
) -> Result<Option<Credential>> {
    let sql = format!(
        "update workflow_credentials set health = 'needs_reauth', health_detail = $3, \
           updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(reason)
        .fetch_optional(pool)
        .await?)
}

/// Mark a credential as disconnected: the token set is gone.
pub async fn mark_disconnected(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<Credential>> {
    let sql = format!(
        "update workflow_credentials set secret_ref = null, health = 'untested', \
           health_checked_at = null, health_detail = null, oauth_expires_at = null, \
           oauth_scopes = null, oauth_subject = null, updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Record who an OAuth flow connected as, and when the token expires.
#[allow(clippy::too_many_arguments)]
pub async fn mark_connected(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    secret_ref: &str,
    subject: Option<&str>,
    scopes: Option<&str>,
    expires_at: Option<OffsetDateTime>,
) -> Result<Option<Credential>> {
    let sql = format!(
        "update workflow_credentials set secret_ref = $3, oauth_subject = $4, \
           oauth_scopes = $5, oauth_expires_at = $6, health = 'ok', \
           health_checked_at = now(), health_detail = null, updated_at = now() \
         where organization_id = $1 and id = $2 returning {CREDENTIAL_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Credential>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(secret_ref)
        .bind(subject)
        .bind(scopes)
        .bind(expires_at)
        .fetch_optional(pool)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// Usage: derived, never stored
// ---------------------------------------------------------------------------------------------

/// The `jsonb_array_elements` expression a usage probe reads.
///
/// A workflow's nodes live in `graph` once the visual builder's migration (REQ-086 slice 1,
/// `0051_workflow_graph.sql`) has landed, and in `steps` before it. Both are probed and
/// coalesced, because the two live in *different branches* until someone merges them — and a
/// probe that names only `graph` is a `500` on every install that has not merged it yet, which
/// is exactly what the first run of `scripts/qa/credential-contract.sh` found.
///
/// The shape is the same in both: an array of nodes, each with `params`. `graph` is the one the
/// canvas edits and the one that carries the credential reference; `steps` is what the runner
/// executes, and it carries the same `params` because they are the same definition written
/// twice. Reading both and coalescing means the answer is right on either branch, and right on
/// the merged one where both exist.
/// `to_jsonb(w)` is the whole trick: it projects whatever columns the row *has*, so naming a
/// key that does not exist is a null at run time rather than a parse error. `w.graph` written
/// directly would be rejected by the planner before the query ever runs, on any install that
/// has not merged `0051_workflow_graph.sql` — and a probe that cannot be parsed cannot be
/// rescued by a `coalesce` around it.
const NODES_EXPR: &str = "coalesce(
        case when jsonb_typeof(to_jsonb(w) -> 'graph' -> 'nodes') = 'array'
             then to_jsonb(w) -> 'graph' -> 'nodes' end,
        case when jsonb_typeof(to_jsonb(w) -> 'steps') = 'array'
             then to_jsonb(w) -> 'steps' end,
        '[]'::jsonb)";

/// The usage statement, built once.
///
/// Both call sites run *this*: the public view and the guard inside the delete's transaction.
/// Two copies of a query is two queries that can disagree, and a guard that disagrees with the
/// screen that explains it is the worst pair on this surface — the reader is told a credential
/// is unused, presses delete, and the guard refuses for a reason neither of them can see.
const SQL_USAGE: &str = "select w.id as workflow_id, w.name as workflow_name, \
     coalesce(n->>'id', '') as node_id, \
     coalesce(nullif(n->>'label', ''), nullif(n->>'name', '')) as node_label, \
     coalesce(nullif(n->>'type', ''), nullif(n->>'action', '')) as node_type \
   from workflows w, lateral jsonb_array_elements(";

/// Every node in every workflow of this organization that names `key`.
///
/// The `coalesce` on each projected field matters: a graph written by the SQL backfill carries
/// `params` on every node, and a node without one is a node that references nothing rather
/// than a row that drops out of the result.
pub async fn usage(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<CredentialUsageReport> {
    let sql = format!(
        "{SQL_USAGE}{NODES_EXPR}) as n \
         where w.organization_id = $1 \
           and n->'params'->>'credential_key' = $2 \
         order by w.name, node_id"
    );
    let rows: Vec<CredentialUsage> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_all(pool)
        .await?;
    Ok(report_from(rows))
}

/// Build a report from raw references: distinct workflows, distinct node types, and whether
/// anything was found at all.
///
/// The two counts are counted differently on purpose — three references in two workflows is
/// *two* workflows for the delete guard and *three* references for the reader — and doing it
/// in one function means the two call sites cannot disagree about which is which.
fn report_from(rows: Vec<CredentialUsage>) -> CredentialUsageReport {
    let mut workflows: Vec<Uuid> = rows.iter().map(|r| r.workflow_id).collect();
    workflows.sort_unstable();
    workflows.dedup();
    let mut node_types: Vec<&str> = rows.iter().filter_map(|r| r.node_type.as_deref()).collect();
    node_types.sort_unstable();
    node_types.dedup();

    CredentialUsageReport {
        workflow_count: workflows.len(),
        node_type_count: node_types.len(),
        in_use: !rows.is_empty(),
        references: rows,
    }
}

/// The keys of every credential a set of workflows names, for the canvas's "missing
/// credential" state.
///
/// Distinct by construction: a graph may name the same key on four nodes, and the canvas wants
/// one warning per key, not four.
pub async fn referenced_keys(pool: &PgPool, organization_id: Uuid) -> Result<Vec<String>> {
    let sql = format!(
        "select distinct n->'params'->>'credential_key' as key \
         from workflows w, lateral jsonb_array_elements({NODES_EXPR}) as n \
         where w.organization_id = $1 and n->'params' ? 'credential_key' \
           and nullif(btrim(n->'params'->>'credential_key'), '') is not null \
         order by key"
    );
    let rows: Vec<(String,)> = sqlx::query_as(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(key,)| key).collect())
}

/// Delete a credential, refusing while anything still names it.
///
/// One transaction, and the credential row is locked for the duration: a workflow save that
/// lands between "counted the usage" and "deleted the row" would otherwise leave a graph
/// pointing at nothing, and no error anywhere. The usage count is taken inside the same
/// transaction for the same reason.
///
/// `force` is what the REQ's "forced delete disables and lists the dependents" needs, and it
/// is a parameter rather than a second function so the guard cannot be skipped by accident:
/// the report is returned either way, so a forced delete still tells the caller exactly what
/// it broke.
pub async fn delete_credential(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    force: bool,
) -> Result<DeleteOutcome> {
    let mut tx = pool.begin().await?;

    let key: Option<(String,)> = sqlx::query_as(
        "select key from workflow_credentials \
         where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((key,)) = key else {
        // A credential that is not there is not an error worth a 500: the caller's intent — it
        // should not exist — is already satisfied.
        tx.rollback().await?;
        return Ok(DeleteOutcome {
            deleted: false,
            report: CredentialUsageReport::default(),
        });
    };

    let report = usage_in(&mut tx, organization_id, &key).await?;
    if report.in_use && !force {
        tx.rollback().await?;
        return Err(WorkflowError::CredentialInUse {
            key,
            workflows: report.workflow_count,
        });
    }

    sqlx::query("delete from workflow_credentials where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(DeleteOutcome {
        deleted: true,
        report,
    })
}

/// What a delete did, and what it broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    /// Whether a row was actually removed.
    pub deleted: bool,
    /// The dependents, returned even on a forced delete so the caller can name them.
    pub report: CredentialUsageReport,
}

/// The usage probe over an open transaction, so the guard reads inside its own lock.
///
/// The same statement and the same `report_from` as the public probe: the guard inside the
/// transaction and the usage view outside it must agree, or a delete can be refused with a
/// count the reader then sees differently.
async fn usage_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: Uuid,
    key: &str,
) -> Result<CredentialUsageReport> {
    let sql = format!(
        "{SQL_USAGE}{NODES_EXPR}) as n \
         where w.organization_id = $1 \
           and n->'params'->>'credential_key' = $2 \
         order by w.name, node_id"
    );
    let rows: Vec<CredentialUsage> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_all(&mut **tx)
        .await?;
    Ok(report_from(rows))
}

// ---------------------------------------------------------------------------------------------
// Node packages
// ---------------------------------------------------------------------------------------------

/// Record an installation.
///
/// An update re-uses the same row: the partial unique index on `(organization_id, key)` makes a
/// second insert a constraint violation, and the REQ's "updates are equal-or-newer only" is the
/// installer's check to make — the ledger's job is to hold one row per key, not to re-derive
/// that rule and get it wrong.
pub async fn upsert_package(pool: &PgPool, new: NewNodePackage) -> Result<NodePackage> {
    let sql = format!(
        "insert into workflow_node_packages (organization_id, key, version, source, checksum, \
           permissions) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (organization_id, key) where removed_at is null \
         do update set version = excluded.version, source = excluded.source, \
                       checksum = excluded.checksum, permissions = excluded.permissions, \
                       enabled = true, installed_at = now() \
         returning {PACKAGE_COLUMNS}"
    );
    let package: NodePackage = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(&new.key)
        .bind(&new.version)
        .bind(&new.source)
        .bind(&new.checksum)
        .bind(new.permissions)
        .fetch_one(pool)
        .await?;
    Ok(package)
}

/// List live packages, newest first.
pub async fn list_packages(pool: &PgPool, organization_id: Uuid) -> Result<Vec<NodePackage>> {
    let sql = format!(
        "select {PACKAGE_COLUMNS} from workflow_node_packages \
         where organization_id = $1 and removed_at is null order by installed_at desc, key"
    );
    Ok(sqlx::query_as::<_, NodePackage>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// Enable or disable one package.
///
/// A *disable* is the REQ's "removal disables them and flags dependent workflows instead of
/// breaking them": the nodes stop being available while every workflow that names one keeps
/// loading. Disabling never touches a workflow.
pub async fn set_package_enabled(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
    enabled: bool,
) -> Result<Option<NodePackage>> {
    let sql = format!(
        "update workflow_node_packages set enabled = $3, installed_at = now() \
         where organization_id = $1 and key = $2 and removed_at is null \
         returning {PACKAGE_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, NodePackage>(&sql)
        .bind(organization_id)
        .bind(key)
        .bind(enabled)
        .fetch_optional(pool)
        .await?)
}

/// Remove a package by marking it removed, so the ledger keeps the history.
pub async fn remove_package(pool: &PgPool, organization_id: Uuid, key: &str) -> Result<bool> {
    let result = sqlx::query(
        "update workflow_node_packages set removed_at = now(), enabled = false \
         where organization_id = $1 and key = $2 and removed_at is null",
    )
    .bind(organization_id)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Count the credentials whose recorded health is one of `values`.
///
/// Read by the panel's summary line, and a single statement so the chips above a list and the
/// count beside it cannot disagree.
pub async fn count_by_health(
    pool: &PgPool,
    organization_id: Uuid,
    values: &[Health],
) -> Result<Vec<(String, i64)>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "select health, count(*)::bigint as count from workflow_credentials \
         where organization_id = $1 and health = any($2) group by health order by health"
    );
    let wanted: Vec<String> = values.iter().map(|h| h.as_str().to_string()).collect();
    Ok(sqlx::query_as::<_, (String, i64)>(&sql)
        .bind(organization_id)
        .bind(&wanted)
        .fetch_all(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usable_key_is_a_credential_key() {
        for key in ["a", "stripe-prod", "s3_vault", "x9"] {
            assert!(key_is_valid(key), "{key} should be usable");
        }
    }

    #[test]
    fn an_unusable_key_is_refused_before_the_database_sees_it() {
        // Every one of these would survive a plain `length > 0` check and then break a graph,
        // an export or a URL later.
        for key in [
            "",
            "Stripe",
            "with space",
            "quote\"key",
            "-leading-dash",
            "_leading_underscore",
            "trailing\nnewline",
            &"x".repeat(65),
        ] {
            assert!(!key_is_valid(key), "{key:?} must be refused");
        }
        assert!(
            key_is_valid(&"x".repeat(64)),
            "64 characters is the ceiling, not a refusal"
        );
    }

    #[test]
    fn the_vocabulary_refusal_names_the_legal_values() {
        let error = check_vocabulary("galaxy", "private").expect_err("a scope that is not one");
        match error {
            WorkflowError::CredentialScopeDenied { field, allowed, .. } => {
                assert_eq!(field, "scope");
                assert_eq!(allowed, SCOPES);
            }
            other => panic!("expected a scope refusal, got {other:?}"),
        }
        let error =
            check_vocabulary("organization", "public").expect_err("a sharing that is not one");
        match error {
            WorkflowError::CredentialScopeDenied { field, allowed, .. } => {
                assert_eq!(field, "sharing");
                assert_eq!(allowed, SHARINGS);
            }
            other => panic!("expected a sharing refusal, got {other:?}"),
        }
        assert!(check_vocabulary("project", "organization").is_ok());
    }

    #[test]
    fn an_unknown_type_is_refused_by_its_own_code() {
        let error = require_type("carrier_pigeon").expect_err("not a registry type");
        assert_eq!(error.code(), "credential_type_unknown");
        assert!(require_type("oauth2").is_ok());
    }

    #[test]
    fn the_column_list_names_only_the_columns_the_row_has() {
        // The `select` list and the struct are kept in step by hand, so a column added to the
        // migration without being added here is caught here rather than as a runtime "missing
        // field" on every read.
        let columns: Vec<&str> = CREDENTIAL_COLUMNS
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        assert_eq!(columns.len(), 20, "the column list drifted from the row");
        for required in [
            "id",
            "organization_id",
            "key",
            "type",
            "secret_ref",
            "settings",
            "health",
        ] {
            assert!(columns.contains(&required), "{required} must be selected");
        }
        for forbidden in ["api_key", "token", "password", "secret_value"] {
            assert!(
                !columns.contains(&forbidden),
                "{forbidden} must never be a column of this table"
            );
        }
    }

    #[test]
    fn the_usage_probe_reads_the_graph_or_the_steps_and_never_a_missing_column() {
        // `graph` arrives with the visual builder's migration (REQ-086 slice 1,
        // `0051_workflow_graph.sql`) and `steps` is what every install has today. The two live
        // in different branches, so a probe naming only `graph` is a `500` on any install that
        // has not merged it — which is what the first run of `scripts/qa/credential-contract.sh`
        // found, as a 500 on `/usage` and on the delete that guards with it.
        assert!(
            NODES_EXPR.contains("-> 'graph' -> 'nodes'"),
            "the builder's representation is probed"
        );
        assert!(
            NODES_EXPR.contains("-> 'steps'"),
            "the runner's representation is probed too"
        );
        // The direct form would not parse at all on a branch without the builder's migration,
        // so the projection goes through `to_jsonb` — a missing key is null, not a syntax
        // error. This assertion is the difference between the two being caught at build time
        // and being caught as a 500 in production.
        assert!(
            NODES_EXPR.contains("to_jsonb(w)"),
            "the probe projects the row, so a column that does not exist yet is not a parse error"
        );
        assert!(
            !NODES_EXPR.contains("w.graph"),
            "a bare `w.graph` reference is rejected by the planner on any install without 0051"
        );
        assert!(
            NODES_EXPR.trim_start().starts_with("coalesce("),
            "and the two are coalesced, so neither branch is a hard dependency"
        );
        // The two representations disagree about names, and the usage view has to render
        // whichever it is given. A `steps` node calls its name `name` and its type `action`; a
        // `graph` node calls them `label` and `type`. Reading only the graph's spelling makes
        // every usage row on a pre-builder install say nothing, which is what the first run of
        // `scripts/qa/delete-guard.sh` found — the guard fired correctly and the row was blank.
        assert!(
            SQL_USAGE.contains("nullif(n->>'name', '')")
                && SQL_USAGE.contains("nullif(n->>'action', '')"),
            "a steps-era node's `name`/`action` are read as well as a graph node's `label`/`type`"
        );
        // A probe with no fallback would be one `500` away from a silently empty usage view,
        // which reads as "nothing uses this" and lets the guard delete a live credential.
        assert!(
            NODES_EXPR.contains("'[]'::jsonb"),
            "a workflow with neither representation contributes no nodes rather than failing"
        );
    }

    #[test]
    fn the_report_counts_workflows_and_references_differently_on_purpose() {
        // Three references in two workflows is *two* workflows for the delete guard and
        // *three* references for the reader. Getting this wrong in either direction is a real
        // bug: undercount the workflows and the guard lets a live credential go.
        let rows = vec![
            CredentialUsage {
                workflow_id: Uuid::from_u128(1),
                workflow_name: "Nightly".into(),
                node_id: "n1".into(),
                node_label: None,
                node_type: Some("http_request".into()),
            },
            CredentialUsage {
                workflow_id: Uuid::from_u128(1),
                workflow_name: "Nightly".into(),
                node_id: "n2".into(),
                node_label: None,
                node_type: Some("http_request".into()),
            },
            CredentialUsage {
                workflow_id: Uuid::from_u128(2),
                workflow_name: "Signup".into(),
                node_id: "n1".into(),
                node_label: None,
                node_type: Some("send_email".into()),
            },
        ];
        let report = report_from(rows);
        assert_eq!(report.references.len(), 3, "three references");
        assert_eq!(report.workflow_count, 2, "two workflows");
        assert_eq!(
            report.node_type_count, 2,
            "two node types, the repeat collapsed"
        );
        assert!(report.in_use);
    }

    #[test]
    fn an_empty_probe_is_not_in_use() {
        // The other direction of the same guard: a credential nothing names must be
        // deletable, or the guard refuses forever and the reader concludes the button is
        // broken.
        let report = report_from(Vec::new());
        assert!(!report.in_use);
        assert_eq!(report.workflow_count, 0);
        assert_eq!(report.references.len(), 0);
    }

    #[test]
    fn the_usage_query_derives_from_the_graph_and_never_a_counter() {
        // A credential used by two nodes in one workflow is one workflow and two references —
        // the report is the thing the delete guard reads, so its two counts are counted
        // differently on purpose. This pins the arithmetic the SQL has to reproduce.
        let rows = vec![
            CredentialUsage {
                workflow_id: Uuid::from_u128(1),
                workflow_name: "Nightly sync".into(),
                node_id: "n1".into(),
                node_label: Some("Call Stripe".into()),
                node_type: Some("http_request".into()),
            },
            CredentialUsage {
                workflow_id: Uuid::from_u128(1),
                workflow_name: "Nightly sync".into(),
                node_id: "n2".into(),
                node_label: None,
                node_type: Some("http_request".into()),
            },
            CredentialUsage {
                workflow_id: Uuid::from_u128(2),
                workflow_name: "Signup".into(),
                node_id: "n1".into(),
                node_label: Some("Welcome mail".into()),
                node_type: Some("send_email".into()),
            },
        ];
        let mut workflows: Vec<Uuid> = rows.iter().map(|r| r.workflow_id).collect();
        workflows.sort_unstable();
        workflows.dedup();
        let mut node_types: Vec<&str> =
            rows.iter().filter_map(|r| r.node_type.as_deref()).collect();
        node_types.sort_unstable();
        node_types.dedup();
        assert_eq!(workflows.len(), 2, "two workflows, three references");
        assert_eq!(node_types, vec!["http_request", "send_email"]);
    }
}
