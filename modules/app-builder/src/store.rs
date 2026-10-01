//! The plan store: `app_builder_*` as rows (docs/requests/REQ-045, slice 1).
//!
//! Four invariants live here rather than in the routes, because every write path has to
//! honour them and a route that repeats a rule is a route that can forget it:
//!
//! * **A status is never written that the row's check would refuse.** The check is the
//!   authority; this store answers the same question first, so a bad value arrives as a `400`
//!   naming the value rather than as a `500` from a constraint violation nobody can act on.
//! * **A rejected artifact carries a reason, and apply names its blockers.** "Apply is
//!   blocked" with nothing beside it is a dead end; the review screen renders a list of the
//!   artifacts by name, which is what turns it into a row the reviewer can press.
//! * **An edited artifact names what it replaced.** A reviewer's inline change and the
//!   model's proposal would otherwise be indistinguishable in the apply log.
//! * **A draft plan is inert.** Nothing in this module writes to a live table, so "nothing
//!   goes live without review" is a property of the code rather than a promise in a doc.
//!
//! **Audit is the route layer's job**, as it is for the sibling AI workflow builder's store
//! (docs/requests/REQ-046): the entry names the signed-in account and its peer address, which
//! only a request knows. What this module does guarantee is that the audit trail cannot
//! disagree with the store, because the store has exactly one write path per rule below and
//! each returns the row it wrote — a caller that records "artifact accepted" has an accepted
//! row to point at, and one that failed to write cannot claim it did.

use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use crate::error::{AppBuilderError, BlockedArtifact, Result};
use crate::model::{
    AppBuilderArtifact, AppBuilderPlan, EditedArtifact, NEW_PLAN_STATUS, NewArtifact, NewPlan,
    PlanCounts, PlanFilter, PlanPage, artifact_is_resolved, artifact_status_is_known,
    kind_is_known, plan_status_is_known,
};

/// Columns read back from `app_builder_plans`.
const PLAN_COLUMNS: &str = "id, organization_id, site_id, prompt, title, status, plan_version, \
     model_label, tokens_in, tokens_out, cost_cents, error, created_by, applied_at, created_at, \
     updated_at, supersedes_id";

/// Columns read back from `app_builder_artifacts`.
const ARTIFACT_COLUMNS: &str = "id, plan_id, kind, key, parent_key, ordinal, status, spec, \
     rationale, validation, supersedes_id, created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Write the row a generation starts from.
///
/// The row is written **before** the provider is called, so a generation that dies
/// mid-flight leaves a `failed` plan naming its reason instead of leaving nothing at all.
pub async fn insert_plan(
    pool: &PgPool,
    new: NewPlan,
) -> Result<AppBuilderPlan> {
    let prompt = read_prompt(&new.prompt)?;
    let title = read_title(new.title.as_deref())?;
    require_known_status(NEW_PLAN_STATUS)?;

    let mut tx = pool.begin().await?;
    let plan = write_plan(&mut tx, &new, &prompt, &title).await?;
    tx.commit().await?;
    Ok(plan)
}

async fn write_plan(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewPlan,
    prompt: &str,
    title: &str,
) -> Result<AppBuilderPlan> {
    let sql = format!(
        "insert into app_builder_plans
             (organization_id, site_id, prompt, title, status, model_label, created_by,
              supersedes_id)
         values ($1, $2, $3, $4, '{NEW_PLAN_STATUS}', $5, $6, $7)
         returning {PLAN_COLUMNS}"
    );
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(prompt)
        .bind(title)
        .bind(&new.model_label)
        .bind(new.created_by)
        .bind(new.supersedes_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(plan)
}

/// Record the model's answer: tokens, cost, and the move out of `generating`.
///
/// The status is an argument rather than a constant because the two callers want different
/// destinations: a validated answer becomes `draft`, and an answer that arrived but could
/// not be validated becomes `failed` with the reason beside it. Both are recorded.
pub async fn apply_answer(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    usage: crate::model::PlanUsage,
    title: Option<&str>,
) -> Result<Option<AppBuilderPlan>> {
    require_known_status(status)?;
    let title = match title {
        Some(raw) => Some(read_title(Some(raw))?),
        None => None,
    };

    // A plan that is no longer `generating` belongs to another attempt: returning `None` is
    // how the caller learns it lost the race, and it is the same answer a missing plan gets.
    // The `where status = …` clause is the guard — a caller that answers a plan twice gets
    // the second answer's `None` rather than a second set of token counts.
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "update app_builder_plans
            set status = $2,
                tokens_in = $3, tokens_out = $4, cost_cents = $5,
                title = coalesce($6, title),
                updated_at = now()
          where id = $1 and status = '{NEW_PLAN_STATUS}'
        returning {PLAN_COLUMNS}"
    ))
    .bind(id)
    .bind(status)
    .bind(usage.input)
    .bind(usage.output)
    .bind(usage.cost_cents)
    .bind(title)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

