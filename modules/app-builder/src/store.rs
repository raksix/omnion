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
     updated_at, supersedes_id, decision_reason";

/// Columns read back from `app_builder_artifacts`.
const ARTIFACT_COLUMNS: &str = "id, plan_id, kind, key, parent_key, ordinal, status, spec, \
     rationale, validation, supersedes_id, created_at, updated_at, rejected_reason";

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Write the row a generation starts from.
///
/// The row is written **before** the provider is called, so a generation that dies
/// mid-flight leaves a `failed` plan naming its reason instead of leaving nothing at all.
pub async fn insert_plan(pool: &PgPool, new: NewPlan) -> Result<AppBuilderPlan> {
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
pub async fn apply_failure(pool: &PgPool, id: Uuid, error: &str) -> Result<Option<AppBuilderPlan>> {
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
    validate_artifact_key(&artifact.kind, &artifact.key)?;
    let spec = require_object(&artifact.spec)?;
    let rationale = read_rationale(&artifact.rationale)?;
    let validation = serde_json::to_value(findings).map_err(|err| {
        AppBuilderError::invalid("app_builder_artifact_findings", err.to_string())
    })?;
    let status = if findings.is_empty() {
        "pending"
    } else {
        "invalid"
    };

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
    let status = if findings.is_empty() {
        "edited"
    } else {
        "invalid"
    };

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

/// Move an artifact to `rejected` with the reviewer's reason.
///
/// The reason is **required** here and nowhere else, which is the asymmetry that keeps the
/// column honest: a rejection is a decision somebody made, and "reject this" with nothing
/// beside it leaves a rejected artifact and a regenerated one indistinguishable in the apply
/// log. The machine retirement in [`supersede_artifact`] lands on `rejected` too and has
/// nothing to say, which is why it does not come through here.
pub async fn reject_artifact(
    pool: &PgPool,
    artifact_id: Uuid,
    reason: &str,
) -> Result<Option<AppBuilderArtifact>> {
    let reason = read_decision(reason, "app_builder_artifact_rejection")?;
    let row = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
        "update app_builder_artifacts
            set status = 'rejected', rejected_reason = $2, updated_at = now()
          where id = $1 and status <> 'accepted'
        returning {ARTIFACT_COLUMNS}"
    ))
    .bind(artifact_id)
    .bind(&reason)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Accept an artifact a validator already cleared.
///
/// A named function rather than a string argument because "which status may a status write
/// claim" is a rule with three answers — `accepted` is allowed, `rejected` must carry a
/// reason, `edited` is the edit path's alone — and a handler that passes `"accepted"` as a
/// string has to know all three.
pub async fn accept_artifact(
    pool: &PgPool,
    artifact_id: Uuid,
) -> Result<Option<AppBuilderArtifact>> {
    set_artifact_status(pool, artifact_id, "accepted").await
}

/// Reject the whole plan with the reviewer's reason.
///
/// Terminal by design: the store's `where` admits only plans that are still open, so a plan
/// somebody already applied or rejected cannot be re-rejected into a different story — the
/// second attempt finds no row and the handler answers `409` naming the status it is in.
pub async fn reject_plan(
    pool: &PgPool,
    plan_id: Uuid,
    reason: &str,
) -> Result<Option<AppBuilderPlan>> {
    let reason = read_decision(reason, "app_builder_plan_rejection")?;
    let plan = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "update app_builder_plans
            set status = 'rejected', decision_reason = $2, updated_at = now()
          where id = $1 and status in ('generating', 'draft', 'approved')
        returning {PLAN_COLUMNS}"
    ))
    .bind(plan_id)
    .bind(&reason)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
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
    // `edited` is refused and `accepted` is not — and the asymmetry is the point, so it is
    // worth the sentence. An EDIT claims the artifact was corrected, which is something the
    // validator has to re-read the body for, so it belongs to the edit path. An ACCEPT is a
    // person saying "this artifact is right" about a body that was already validated when it
    // was stored, and the review screen has an Accept button that must work: refusing the
    // verb would leave the whole review unable to reach an applicable plan.
    //
    // An earlier version of this refused both. That was an over-correction -- the rule was
    // meant to stop a caller asserting validity, and it went on to delete the only way a
    // reviewer can assert it. The guard belongs on the STATE an artifact is in, not on the
    // verb: see the `invalid` branch below.
    if status == "edited" {
        return Err(AppBuilderError::invalid(
            "app_builder_artifact_not_decidable",
            "`edited` is written by the edit path, which re-validates the body; a status write \
             cannot claim a correction it did not check",
        ));
    }

    let row = if artifact_is_resolved(status) {
        // Accepting an artifact the validator refused is the one write in this module that
        // would make apply unsafe, so it is refused BY NAME and by what is in `validation` --
        // the reviewer is told which finding is in the way rather than being offered a
        // retry that will fail the same way.
        let existing = sqlx::query_as::<_, AppBuilderArtifact>(&format!(
            "select {ARTIFACT_COLUMNS} from app_builder_artifacts where id = $1"
        ))
        .bind(artifact_id)
        .fetch_optional(pool)
        .await?;
        let Some(existing) = existing else {
            return Ok(None);
        };
        let findings = existing.validation.as_array().map_or(0, Vec::len);
        if findings > 0 {
            let first = first_finding(&existing.validation).unwrap_or_default();
            return Err(AppBuilderError::invalid(
                "app_builder_artifact_invalid",
                format!(
                    "`{}` cannot be accepted while it has {} validation finding(s): {first}. \
                     Edit it, or reject it",
                    existing.key, findings
                ),
            ));
        }
        sqlx::query_as::<_, AppBuilderArtifact>(&format!(
            "update app_builder_artifacts
                set status = $2, updated_at = now()
              where id = $1 and status <> 'accepted'
            returning {ARTIFACT_COLUMNS}"
        ))
        .bind(artifact_id)
        .bind(status)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query_as::<_, AppBuilderArtifact>(&format!(
            "update app_builder_artifacts
                set status = $2, updated_at = now()
              where id = $1 and status <> 'accepted'
            returning {ARTIFACT_COLUMNS}"
        ))
        .bind(artifact_id)
        .bind(status)
        .fetch_optional(pool)
        .await?
    };
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
    validate_artifact_key(&replacement.kind, &replacement.key)?;
    let spec = require_object(&replacement.spec)?;
    let rationale = read_rationale(&replacement.rationale)?;
    let validation = serde_json::to_value(findings).map_err(|err| {
        AppBuilderError::invalid("app_builder_artifact_findings", err.to_string())
    })?;
    let status = if findings.is_empty() {
        "pending"
    } else {
        "invalid"
    };

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

    // **Retire the predecessor FIRST, then insert the replacement.** The order is load-bearing and
    // the natural one is wrong: `app_builder_artifacts_unique_key (plan_id, kind, key)` means
    // the live row and its replacement cannot both exist, so inserting before retiring the
    // predecessor — which is what the comment above this function used to describe as "writes
    // the new row first and retires the old one" — raises `duplicate key value violates unique
    // constraint` and the whole regeneration dies as a `500`. The two statements are in ONE
    // transaction and the `for update` above holds the predecessor's lock, so retiring first
    // leaves no window: a concurrent regeneration blocks on the lock, and by the time it
    // reads the row this one's replacement is already in.
    //
    // The retirement writes the machine reason, so a reviewer reading the tree can tell this
    // from their own refusal: "superseded by a regenerated version", not a person deciding.
    // `0226` only requires that a reason is never set on a row that is *not* rejected, so
    // writing one here is allowed.
    sqlx::query(
        "update app_builder_artifacts
            set status = 'rejected',
                rejected_reason = 'superseded by a regenerated version',
                updated_at = now()
          where id = $1",
    )
    .bind(previous_id)
    .execute(&mut *tx)
    .await?;

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
        "update app_builder_plans
            set status = 'rejected',
                decision_reason = 'superseded by a newer attempt',
                updated_at = now()
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

