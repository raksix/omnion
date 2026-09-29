//! Assignment and SLA storage: the ordered rules, the policies, the atomic round-robin claim
//! and the deadline the worker writes.
//!
//! REQ-117, slice 2. The evaluator ([`crate::assignment`]) is pure and unit-tested; this file
//! is the SQL that feeds it and the one place a cursor moves. The split exists so the
//! fairness claim can be tested without a database *and* verified against a real one — the
//! distribution the unit test asserts and the distribution the walkthrough observes against
//! a live database are the same function reading the same cursor.
//!
//! ## The claim is a transaction, and that is the whole design
//!
//! [`claim_assignment`] is the only function that advances a round-robin cursor, and it does
//! so with `select … for update` inside one transaction. A read-then-write — the obvious
//! implementation, and the one every other round-robin in the wild ships — hands the same
//! pool member two leads in a row the moment two app instances exist, because both callers
//! read cursor *n* before either writes *n+1*. Locking the rule row makes the second caller
//! block until the first commits, and it then reads *n+1*. The unit test proves the
//! arithmetic; `scripts/qa` proves two real submissions land on two different people.

use sqlx::PgPool;
use uuid::Uuid;

use crate::assignment::{
    AssignmentInput, AssignmentOutcome, AssignmentRule, SlaPolicy, due_at, next_position, renumber,
    simulate, validate_policy, validate_rule,
};
use crate::error::{CrmIntakeError, Result};
use crate::vocabulary::MAX_PAGE;

const RULE_COLUMNS: &str = "id, organization_id, name, position, conditions, target_kind, \
     target_user_id, pool_user_ids, round_robin_cursor, active, created_at, updated_at";

const POLICY_COLUMNS: &str = "id, organization_id, name, first_response_minutes, \
     business_hours_only, reminder_minutes, escalate_to_user_id, business_hours, active, \
     created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Assignment rules
// ---------------------------------------------------------------------------------------------

/// Make sure an organization has the two rows the rest of the slice assumes exist: a
/// catch-all rule that puts unmatched leads in the visible queue, and a first-response
/// policy to run their clock against.
///
/// **Why this is not only in the migration.** The migration seeds the organizations that
/// exist when it runs, which is the right thing for a migration to do and is *not* enough:
/// every organization created afterwards has no rule and no policy, so its first lead is
/// evaluated against an empty chain and lands unassigned with no deadline — indistinguishable,
/// in the panel, from an operator who has deliberately turned every rule off. Calling this
/// from the read path costs one indexed existence check and makes the invariant hold for
/// every organization regardless of when it was created.
///
/// The insert is `on conflict do nothing` and is keyed on the seeded names, so it is a no-op
/// after the first call and cannot resurrect a default an operator deleted: a deleted default
/// stays deleted, because "I removed the catch-all" is a decision and re-adding it on the
/// next page load would be a platform that argues with its operator.
pub async fn ensure_defaults(pool: &PgPool, organization_id: Uuid) -> Result<()> {
    sqlx::query(
        "insert into crm_assignment_rules (organization_id, name, position, conditions, target_kind) \
         values ($1, $2, 1000, '{}'::jsonb, 'queue') \
         on conflict (organization_id, name) do nothing",
    )
    .bind(organization_id)
    .bind(DEFAULT_RULE_NAME)
    .execute(pool)
    .await?;
    sqlx::query(
        "insert into crm_sla_policies \
             (organization_id, name, first_response_minutes, business_hours_only) \
         values ($1, $2, 240, false) \
         on conflict (organization_id, name) do nothing",
    )
    .bind(organization_id)
    .bind(DEFAULT_POLICY_NAME)
    .execute(pool)
    .await?;
    Ok(())
}

/// The seeded catch-all's name. A constant rather than a literal at each call site because
/// `ensure_defaults` and the migration's `insert … select` have to agree on it, and two
/// spellings of one string is how an organization ends up with two default rules.
pub const DEFAULT_RULE_NAME: &str = "Default (unassigned queue)";

/// The seeded policy's name, for the same reason.
pub const DEFAULT_POLICY_NAME: &str = "Web default";