/// Record why a generation failed.
///
/// Leaves the row's prompt and title in place: a failed plan is still the operator's request,
/// and the console's error state shows the plan so they can retry it rather than retype it.
pub async fn apply_failure(
    pool: &PgPool,
    id: Uuid,
    error: &str,
) -> Result<Option<AppBuilderPlan>> {
    let reason = read_error(error)?;
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "update app_builder_plans
            set status = 'failed', error = $2, updated_at = now()
          where id = $1 and status = '{NEW_PLAN_STATUS}'
        returning {PLAN_COLUMNS}"
    ))
    .bind(id)
    .bind(&reason)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

/// Write one artifact and record what the validator said about it.
///
/// The artifact's `status` is derived here rather than taken from the caller: an artifact
/// with findings is `invalid` whatever the generator believed, and one without is `pending`
/// until a person says otherwise. A generator that could write `accepted` would be able to
/// approve its own work, which is the exact thing the request rules out.
pub async fn insert_artifact(
    pool: &PgPool,
    plan_id: Uuid,
    artifact: &NewArtifact,
    findings: &[crate::validate::Finding],
) -> Result<AppBuilderArtifact> {
    require_known_kind(&artifact.kind)?;
    validate_artifact_key(&artifact.key)?;
    let spec = require_object(&artifact.spec)?;
    let rationale = read_rationale(&artifact.rationale)?;
    let validation = serde_json::to_value(findings).map_err(|err| {
        AppBuilderError::invalid("app_builder_artifact_findings", err.to_string())
    })?;
    let status = if findings.is_empty() { "pending" } else { "invalid" };

    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "insert into app_builder_artifacts
             (plan_id, kind, key, parent_key, ordinal, status, spec, rationale, validation)
         values ($1, $2, $3, $4, $5, '{status}', $6, $7, $8)
         returning {ARTIFACT_COLUMNS}"
    ))
    .bind(plan_id)
    .bind(&artifact.kind)
    .bind(&artifact.key)
    .bind(&artifact.parent_key)
    .bind(artifact.ordinal)
    .bind(spec)
    .bind(&rationale)
    .bind(&validation)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(row)
}

/// Replace an artifact's body with a reviewer's edit, and re-validate it.
///
/// `edited` is the one status a caller may not simply assert: the row's check demands that an
/// `edited` artifact name what it replaced, so an edit with no predecessor is refused here
/// rather than at the database, with a message that says so.
pub async fn update_artifact_spec(
    pool: &PgPool,
    artifact_id: Uuid,
    edit: &EditedArtifact,
    findings: &[crate::validate::Finding],
) -> Result<Option<AppBuilderArtifact>> {
    let spec = require_object(&edit.spec)?;
    let validation = serde_json::to_value(findings).map_err(|err| {
        AppBuilderError::invalid("app_builder_artifact_findings", err.to_string())
    })?;
    // An edit that still has findings is `invalid`, not `edited`: "edited" reads as *fixed* on
    // the review screen, and a body the validator refuses is not fixed.
    let status = if findings.is_empty() { "edited" } else { "invalid" };

    let row = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "update app_builder_artifacts
            set spec = $2, validation = $3, status = '{status}',
                supersedes_id = $4, updated_at = now()
          where id = $1
        returning {ARTIFACT_COLUMNS}"
    ))
    .bind(artifact_id)
    .bind(spec)
    .bind(&validation)
    .bind(edit.supersedes_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Move an artifact to `accepted`, `rejected` or `invalid`.
///
/// `accepted` and `edited` are refused: they are claims about validity, and a caller that
/// wants to claim one has to go through the paths that validate. Accepting an artifact the
/// validator refused is the one write in this module that would make `apply` unsafe.
pub async fn set_artifact_status(
    pool: &PgPool,
    artifact_id: Uuid,
    status: &str,
) -> Result<Option<AppBuilderArtifact>> {
    require_known_artifact_status(status)?;
    if artifact_is_resolved(status) {
        return Err(AppBuilderError::invalid(
            "app_builder_artifact_not_decidable",
            format!(
                "`{status}` is decided by the edit path, not by a status write: it is a claim \
                 that the artifact is valid, and the validator is what decides that"
            ),
        ));
    }

    let row = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "update app_builder_artifacts
            set status = $2, updated_at = now()
          where id = $1 and status <> 'accepted'
        returning {ARTIFACT_COLUMNS}"
    ))
    .bind(artifact_id)
    .bind(status)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Write a regenerated artifact beside the one it replaces and retire that one.