/// What a bulk delete did, and what it refused.
///
/// Two lists rather than a count: "3 of 5 deleted" is a number an operator has to reconstruct,
/// while the ids say what is gone and the refusals say what stayed and why. A bulk that
/// reports only a count renders as a complete one even when half of it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkDelete {
    /// Plans this call removed.
    pub deleted: Vec<Uuid>,
    /// Plans it did not, with the reason each one carries.
    pub failures: Vec<RefusedPlan>,
}

impl BulkDelete {
    /// How many plans the caller asked about.
    #[must_use]
    pub fn requested(&self) -> usize {
        self.deleted.len() + self.failures.len()
    }
}

/// One plan a bulk delete would not remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedPlan {
    /// The plan the caller named.
    pub id: Uuid,
    /// Why it is still there.
    pub message: String,
}

/// Why a plan stays when a bulk delete names it.
///
/// Two sentences, and they are deliberately different: "no such plan" covers both a row that
/// never existed and one belonging to another tenant, because the tenant predicate is part of
/// the read and a bulk that answered "that plan belongs to somebody else" would confirm the
/// existence of every id an operator (or a script) guessed.
///
/// The **status** rather than the row, so the sentence can be proven without a database — this
/// is the one rule a bulk delete has beyond the tenant predicate, and a rule that can only be
/// checked against a live server is a rule nobody checks.
#[must_use]
pub fn refusal_reason(status: Option<&str>, short_id: &str) -> String {
    match status {
        Some("applied") => format!(
            "`{short_id}` was applied — its artifacts are what the live app was built from, so it stays"
        ),
        _ => "no such plan in this organization".to_owned(),
    }
}