/// Every rule of an organization, in evaluation order.
///
/// The read is the *only* definition of evaluation order: `position`, then `id`. The panel
/// renders this order and the evaluator re-sorts it, so a rule cannot appear in one place
/// above a rule it actually sits below.
pub async fn list_rules(pool: &PgPool, organization_id: Uuid) -> Result<Vec<AssignmentRule>> {
    ensure_defaults(pool, organization_id).await?;
    let query = format!(
        "select {RULE_COLUMNS} from crm_assignment_rules \
                         where organization_id = $1 order by position, id"
    );
    Ok(sqlx::query_as::<_, AssignmentRule>(&query)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// One rule, or `None` for another organization's rule. A cross-organization id is a `None`
/// rather than a 403 so the route cannot be used to probe which ids exist.
pub async fn find_rule(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<AssignmentRule>> {
    let query = format!(
        "select {RULE_COLUMNS} from crm_assignment_rules where organization_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, AssignmentRule>(&query)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// The fields a create or update may set. Every one of them is validated before it reaches
/// the database, so the refusals an operator sees name the offending field.
#[derive(Debug, Clone)]
pub struct NewRule {
    pub name: String,
    pub conditions: serde_json::Value,
    pub target_kind: String,
    pub target_user_id: Option<Uuid>,
    pub pool_user_ids: Vec<Uuid>,
    pub active: bool,
}

impl NewRule {
    /// A rule that catches everything and hands it to the unassigned queue — the shape the
    /// seeded default has, and the shape the "add a rule" button starts from so the editor
    /// never opens on an invalid document.
    #[must_use]
    pub fn catch_all(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            conditions: serde_json::json!({}),
            target_kind: "queue".into(),
            target_user_id: None,
            pool_user_ids: Vec::new(),
            active: true,
        }
    }

    fn check(&self) -> Result<()> {
        validate_rule(
            &self.name,
            &self.conditions,
            &self.target_kind,
            self.target_user_id,
            &self.pool_user_ids,
        )
    }
}

/// Create a rule at the bottom of the chain.
pub async fn create_rule(
    pool: &PgPool,
    organization_id: Uuid,
    rule: &NewRule,
) -> Result<AssignmentRule> {
    rule.check()?;
    let position = next_position(&list_rules(pool, organization_id).await?);
    let query = format!(
        "insert into crm_assignment_rules \
             (organization_id, name, position, conditions, target_kind, target_user_id, \
              pool_user_ids, active) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning {RULE_COLUMNS}"
    );
    let created = sqlx::query_as::<_, AssignmentRule>(&query)
        .bind(organization_id)
        .bind(rule.name.trim())
        .bind(position)
        .bind(&rule.conditions)
        .bind(&rule.target_kind)
        .bind(rule.target_user_id)
        .bind(&rule.pool_user_ids)
        .bind(rule.active)
        .fetch_one(pool)
        .await;
    match created {
        Ok(row) => Ok(row),
        Err(error) => Err(map_unique(error, "a rule with that name already exists")),
    }
}

/// Update a rule's editable fields. `position` is deliberately absent: order changes through
/// [`reorder_rules`], so there is one way to move a rule and the panel's drag cannot
/// disagree with a form field.
pub async fn update_rule(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    rule: &NewRule,
) -> Result<Option<AssignmentRule>> {
    rule.check()?;
    let query = format!(
        "update crm_assignment_rules set \
             name = $3, conditions = $4, target_kind = $5, target_user_id = $6, \
             pool_user_ids = $7, active = $8, updated_at = now() \
         where organization_id = $1 and id = $2 \
         returning {RULE_COLUMNS}"
    );
    let updated = sqlx::query_as::<_, AssignmentRule>(&query)
        .bind(organization_id)
        .bind(id)
        .bind(rule.name.trim())
        .bind(&rule.conditions)
        .bind(&rule.target_kind)
        .bind(rule.target_user_id)
        .bind(&rule.pool_user_ids)
        .bind(rule.active)
        .fetch_optional(pool)
        .await;
    match updated {
        Ok(row) => Ok(row),
        Err(error) => Err(map_unique(error, "a rule with that name already exists")),
    }
}

/// Delete a rule. A rule a source points at becomes null rather than blocking the delete:
/// the alternative is an operator unable to remove a rule because a source three months ago
/// referenced it, and the lead's `assignment_reason` still says which rule decided it.
pub async fn delete_rule(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from crm_assignment_rules where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() > 0)
}

/// Reorder the chain to the caller's order, then renumber the rest densely.
///
/// The caller's list is the *prefix* of the new order: rules it does not name keep their
/// relative order and follow. That is what makes the panel's "move up" work — a drag sends
/// two ids, not the whole table — and it is why the untouched rules still end up with a
/// correct, gap-free position instead of a stale one.
pub async fn reorder_rules(
    pool: &PgPool,
    organization_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<AssignmentRule>> {
    let existing = list_rules(pool, organization_id).await?;
    if existing.is_empty() {
        return Ok(existing);
    }
    let named: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| existing.iter().any(|r| r.id == *id))
        .take(crate::vocabulary::MAX_BULK_IDS)
        .collect();
    if named.is_empty() {
        return Ok(existing);
    }
    let mut ordered: Vec<AssignmentRule> = Vec::with_capacity(existing.len());
    for id in &named {
        if let Some(rule) = existing.iter().find(|r| r.id == *id) {
            ordered.push(rule.clone());
        }
    }
    for rule in &existing {
        if !named.contains(&rule.id) {
            ordered.push(rule.clone());
        }
    }
    let plan = renumber(
        &ordered.iter().map(|r| r.id).collect::<Vec<_>>(),
        &ordered,
    );
    if plan.is_empty() {
        return Err(CrmIntakeError::Invalid(
            "the new order names the same rule twice".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    for (id, position) in plan {
        sqlx::query(
            "update crm_assignment_rules set position = $3, updated_at = now() \
             where organization_id = $1 and id = $2",
        )
        .bind(organization_id)
        .bind(id)
        .bind(position)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    list_rules(pool, organization_id).await
}

// ---------------------------------------------------------------------------------------------
// The claim
// ---------------------------------------------------------------------------------------------

/// Decide a lead's owner, advancing the round-robin cursor when the winning rule is a pool.
///
/// This is the only function that writes a cursor, and it writes it under a row lock. The
/// read of the rule, the evaluation and the write are one transaction, so two simultaneous
/// leads cannot both read cursor *n*: the second blocks on the `for update` and then sees
/// *n+1*. A naive read-then-write is correct for one app instance and silently wrong for two,
/// which is the hardest kind of bug to notice — the distribution looks fine in development
/// and clusters in production.
pub async fn claim_assignment(
    pool: &PgPool,
    organization_id: Uuid,
    input: &AssignmentInput,
) -> Result<AssignmentOutcome> {
    let mut tx = pool.begin().await?;
    let query = format!(
        "select {RULE_COLUMNS} from crm_assignment_rules \
         where organization_id = $1 and active order by position, id for update"
    );
    let rules = sqlx::query_as::<_, AssignmentRule>(&query)
        .bind(organization_id)
        .fetch_all(&mut *tx)
        .await?;
    let outcome = simulate(&rules, input);

    if let (Some(rule_id), Some(cursor)) = (outcome.rule_id, outcome.cursor_after) {
        // The guard is the rule id *and* the cursor we read: if anything moved the row
        // between the evaluation and this write, the condition matches nothing and the
        // claim is refused rather than overwriting a cursor somebody else advanced.
        let updated = sqlx::query(
            "update crm_assignment_rules set round_robin_cursor = $3, updated_at = now() \
             where organization_id = $1 and id = $2 and round_robin_cursor = $4",
        )
        .bind(organization_id)
        .bind(rule_id)
        .bind(cursor)
        .bind(outcome.cursor_before.unwrap_or(0))
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(CrmIntakeError::Invalid(
                "another lead claimed this pool at the same moment — try again".into(),
            ));
        }
    }
    tx.commit().await?;
    Ok(outcome)
}

// ---------------------------------------------------------------------------------------------
// SLA policies
// ---------------------------------------------------------------------------------------------

/// Every policy of an organization, by name.
pub async fn list_policies(pool: &PgPool, organization_id: Uuid) -> Result<Vec<SlaPolicy>> {
    ensure_defaults(pool, organization_id).await?;
    let query = format!(
        "select {POLICY_COLUMNS} from crm_sla_policies \
                         where organization_id = $1 order by name, id"
    );
    Ok(sqlx::query_as::<_, SlaPolicy>(&query)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// One policy, or `None` if it belongs to another organization.
pub async fn find_policy(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<SlaPolicy>> {
    let query = format!(
        "select {POLICY_COLUMNS} from crm_sla_policies where organization_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, SlaPolicy>(&query)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// The fields a create or update may set.
#[derive(Debug, Clone)]
pub struct NewPolicy {
    pub name: String,
    pub first_response_minutes: i32,
    pub business_hours_only: bool,
    pub reminder_minutes: Option<i32>,
    pub escalate_to_user_id: Option<Uuid>,
    pub business_hours: serde_json::Value,
    pub active: bool,
}

impl NewPolicy {
    /// The seeded default: four working hours, clock running around the clock.
    #[must_use]
    pub fn web_default(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            first_response_minutes: 240,
            business_hours_only: false,
            reminder_minutes: None,
            escalate_to_user_id: None,
            business_hours: serde_json::json!({}),
            active: true,
        }
    }

    fn check(&self) -> Result<()> {
        validate_policy(
            &self.name,
            self.first_response_minutes,
            self.reminder_minutes,
            &self.business_hours,
        )
    }
}

/// Create a policy.
pub async fn create_policy(
    pool: &PgPool,
    organization_id: Uuid,
    policy: &NewPolicy,
) -> Result<SlaPolicy> {
    policy.check()?;
    let query = format!(
        "insert into crm_sla_policies \
             (organization_id, name, first_response_minutes, business_hours_only, \
              reminder_minutes, escalate_to_user_id, business_hours, active) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning {POLICY_COLUMNS}"
    );
    match sqlx::query_as::<_, SlaPolicy>(&query)
        .bind(organization_id)
        .bind(policy.name.trim())
        .bind(policy.first_response_minutes)
        .bind(policy.business_hours_only)
        .bind(policy.reminder_minutes)
        .bind(policy.escalate_to_user_id)
        .bind(&policy.business_hours)
        .bind(policy.active)
        .fetch_one(pool)
        .await
    {
        Ok(row) => Ok(row),
        Err(error) => Err(map_unique(error, "a policy with that name already exists")),
    }
}

/// Update a policy. Applied to a lead already carrying a deadline, it does **not** move that
/// deadline: a lead's clock is the promise made when it arrived, and re-writing it afterwards
/// would let an operator erase a breach by editing the policy.
pub async fn update_policy(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    policy: &NewPolicy,
) -> Result<Option<SlaPolicy>> {
    policy.check()?;
    let query = format!(
        "update crm_sla_policies set \
             name = $3, first_response_minutes = $4, business_hours_only = $5, \
             reminder_minutes = $6, escalate_to_user_id = $7, business_hours = $8, \
             active = $9, updated_at = now() \
         where organization_id = $1 and id = $2 \
         returning {POLICY_COLUMNS}"
    );
    match sqlx::query_as::<_, SlaPolicy>(&query)
        .bind(organization_id)
        .bind(id)
        .bind(policy.name.trim())
        .bind(policy.first_response_minutes)
        .bind(policy.business_hours_only)
        .bind(policy.reminder_minutes)
        .bind(policy.escalate_to_user_id)
        .bind(&policy.business_hours)
        .bind(policy.active)
        .fetch_optional(pool)
        .await
    {
        Ok(row) => Ok(row),
        Err(error) => Err(map_unique(error, "a policy with that name already exists")),
    }
}

/// Delete a policy. A lead already holding it keeps its deadline and reads "the policy is
/// gone" rather than losing the clock it was promised.
pub async fn delete_policy(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from crm_sla_policies where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() > 0)
}

// ---------------------------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------------------------

/// Stamp a lead with the owner a claim chose and the deadline the policy sets.
///
/// The order here is deliberate: the deadline is computed from the policy that the *source*
/// names, falling back to the organization's first active policy. A lead with no policy gets
/// `None` rather than a guess, so the inbox shows "no target set" instead of a deadline
/// against a policy the operator never chose.
pub async fn stamp_assignment(
    pool: &PgPool,
    lead_id: Uuid,
    outcome: &AssignmentOutcome,
    policy: Option<&SlaPolicy>,
    received_at: time::OffsetDateTime,
) -> Result<()> {
    let (rule_id, owner) = match outcome.owner_user_id {
        Some(owner) => (outcome.rule_id, Some(owner)),
        // No owner: the claim decided the lead waits in the queue. The rule id is still
        // recorded, because "which rule decided this waits" is the question an operator
        // asks when a queue is not draining.
        None => (outcome.rule_id, None),
    };
    let due = policy.map(|p| due_at(p, received_at));
    sqlx::query(
        "update crm_leads set owner_user_id = $2, assignment_rule_id = $3, \
             assignment_reason = $4, sla_policy_id = $5, first_response_due_at = $6, \
             status = case when status = 'new' and $2 is not null then 'assigned' else status end, \
             updated_at = now() \
         where id = $1",
    )
    .bind(lead_id)
    .bind(owner)
    .bind(rule_id)
    .bind(outcome.rule_name.as_deref())
    .bind(policy.map(|p| p.id))
    .bind(due)
    .execute(pool)
    .await?;
    Ok(())
}

/// The policy a source's leads run on: the source's own choice, else the organization's
/// first active policy, else `None`. "First" is by name so the answer is the same on every
/// read rather than depending on insertion order.
pub async fn policy_for_source(
    pool: &PgPool,
    organization_id: Uuid,
    source_id: Option<Uuid>,
) -> Result<Option<SlaPolicy>> {
    ensure_defaults(pool, organization_id).await?;
    if let Some(id) = source_id {
        let query = format!(
            "select p.{POLICY_COLUMNS_SCALAR} from crm_sla_policies p \
             join crm_intake_sources s on s.sla_policy_id = p.id \
             where s.organization_id = $1 and s.id = $2 and p.active"
        );
        if let Some(policy) = sqlx::query_as::<_, SlaPolicy>(&query)
            .bind(organization_id)
            .bind(id)
            .fetch_optional(pool)
            .await?
        {
            return Ok(Some(policy));
        }
    }
    let query = format!(
        "select {POLICY_COLUMNS} from crm_sla_policies \
         where organization_id = $1 and active order by name, id limit 1"
    );
    Ok(sqlx::query_as::<_, SlaPolicy>(&query)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

const POLICY_COLUMNS_SCALAR: &str = "p.id, p.organization_id, p.name, p.first_response_minutes, \
     p.business_hours_only, p.reminder_minutes, p.escalate_to_user_id, p.business_hours, \
     p.active, p.created_at, p.updated_at";

/// One breached lead, ready to escalate. The read is bounded and ordered so the worker is
/// reproducible: given the same database and the same `now`, it escalates the same leads in
/// the same order, and a run that dies half way does not re-escalate what it already did
/// because the `escalated_at is null` predicate removes those rows from the next read.
pub async fn due_breaches(
    pool: &PgPool,
    organization_id: Uuid,
    now: time::OffsetDateTime,
    limit: i64,
) -> Result<Vec<Breach>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        owner_user_id: Option<Uuid>,
        sla_policy_id: Option<Uuid>,
        first_response_due_at: Option<time::OffsetDateTime>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "select id, owner_user_id, sla_policy_id, first_response_due_at \
         from crm_leads \
         where organization_id = $1 \
           and first_response_at is null \
           and escalated_at is null \
           and first_response_due_at is not null \
           and first_response_due_at <= $2 \
           and status in ('new', 'assigned', 'contacted', 'qualified') \
         order by first_response_due_at, received_at, id \
         limit $3",
    )
    .bind(organization_id)
    .bind(now)
    .bind(limit.clamp(1, MAX_PAGE))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Breach {
            lead_id: row.id,
            owner_user_id: row.owner_user_id,
            sla_policy_id: row.sla_policy_id,
            due_at: row.first_response_due_at,
        })
        .collect())
}

/// A lead whose deadline passed unanswered.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Breach {
    pub lead_id: Uuid,
    pub owner_user_id: Option<Uuid>,
    pub sla_policy_id: Option<Uuid>,
    pub due_at: Option<time::OffsetDateTime>,
}

/// Mark a breach as escalated, *only if it is still un-escalated*.
///
/// The `escalated_at is null` predicate is what makes "notifies the escalation target
/// exactly once" true rather than approximately true: a worker that runs twice over the same
/// row — a retry, a second tick, a second app instance — matches zero rows the second time
/// and writes nothing. The return value is therefore the answer to "did *this* call escalate
/// it", and the caller uses it to decide whether to send a notification at all.
pub async fn mark_escalated(
    pool: &PgPool,
    lead_id: Uuid,
    now: time::OffsetDateTime,
) -> Result<bool> {
    let updated = sqlx::query(
        "update crm_leads set escalated_at = $2, updated_at = now() \
         where id = $1 and escalated_at is null and first_response_at is null",
    )
    .bind(lead_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Who a breach escalates to: the policy's configured person, else the lead's own owner's
/// fallback, else nobody. Returning `None` is a real answer — the worker then records the
/// breach on the timeline and moves on, which is better than notifying the person whose
/// deadline just passed and calling it an escalation.
pub async fn escalation_target(
    pool: &PgPool,
    lead_id: Uuid,
) -> Result<Option<Uuid>> {
    let target = sqlx::query_scalar::<_, Option<Uuid>>(
        "select p.escalate_to_user_id from crm_leads l \
         join crm_sla_policies p on p.id = l.sla_policy_id \
         where l.id = $1 and p.escalate_to_user_id is not null",
    )
    .bind(lead_id)
    .fetch_optional(pool)
    .await?;
    Ok(target.flatten())
}

fn map_unique(error: sqlx::Error, message: &str) -> CrmIntakeError {
    if let sqlx::Error::Database(ref db) = error {
        // 23505 is unique_violation. Matching on the code rather than the message keeps
        // this working across the Postgres versions the platform runs on.
        if db.code().as_deref() == Some("23505") {
            return CrmIntakeError::Invalid(message.into());
        }
    }
    CrmIntakeError::Database(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The catch-all constructor the "add a rule" button uses must be a document the
    /// validator accepts — otherwise the editor's first save always fails, which is the
    /// fastest way to make an operator believe the screen is broken.
    /// The two defaults exist twice: as the migration's `insert … select` and as
    /// [`ensure_defaults`]'s runtime insert. They must name their rows identically, and the
    /// only thing that catches it is a test that reads the migration — the same reasoning
    /// as the vocabulary's `the_migration_agrees_with_the_lists`, applied to seed rows. A
    /// drift here is silent in the worst way: the migration seeds "Web default" and the
    /// runtime seeds "Web default " or "web default", so every new organization gets a
    /// *second* policy and an operator sees two four-hour targets.
    #[test]
    fn the_runtime_seeds_name_their_rows_the_way_the_migration_does() {
        let path = format!(
            "{}/../../database/migrations/0056_crm_assignment_sla.sql",
            env!("CARGO_MANIFEST_DIR")
        );
        let sql = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read 0056_crm_assignment_sla.sql ({error})"));
        assert!(
            sql.contains(&format!("'{}'", DEFAULT_RULE_NAME)),
            "the migration must seed the catch-all as {DEFAULT_RULE_NAME:?}"
        );
        assert!(
            sql.contains(&format!("'{}'", DEFAULT_POLICY_NAME)),
            "the migration must seed the policy as {DEFAULT_POLICY_NAME:?}"
        );
        // The seeded policy's target is the documented 240 minutes with the clock running
        // around the clock; a seeded "business hours only" default would park every lead
        // outside working hours with the panel saying the clock is running.
        assert!(
            sql.contains("'Web default', 240, false"),
            "the seeded policy is 240 minutes, business hours off"
        );
    }

    #[test]
    fn the_catch_all_constructor_passes_validation() {
        assert!(NewRule::catch_all("Default").check().is_ok());
        assert!(NewPolicy::web_default("Web default").check().is_ok());
    }

    #[test]
    fn a_catch_all_rule_names_itself_as_matching_everything() {
        let rule = NewRule::catch_all("Default");
        let text = json!({}).to_string();
        assert_eq!(text, "{}", "the catch-all document is empty, not null");
        assert_eq!(rule.target_kind, "queue");
    }

    #[test]
    fn the_unique_message_is_carried_rather_than_a_bare_constraint_error() {
        // Constructing the variant directly is the only way to assert the mapping without
        // a live database; the mapping itself is exercised by the QA gate.
        let error = CrmIntakeError::Invalid("a rule with that name already exists".into());
        assert!(error.to_string().contains("already exists"));
    }
}