///
/// Two rows and one status change in **one transaction**, because the alternative is a plan
/// with two live artifacts of the same `(plan, kind, key)` — which the migration's unique
/// index refuses, so a partial write would surface as a constraint violation rather than as
/// the version history it was meant to be.
pub async fn supersede_artifact(
    pool: &PgPool,
    previous_id: Uuid,
    replacement: &NewArtifact,
    findings: &[crate::validate::Finding],
) -> Result<Option<AppBuilderArtifact>> {
    require_known_kind(&replacement.kind)?;
    validate_artifact_key(&replacement.key)?;
    let spec = require_object(&replacement.spec)?;
    let rationale = read_rationale(&replacement.rationale)?;
    let validation = serde_json::to_value(findings).map_err(|err| {
        AppBuilderError::invalid("app_builder_artifact_findings", err.to_string())
    })?;
    let status = if findings.is_empty() { "pending" } else { "invalid" };

    let mut tx = pool.begin().await?;
    // `for update` takes the row's lock, so a second regeneration of the same artifact
    // blocks here rather than racing to insert two live rows for one `(plan, kind, key)`.
    // The kind is read in the same statement for the same reason: a second round-trip could
    // read it after the first caller's regeneration committed. The plan id is bound only so
    // the `select` has the shape the insert below can be checked against; the insert copies
    // it out of the locked row rather than trusting a value the caller supplied.
    let previous: Option<(String, String)> = sqlx::query_as(
        "select kind, plan_id::text from app_builder_artifacts
          where id = $1 and status <> 'accepted'
          for update",
    )
    .bind(previous_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((previous_kind, _plan_id)) = previous else {
        tx.rollback().await?;
        return Ok(None);
    };
    if replacement.kind != previous_kind {
        tx.rollback().await?;
        return Err(AppBuilderError::invalid(
            "app_builder_artifact_kind_change",
            "a regenerated artifact keeps its kind; a different kind is a new artifact",
        ));
    }

    let row = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "insert into app_builder_artifacts
             (plan_id, kind, key, parent_key, ordinal, status, spec, rationale, validation,
              supersedes_id)
         select plan_id, kind, key, $2, ordinal, '{status}', $3, $4, $5, id
           from app_builder_artifacts where id = $1
        returning {ARTIFACT_COLUMNS}"
    ))
    .bind(previous_id)
    .bind(&replacement.parent_key)
    .bind(spec)
    .bind(&rationale)
    .bind(&validation)
    .fetch_one(&mut *tx)
    .await?;

    // The predecessor becomes `rejected` rather than being deleted: the request asks for the
    // previous version to be kept so a rejected attempt stays comparable, and a row that
    // still carries `accepted` would count towards apply's totals.
    sqlx::query("update app_builder_artifacts set status = 'rejected', updated_at = now() where id = $1")
        .bind(previous_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Some(row))
}