/// Eight hex characters of a plan id, the way the console names one.
fn short_id(id: &Uuid) -> String {
    id.as_simple().to_string()[..8].to_owned()
}

/// Delete several plans at once, refusing each one by its own rule.
///
/// **The delete is driven by the scoped read, never by the request.** The rows inside
/// `organization_id` are read first, the applied ones are dropped from that list, and only what
/// remains is handed to a single `delete … where id = any(...)`. Writing the delete straight
/// from the caller's array would be shorter and would be a cross-tenant write: the ids are the
/// caller's, and nothing in a `delete` looks at an organization.
///
/// One round trip for the read and one for the delete, rather than a pair per id: the console
/// selects a page at a time, and twenty selections must not cost forty statements.
pub async fn delete_plans(
    pool: &PgPool,
    ids: &[Uuid],
    organization_id: Option<Uuid>,
) -> Result<BulkDelete> {
    let mut deleted = Vec::new();
    let mut failures = Vec::new();
    if ids.is_empty() {
        return Ok(BulkDelete { deleted, failures });
    }

    // Which of the requested plans exist inside this organization, and in what state.
    let rows = sqlx::query_as::<_, AppBuilderPlan>(&format!(
        "select {PLAN_COLUMNS} from app_builder_plans
          where id = any($1)
            and (organization_id is null or organization_id = $2 or $2 is null)"
    ))
    .bind(ids)
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let eligible: Vec<Uuid> = rows
        .iter()
        .filter(|plan| plan.status != "applied")
        .map(|plan| plan.id)
        .collect();

    if !eligible.is_empty() {
        let removed: Vec<Uuid> = sqlx::query_scalar(
            "delete from app_builder_plans
              where id = any($1) and status <> 'applied'
              returning id",
        )
        .bind(&eligible)
        .fetch_all(pool)
        .await?;
        deleted = removed;
    }

    // Everything the caller named and did not get: refused inside the organization, or absent
    // from it. Both are answered by the same question — was it here? — because the read above
    // already applied the tenant rule.
    for id in ids {
        if deleted.contains(id) {
            continue;
        }
        let short = short_id(id);
        failures.push(RefusedPlan {
            id: *id,
            message: refusal_reason(
                rows.iter()
                    .find(|plan| plan.id == *id)
                    .map(|plan| plan.status.as_str()),
                &short,
            ),
        });
    }

    Ok(BulkDelete { deleted, failures })
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
                p.applied_at, p.created_at, p.updated_at, p.supersedes_id, p.decision_reason, \
                count(a.id)::bigint                                        as artifact_count, \
                count(a.id) filter (where a.status in ('accepted', 'edited'))::bigint as accepted_count, \
                count(a.id) filter (where a.status = 'rejected')::bigint   as rejected_count, \
                count(a.id) filter (where a.status = 'pending')::bigint    as pending_count, \
                count(a.id) filter (where a.status = 'invalid')::bigint    as invalid_count \
           from app_builder_plans p \
           left join app_builder_artifacts a on a.plan_id = p.id \
          where (p.organization_id is not distinct from ",
    );
    builder.push_bind(organization_id);
    builder.push(" or p.organization_id is null)");

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

    let rows = builder.build_query_as::<PlanRow>().fetch_all(pool).await?;

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

    Ok(PlanPage {
        plans,
        counts,
        total,
    })
}

async fn count_matching(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    filter: &PlanFilter,
) -> Result<i64> {
    let mut builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "select count(*) from app_builder_plans \
          where (organization_id is not distinct from ",
    );
    builder.push_bind(organization_id);
    builder.push(" or organization_id is null)");
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
    let present: Vec<String> =
        sqlx::query_scalar("select distinct kind from app_builder_artifacts where plan_id = $1")
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
    let present: Vec<String> =
        sqlx::query_scalar("select distinct kind from app_builder_artifacts where plan_id = $1")
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

fn read_decision(raw: &str, code: &'static str) -> Result<String> {
    let reason = raw.trim();
    if reason.is_empty() {
        return Err(AppBuilderError::invalid(
            code,
            "say why — a rejection with no reason cannot be told apart from a version that \
             was simply overtaken later",
        ));
    }
    Ok(chars_bounded(reason, MAX_DECISION_LEN))
}

/// Longest a reviewer's reason may be. The column has no check of its own: the two machine
/// retirements write `null`, so there is no value to compare a length against in SQL, and the
/// bound belongs where the reason is written rather than in a constraint nothing else uses.
const MAX_DECISION_LEN: usize = 2000;

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

fn validate_artifact_key(kind: &str, key: &str) -> Result<()> {
    // A permission artifact's key is a `domain.action` permission key and the dot is part of
    // that vocabulary, so the **kind** decides who owns the check — and for a permission it is
    // the validator, not this boundary. Checking it here as well made a permission artifact
    // impossible to write at all: `permission` is a required kind, so every plan was blocked by
    // a missing kind that no artifact row could ever fill, and no amount of reviewing could
    // reach an applicable plan. Two owners for one key is how that happened — the store's rule
    // and the validator's rule both claimed the key and only one of them knew about the dot.
    if kind == "permission" {
        return Ok(());
    }
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

    /// Accept an artifact.
    pub async fn accept(&self, artifact_id: Uuid) -> Result<Option<AppBuilderArtifact>> {
        accept_artifact(self.pool, artifact_id).await
    }

    /// Reject an artifact, with the reason a reviewer has to give.
    pub async fn reject(
        &self,
        artifact_id: Uuid,
        reason: &str,
    ) -> Result<Option<AppBuilderArtifact>> {
        reject_artifact(self.pool, artifact_id, reason).await
    }

    /// Reject the whole plan, with its reason.
    pub async fn reject_plan(&self, plan_id: Uuid, reason: &str) -> Result<Option<AppBuilderPlan>> {
        reject_plan(self.pool, plan_id, reason).await
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

    /// Delete a selection, refusing each plan by its own rule.
    pub async fn delete_many(
        &self,
        ids: &[Uuid],
        organization_id: Option<Uuid>,
    ) -> Result<BulkDelete> {
        delete_plans(self.pool, ids, organization_id).await
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
        assert_eq!(
            read_prompt("  build an app  ").expect("valid"),
            "build an app"
        );
    }

    #[test]
    fn a_title_may_be_empty_but_not_overlong() {
        assert_eq!(read_title(None).expect("no title"), "");
        assert_eq!(read_title(Some(" Leave app ")).expect("valid"), "Leave app");
        let long = "t".repeat(crate::model::MAX_TITLE_LEN + 1);
        assert_eq!(
            read_title(Some(&long)).expect_err("too long").code(),
            "invalid_plan_title"
        );
    }

    #[test]
    fn a_failure_needs_a_reason() {
        assert_eq!(
            read_error("   ")
                .expect_err("an empty reason explains nothing")
                .code(),
            "invalid_plan_error"
        );
        assert_eq!(read_error(" no route ").expect("valid"), "no route");
    }

    #[test]
    fn a_rationale_is_bounded() {
        let long = "r".repeat(crate::validate::MAX_RATIONALE_LEN + 1);
        assert_eq!(
            read_rationale(&long).expect_err("too long").code(),
            "invalid_artifact_rationale"
        );
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
            require_object(&json!("text"))
                .expect_err("a scalar has no fields")
                .code(),
            "invalid_artifact_spec"
        );
        assert!(require_object(&json!({ "key": "x" })).is_ok());
    }

    #[test]
    fn an_artifact_key_is_held_to_the_naming_rules_here_too() {
        assert_eq!(
            validate_artifact_key("entity", "users")
                .expect_err("reserved")
                .code(),
            "invalid_artifact_key"
        );
        assert!(validate_artifact_key("entity", "leave_request").is_ok());
    }

    #[test]
    fn a_permission_artifact_key_is_the_validators_to_judge_not_this_boundarys() {
        // The regression this file exists for: with the store checking the key itself, a
        // permission artifact could **never** be written, because `permission` is a required
        // kind — so `blockers` reported it missing on every plan, and no review could ever
        // reach an applicable plan. The two rules both claimed the key and only the validator
        // knew the dot is vocabulary.
        assert!(
            validate_artifact_key("permission", "leave.approve").is_ok(),
            "a `domain.action` permission key is not a storage key"
        );
        // Every OTHER kind is still held to the storage rule, or the exemption would be a
        // blanket hole.
        assert_eq!(
            validate_artifact_key("entity", "Leave.Request")
                .expect_err("mixed case is not a storage key")
                .code(),
            "invalid_artifact_key"
        );
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
        assert!(
            !metadata
                .to_string()
                .contains("leave requests are what the app is for")
        );
    }

    #[test]
    fn an_applied_plan_is_refused_by_name_and_everything_else_by_the_same_sentence() {
        // Two refusals, and they must not be the same one: "it was applied, here it is by name"
        // tells an operator why the row is still on their screen, while "no such plan" covers
        // both a row that never existed and one belonging to another tenant — answering that
        // one differently would confirm the existence of every id a caller guessed.
        let applied = refusal_reason(Some("applied"), "0123abcd");
        assert!(
            applied.contains("0123abcd") && applied.contains("applied"),
            "{applied}"
        );
        for absent in [
            refusal_reason(None, "0123abcd"),
            refusal_reason(Some("draft"), "0123abcd"),
        ] {
            assert_eq!(absent, "no such plan in this organization");
        }
    }

    #[test]
    fn a_bulk_delete_reports_every_requested_plan_exactly_once() {
        // The number the console renders comes from here. If `requested()` counted only the
        // deletions, a selection of three where one was applied would render "1 selected" the
        // moment the call came back and the operator would learn nothing happened to the rest.
        let report = BulkDelete {
            deleted: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            failures: vec![RefusedPlan {
                id: Uuid::from_u128(3),
                message: refusal_reason(Some("applied"), "00000003"),
            }],
        };
        assert_eq!(report.requested(), 3);
        assert_eq!(
            report.deleted.len() + report.failures.len(),
            report.requested()
        );
        assert_eq!(
            BulkDelete {
                deleted: vec![],
                failures: vec![]
            }
            .requested(),
            0
        );
    }

    #[test]
    fn a_short_id_is_eight_characters_and_never_the_whole_uuid() {
        // The console names a plan by its short id, and the refusal quotes that name back. A
        // short id that could carry a `/` or a quote would only ever be safe inside JSON, but
        // the same string ends up in a `Content-Disposition` filename on the export path.
        let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(short_id(&id), "01234567");
        assert_eq!(short_id(&id).len(), 8);
        assert!(!short_id(&id).contains('/'));
        assert!(!short_id(&id).contains('"'));
    }
}