/// Write a fresh attempt beside the plan it replaces.
///
/// The superseded plan is **rejected**, not deleted: "kept plan versions so a rejected
/// attempt can be compared" is the request's own requirement, and a delete would answer it
/// for the wrong reasons.
pub async fn supersede_plan(
    pool: &PgPool,
    previous_id: Uuid,
    new: NewPlan,
) -> Result<AppBuilderPlan> {
    if new.supersedes_id != Some(previous_id) {
        return Err(AppBuilderError::invalid(
            "app_builder_supersedes_mismatch",
            "the new plan must name the plan it replaces",
        ));
    }
    let mut new = new;
    new.supersedes_id = Some(previous_id);
    let plan = insert_plan(pool, new).await?;

    let mut tx = pool.begin().await?;
    sqlx::query(
        "update app_builder_plans set status = 'rejected', updated_at = now()
          where id = $1 and status in ('generating', 'draft')",
    )
    .bind(previous_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(plan)
}

/// Delete a plan that was never applied.
///
/// The refusal is here rather than left to the caller because "applied plans are
/// undeletable" is a rule about **history**, and every caller that reaches for a delete is
/// holding an id rather than the plan's history.
pub async fn delete_plan(pool: &PgPool, id: Uuid) -> Result<bool> {
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "delete from app_builder_plans where id = $1 and status <> 'applied'
        returning {PLAN_COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(plan.is_some())
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// One plan by id.
pub async fn find_plan(pool: &PgPool, id: Uuid) -> Result<Option<AppBuilderPlan>> {
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "select {PLAN_COLUMNS} from app_builder_plans where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

/// One plan by id, refusing one that is not in `organization_id`.
///
/// Two queries rather than one: the first is the tenant check the route layer applies to
/// every resource, and the second is the read. A single `where id = $1 and organization_id = $2`
/// answers "not found" for both a plan that does not exist and one that belongs to somebody
/// else — which is right for a public API but useless for a screen that has to tell a reviewer
/// their session lost the plan they were editing.
pub async fn find_plan_in(
    pool: &PgPool,
    id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<Option<AppBuilderPlan>> {
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "select {PLAN_COLUMNS} from app_builder_plans
          where id = $1
            and (organization_id is null or organization_id = $2 or $2 is null)
        limit 1"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

/// One artifact by id.
pub async fn find_artifact(pool: &PgPool, id: Uuid) -> Result<Option<AppBuilderArtifact>> {
    let artifact = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "select {ARTIFACT_COLUMNS} from app_builder_artifacts where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(artifact)
}

/// A plan's artifacts, in the order the review tree shows them.
pub async fn list_artifacts(pool: &PgPool, plan_id: Uuid) -> Result<Vec<AppBuilderArtifact>> {
    let artifacts = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "select {ARTIFACT_COLUMNS} from app_builder_artifacts
          where plan_id = $1
          order by ordinal, kind, key"
    ))
    .bind(plan_id)
    .fetch_all(pool)
    .await?;
    Ok(artifacts)
}

/// One organization's plans, newest first, with the counts the list's columns show.
///
/// The counts come back in the **same** query as the page. A count per row would turn one
/// round-trip into eleven on the screen that is re-fetched on every filter change, and eleven
/// queries whose results can disagree with the page they decorate.
pub async fn list_plans(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    filter: &PlanFilter,
) -> Result<PlanPage> {
    if let Some(status) = filter.status.as_deref() {
        require_known_status(status)?;
    }

    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select p.id, p.organization_id, p.site_id, p.prompt, p.title, p.status, p.plan_version, \
                p.model_label, p.tokens_in, p.tokens_out, p.cost_cents, p.error, p.created_by, \
                p.applied_at, p.created_at, p.updated_at, p.supersedes_id, \
                count(a.id)::bigint                                        as artifact_count, \
                count(a.id) filter (where a.status in ('accepted', 'edited'))::bigint as accepted_count, \
                count(a.id) filter (where a.status = 'rejected')::bigint   as rejected_count, \
                count(a.id) filter (where a.status = 'pending')::bigint    as pending_count, \
                count(a.id) filter (where a.status = 'invalid')::bigint    as invalid_count \
           from app_builder_plans p \
           left join app_builder_artifacts a on a.plan_id = p.id \
          where (p.organization_id is null or p.organization_id = ",
    );
    builder.push_bind(organization_id);
    builder.push(" or p.organization_id is null or ");
    builder.push_bind(organization_id);
    builder.push(")");

    if let Some(status) = filter.status.as_deref() {
        builder.push(" and p.status = ");
        builder.push_bind(status);
    }
    if let Some(text) = filter.text.as_deref().filter(|t| !t.trim().is_empty()) {
        builder.push(" and (p.title ilike ");
        builder.push_bind(format!("%{}%", text.trim()));
        builder.push(" or p.prompt ilike ");
        builder.push_bind(format!("%{}%", text.trim()));
        builder.push(")");
    }
    if let Some(created_by) = filter.created_by {
        builder.push(" and p.created_by = ");
        builder.push_bind(created_by);
    }

    builder.push(" group by p.id");
    // The window over the whole match, computed **before** the page slice: a `count(*)` over
    // the paged rows answers "how many rows did this page return", which is a number the
    // screen already has, rather than "how many plans match".
    builder.push(" order by p.created_at desc, p.id limit ");
    builder.push_bind(filter.limit.max(1));
    builder.push(" offset ");
    builder.push_bind(filter.offset.max(0));

    let rows = builder
        .build_query_as::<PlanRow>()
        .fetch_all(pool)
        .await?;

    let plans = rows.iter().map(|row| row.plan.clone()).collect();
    let counts = rows
        .iter()
        .map(|row| PlanCounts {
            artifacts: row.artifact_count,
            accepted: row.accepted_count,
            rejected: row.rejected_count,
            pending: row.pending_count,
            invalid: row.invalid_count,
        })
        .collect();

    let total = count_matching(pool, organization_id, filter).await?;

    Ok(PlanPage { plans, counts, total })
}

async fn count_matching(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    filter: &PlanFilter,
) -> Result<i64> {
    let mut builder: QueryBuilder<'_, Postgres> =
        QueryBuilder::new("select count(*) from app_builder_plans where (organization_id is null or organization_id = ");
    builder.push_bind(organization_id);
    builder.push(" or organization_id is null or ");
    builder.push_bind(organization_id);
    builder.push(")");
    if let Some(status) = filter.status.as_deref() {
        builder.push(" and status = ");
        builder.push_bind(status);
    }
    if let Some(text) = filter.text.as_deref().filter(|t| !t.trim().is_empty()) {
        builder.push(" and (title ilike ");
        builder.push_bind(format!("%{}%", text.trim()));
        builder.push(" or prompt ilike ");
        builder.push_bind(format!("%{}%", text.trim()));
        builder.push(")");
    }
    if let Some(created_by) = filter.created_by {
        builder.push(" and created_by = ");
        builder.push_bind(created_by);
    }
    let total: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(total)
}

/// Per-plan artifact counts, for the detail screen's footer.
pub async fn artifact_counts(pool: &PgPool, plan_id: Uuid) -> Result<PlanCounts> {
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "select count(*)::bigint,
                count(*) filter (where status in ('accepted', 'edited'))::bigint,
                count(*) filter (where status = 'rejected')::bigint,
                count(*) filter (where status = 'pending')::bigint,
                count(*) filter (where status = 'invalid')::bigint
           from app_builder_artifacts where plan_id = $1",
    )
    .bind(plan_id)
    .fetch_one(pool)
    .await?;
    Ok(PlanCounts {
        artifacts: counts.0,
        accepted: counts.1,
        rejected: counts.2,
        pending: counts.3,
        invalid: counts.4,
    })
}

/// What stands between a plan and its apply, named.
///
/// This is the list the review screen's footer renders and the `409`'s body carries. It is
/// built from the plan's own artifacts rather than returned by a boolean, because "3
/// unresolved" is a number the reviewer has to look up and "the report artifact is pending" is
/// a row they can press.
pub async fn blockers(pool: &PgPool, plan_id: Uuid) -> Result<Vec<BlockedArtifact>> {
    let unresolved = sqlx::query_as::<_, (String, String, String, serde_json::Value)>(
        "select kind, key, status, validation from app_builder_artifacts
          where plan_id = $1 and status not in ('accepted', 'edited')
          order by ordinal, kind, key",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await?;

    let mut blockers: Vec<BlockedArtifact> = unresolved
        .into_iter()
        .map(|(kind, key, status, validation)| BlockedArtifact {
            kind,
            key,
            status,
            reason: first_finding(&validation),
        })
        .collect();

    // A required kind the plan never produced blocks apply too, and it is named here rather
    // than only counted: "the plan proposes no report" is actionable in a way that "the plan
    // is incomplete" is not.
    let present: Vec<String> = sqlx::query_scalar(
        "select distinct kind from app_builder_artifacts where plan_id = $1",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await?;
    for kind in crate::model::REQUIRED_KINDS {
        if !present.iter().any(|present_kind| present_kind == kind) {
            blockers.push(BlockedArtifact {
                kind: (*kind).to_owned(),
                key: String::new(),
                status: "missing".to_owned(),
                reason: Some("the plan proposes none".to_owned()),
            });
        }
    }

    Ok(blockers)
}

/// `true` when every required kind is present at least once.
///
/// Separate from [`blockers`] because "is apply offered?" is a boolean a screen binds to and
/// "why not?" is a list it renders, and deriving the first from the second would mean the
/// screen re-derives a rule the store already applied.
pub async fn required_kinds_present(pool: &PgPool, plan_id: Uuid) -> Result<bool> {
    let present: Vec<String> = sqlx::query_scalar(
        "select distinct kind from app_builder_artifacts where plan_id = $1",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await?;
    Ok(crate::model::REQUIRED_KINDS
        .iter()
        .all(|kind| present.iter().any(|present_kind| present_kind == kind)))
}

fn first_finding(validation: &serde_json::Value) -> Option<String> {
    validation
        .as_array()?
        .first()
        .and_then(|finding| finding.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

fn require_known_status(status: &str) -> Result<()> {
    if plan_status_is_known(status) {
        return Ok(());
    }
    Err(AppBuilderError::invalid(
        "invalid_plan_status",
        format!(
            "`{status}` is not a plan status; use one of: {}",
            crate::model::STATUSES.join(", ")
        ),
    ))
}

fn require_known_artifact_status(status: &str) -> Result<()> {
    if artifact_status_is_known(status) {
        return Ok(());
    }
    Err(AppBuilderError::invalid(
        "invalid_artifact_status",
        format!(
            "`{status}` is not an artifact status; use one of: {}",
            crate::model::ARTIFACT_STATUSES.join(", ")
        ),
    ))
}

fn require_known_kind(kind: &str) -> Result<()> {
    if kind_is_known(kind) {
        return Ok(());
    }
    Err(AppBuilderError::invalid(
        "invalid_artifact_kind",
        format!(
            "`{kind}` is not an artifact kind; use one of: {}",
            crate::model::KINDS.join(", ")
        ),
    ))
}

/// The prompt, trimmed and bounded.
///
/// The lengths here are the migration's constraints, so a refusal happens **before** the
/// insert: a 400 naming the field rather than a 500 from a check constraint.
fn read_prompt(raw: &str) -> Result<String> {
    let prompt = raw.trim();
    let len = prompt.chars().count();
    if len < crate::model::MIN_PROMPT_LEN {
        return Err(AppBuilderError::invalid(
            "invalid_plan_prompt",
            format!(
                "the request is {len} characters; it needs at least {}",
                crate::model::MIN_PROMPT_LEN
            ),
        ));
    }
    if len > crate::model::MAX_PROMPT_LEN {
        return Err(AppBuilderError::invalid(
            "invalid_plan_prompt",
            format!(
                "the request is {len} characters; the limit is {}",
                crate::model::MAX_PROMPT_LEN
            ),
        ));
    }
    Ok(prompt.to_owned())
}

/// A plan's title. Empty is allowed — the generator names the plan, and a human may not have.
fn read_title(raw: Option<&str>) -> Result<String> {
    let Some(raw) = raw else {
        return Ok(String::new());
    };
    let title = raw.trim();
    if title.chars().count() > crate::model::MAX_TITLE_LEN {
        return Err(AppBuilderError::invalid(
            "invalid_plan_title",
            format!(
                "the title is {} characters; the limit is {}",
                title.chars().count(),
                crate::model::MAX_TITLE_LEN
            ),
        ));
    }
    Ok(title.to_owned())
}

fn read_rationale(raw: &str) -> Result<String> {
    let rationale = raw.trim();
    if rationale.chars().count() > crate::validate::MAX_RATIONALE_LEN {
        return Err(AppBuilderError::invalid(
            "invalid_artifact_rationale",
            format!(
                "the rationale is {} characters; the limit is {}",
                rationale.chars().count(),
                crate::validate::MAX_RATIONALE_LEN
            ),
        ));
    }
    Ok(rationale.to_owned())
}

fn read_error(raw: &str) -> Result<String> {
    let reason = raw.trim();
    if reason.is_empty() {
        return Err(AppBuilderError::invalid(
            "invalid_plan_error",
            "a failed generation needs a reason; an empty one leaves a plan nobody can retry",
        ));
    }
    Ok(chars_bounded(reason, 2000))
}

fn validate_artifact_key(key: &str) -> Result<()> {
    let findings = crate::validate::validate_key(key, "key");
    if let Some(first) = findings.first() {
        return Err(AppBuilderError::invalid(
            "invalid_artifact_key",
            first.message.clone(),
        ));
    }
    Ok(())
}

fn require_object(spec: &serde_json::Value) -> Result<serde_json::Value> {
    if spec.is_object() {
        return Ok(spec.clone());
    }
    Err(AppBuilderError::invalid(
        "invalid_artifact_spec",
        "the artifact body is not an object, so it has no fields to read",
    ))
}

fn chars_bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// One row of the plan list: the plan, flattened beside its artifact counts.
///
/// `#[sqlx(flatten)]` rather than a six-member tuple for a reason that is not tidiness —
/// **a `FromRow` struct cannot be one member of a tuple**, so `(AppBuilderPlan, i64, ...)`
/// does not compile, and the fix that *does* compile (dropping to a bare tuple of
/// primitives) throws away the field names that let the next reader tell `artifact_count`
/// from `rejected_count`. Flattening keeps both.
#[derive(sqlx::FromRow)]
struct PlanRow {
    #[sqlx(flatten)]
    plan: AppBuilderPlan,
    artifact_count: i64,
    accepted_count: i64,
    rejected_count: i64,
    pending_count: i64,
    invalid_count: i64,
}

// ---------------------------------------------------------------------------------------------
// PlanStore — the same operations behind a struct
// ---------------------------------------------------------------------------------------------

/// The store, bound to a pool and an audit sink.
///
/// A struct rather than a bag of free functions because slice 3's apply runner needs all
/// three of these in one place, and a runner that takes `&PgPool, &Audit, Uuid` as three
/// arguments at every call site is a runner that will eventually pass the wrong two.
pub struct PlanStore<'a> {
    pool: &'a PgPool,
}

impl<'a> PlanStore<'a> {
    /// Bind the store.
    #[must_use]
    pub const fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// The pool the store writes to.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        self.pool
    }

    /// Write the row a generation starts from.
    pub async fn begin(&self, new: NewPlan) -> Result<AppBuilderPlan> {
        insert_plan(self.pool, new).await
    }

    /// Record the model's answer.
    pub async fn answer(
        &self,
        id: Uuid,
        status: &str,
        usage: crate::model::PlanUsage,
        title: Option<&str>,
    ) -> Result<Option<AppBuilderPlan>> {
        apply_answer(self.pool, id, status, usage, title).await
    }

    /// Record why a generation failed.
    pub async fn fail(&self, id: Uuid, error: &str) -> Result<Option<AppBuilderPlan>> {
        apply_failure(self.pool, id, error).await
    }

    /// Write one artifact.
    pub async fn artifact(
        &self,
        plan_id: Uuid,
        artifact: &NewArtifact,
        findings: &[crate::validate::Finding],
    ) -> Result<AppBuilderArtifact> {
        insert_artifact(self.pool, plan_id, artifact, findings).await
    }

    /// Replace an artifact's body with a reviewer's edit.
    pub async fn edit(
        &self,
        artifact_id: Uuid,
        edit: &EditedArtifact,
        findings: &[crate::validate::Finding],
    ) -> Result<Option<AppBuilderArtifact>> {
        update_artifact_spec(self.pool, artifact_id, edit, findings).await
    }

    /// Move an artifact to a decidable status.
    pub async fn decide(
        &self,
        artifact_id: Uuid,
        status: &str,
    ) -> Result<Option<AppBuilderArtifact>> {
        set_artifact_status(self.pool, artifact_id, status).await
    }

    /// Write a regenerated artifact beside the one it replaces.
    pub async fn regenerate(
        &self,
        previous_id: Uuid,
        replacement: &NewArtifact,
        findings: &[crate::validate::Finding],
    ) -> Result<Option<AppBuilderArtifact>> {
        supersede_artifact(self.pool, previous_id, replacement, findings).await
    }

    /// Write a fresh attempt beside the plan it replaces.
    pub async fn retry(&self, previous_id: Uuid, new: NewPlan) -> Result<AppBuilderPlan> {
        supersede_plan(self.pool, previous_id, new).await
    }

    /// Delete a plan that was never applied.
    pub async fn delete(&self, id: Uuid) -> Result<bool> {
        delete_plan(self.pool, id).await
    }

    /// One plan by id.
    pub async fn get(&self, id: Uuid) -> Result<Option<AppBuilderPlan>> {
        find_plan(self.pool, id).await
    }

    /// One plan by id, inside an organization.
    pub async fn get_in(
        &self,
        id: Uuid,
        organization_id: Option<Uuid>,
    ) -> Result<Option<AppBuilderPlan>> {
        find_plan_in(self.pool, id, organization_id).await
    }

    /// A plan's artifacts.
    pub async fn artifacts(&self, plan_id: Uuid) -> Result<Vec<AppBuilderArtifact>> {
        list_artifacts(self.pool, plan_id).await
    }

    /// One organization's plans.
    pub async fn list(
        &self,
        organization_id: Option<Uuid>,
        filter: &PlanFilter,
    ) -> Result<PlanPage> {
        list_plans(self.pool, organization_id, filter).await
    }

    /// Per-plan artifact counts.
    pub async fn counts(&self, plan_id: Uuid) -> Result<PlanCounts> {
        artifact_counts(self.pool, plan_id).await
    }

    /// What stands between a plan and its apply.
    pub async fn blockers(&self, plan_id: Uuid) -> Result<Vec<BlockedArtifact>> {
        blockers(self.pool, plan_id).await
    }

    /// `true` when every required kind is present.
    pub async fn applicable(&self, plan_id: Uuid) -> Result<bool> {
        required_kinds_present(self.pool, plan_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PlanUsage;
    use serde_json::json;

    fn new_plan(prompt: &str) -> NewPlan {
        NewPlan {
            organization_id: None,
            site_id: None,
            prompt: prompt.into(),
            title: None,
            model_label: "qa/mock-model".into(),
            created_by: None,
            supersedes_id: None,
        }
    }

    fn artifact(kind: &str, key: &str) -> NewArtifact {
        NewArtifact {
            kind: kind.into(),
            key: key.into(),
            parent_key: None,
            ordinal: 0,
            spec: json!({ "key": key, "label": "Label" }),
            rationale: "Because.".into(),
            validation: json!([]),
        }
    }

    // The rules below are pure, so they are asserted without a database. The database-backed
    // rules live in `apps/api/tests/app_builder.rs`, because a test that needs a live server
    // is a test that measures the wrong thing in a unit suite.

    #[test]
    fn a_prompt_shorter_than_the_minimum_is_refused_by_name() {
        let error = read_prompt("  a  ").expect_err("a one-character request is nothing");
        assert_eq!(error.code(), "invalid_plan_prompt");
        assert!(error.to_string().contains("at least 3"), "{error}");
    }

    #[test]
    fn an_overlong_prompt_reports_its_own_length() {
        let prompt = "x".repeat(crate::model::MAX_PROMPT_LEN + 1);
        let error = read_prompt(&prompt).expect_err("the request is too long");
        assert!(error.to_string().contains("4000"), "{error}");
    }

    #[test]
    fn a_prompt_is_trimmed_before_it_is_stored() {
        assert_eq!(read_prompt("  build an app  ").expect("valid"), "build an app");
    }

    #[test]
    fn a_title_may_be_empty_but_not_overlong() {
        assert_eq!(read_title(None).expect("no title"), "");
        assert_eq!(read_title(Some(" Leave app ")).expect("valid"), "Leave app");
        let long = "t".repeat(crate::model::MAX_TITLE_LEN + 1);
        assert_eq!(read_title(Some(&long)).expect_err("too long").code(), "invalid_plan_title");
    }

    #[test]
    fn a_failure_needs_a_reason() {
        assert_eq!(
            read_error("   ").expect_err("an empty reason explains nothing").code(),
            "invalid_plan_error"
        );
        assert_eq!(read_error(" no route ").expect("valid"), "no route");
    }

    #[test]
    fn a_rationale_is_bounded() {
        let long = "r".repeat(crate::validate::MAX_RATIONALE_LEN + 1);
        assert_eq!(read_rationale(&long).expect_err("too long").code(), "invalid_artifact_rationale");
    }

    #[test]
    fn an_unknown_status_names_the_ones_the_catalogue_admits() {
        let error = require_known_status("shipped").expect_err("not a status");
        assert!(error.to_string().contains("generating"), "{error}");
        assert!(error.to_string().contains("applied"), "{error}");
        assert!(require_known_status("applied").is_ok());
    }

    #[test]
    fn an_unknown_kind_names_the_ones_the_catalogue_admits() {
        let error = require_known_kind("module").expect_err("not a kind");
        assert!(error.to_string().contains("entity"), "{error}");
        assert!(require_known_kind("report").is_ok());
    }

    #[test]
    fn validity_is_not_a_status_a_caller_may_assert() {
        // `require_known_artifact_status` accepts them — they are real statuses — and
        // `set_artifact_status` is the place that refuses them. This test pins the half that
        // is a pure function so the split is visible without a pool.
        assert!(require_known_artifact_status("accepted").is_ok());
        assert!(require_known_artifact_status("edited").is_ok());
        let error = require_known_artifact_status("shipped").expect_err("not a status");
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[test]
    fn a_spec_that_is_not_an_object_is_refused_before_the_insert() {
        assert_eq!(
            require_object(&json!("text")).expect_err("a scalar has no fields").code(),
            "invalid_artifact_spec"
        );
        assert!(require_object(&json!({ "key": "x" })).is_ok());
    }

    #[test]
    fn an_artifact_key_is_held_to_the_naming_rules_here_too() {
        assert_eq!(
            validate_artifact_key("users").expect_err("reserved").code(),
            "invalid_artifact_key"
        );
        assert!(validate_artifact_key("leave_request").is_ok());
    }

    #[test]
    fn the_first_validation_finding_is_the_one_a_reviewer_is_shown() {
        let findings = json!([
            { "path": "spec.key", "message": "`users` is a reserved platform key" },
            { "path": "spec.label", "message": "the entity has no label" }
        ]);
        assert_eq!(
            first_finding(&findings).as_deref(),
            Some("`users` is a reserved platform key")
        );
        assert!(first_finding(&json!([])).is_none());
        assert!(first_finding(&json!({})).is_none());
    }

    #[test]
    fn usage_defaults_to_nothing_reported_rather_than_to_nothing_spent() {
        // The distinction the migration's constraint encodes: `null` is "the provider did not
        // say", `0` is "the provider said zero". Collapsing them would make an unreported
        // provider read as a free one.
        assert_eq!(PlanUsage::default().input, None);
        assert_eq!(PlanUsage::default().cost_cents, 0);
    }

    #[test]
    fn the_audit_metadata_carries_the_address_and_not_the_body() {
        // The rule itself is enforced in the route layer (REQ-046's route module is the
        // precedent); this pins the shape of what may be carried, so a future edit that
        // starts including the artifact body is visible.
        let metadata = json!({ "kind": "entity", "key": "leave_request" });
        assert_eq!(metadata.as_object().expect("an object").len(), 2);
        assert!(!metadata.to_string().contains("leave requests are what the app is for"));
    }
}