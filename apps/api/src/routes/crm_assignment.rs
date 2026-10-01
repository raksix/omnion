//! `/api/v1/crm/assignment/*` and `/api/v1/crm/sla/*` — the rules that decide an owner and a
//! deadline (docs/requests/REQ-117, slice 2).
//!
//! The panel surface for the two tables slice 1 reserved columns for. Three things make it
//! more than CRUD, and each is a decision rather than a convenience:
//!
//! * **The simulator writes nothing and says why.** `POST /crm/assignment/simulate` runs the
//!   *same* [`omnion_module_crm_intake::simulate`] the real claim runs, on the same rows, and
//!   answers the winning rule *and* every rule it passed over with the condition key that
//!   made it miss. A settings screen that only shows "which rule wins" cannot answer the
//!   question an operator actually has, which is "why didn't my other rule win".
//! * **Reorder is a prefix, not a permutation.** `PUT …/order` takes the ids the caller
//!   moved and keeps the rest behind them in their existing relative order, so the panel's
//!   "move up" sends two ids rather than the whole table — and the untouched rules still end
//!   up with a correct, gap-free position.
//! * **The policy read tells the truth about the clock.** Every policy answers its own
//!   `business_hours_only` and window, and a lead's deadline is a stored instant; the panel
//!   never recomputes a deadline in the browser, because a deadline the browser recomputes
//!   is a deadline that disagrees with the escalation the worker already sent.
//!
//! Cross-organization ids answer `404`, never `403`: a `403` is an oracle for "that exists".

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use omnion_events::{NewEvent, bus};
use omnion_module_crm_intake::assignment::{AssignmentInput, AssignmentRule, SlaPolicy};
use omnion_module_crm_intake::assignment_store::{
    self, NewPolicy, NewRule, DEFAULT_POLICY_NAME, DEFAULT_RULE_NAME,
};
use omnion_module_crm_intake::vocabulary::ASSIGNMENT_TARGETS;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

use super::crm_intake::{audit, map_store, not_found, organization_of};

/// A rule as the panel reads it, with the two derived lines the screen would otherwise have
/// to compute itself — and could compute differently from the evaluator.
#[derive(Debug, serde::Serialize)]
pub struct RuleBody {
    #[serde(flatten)]
    pub rule: AssignmentRule,
    /// "no conditions (matches every lead)" or the chips. Server-computed so the screen
    /// cannot render a rule as active-looking while the evaluator ignores it.
    pub condition_summary: String,
    /// The people a match would go to, in pool order. Empty for a queue rule, which is the
    /// honest answer rather than a dash the reader has to interpret.
    pub candidate_count: usize,
    /// What the round-robin cursor would hand the *next* matching lead, without advancing
    /// it. This is why the settings screen is safe to open: reading it costs no fairness.
    pub next_pool_user_id: Option<Uuid>,
}

impl From<AssignmentRule> for RuleBody {
    fn from(rule: AssignmentRule) -> Self {
        let condition_summary = rule.condition_summary();
        let pool = rule.candidate_user_ids();
        let next_pool_user_id = if rule.target_kind == "pool" && !pool.is_empty() {
            pool.get(rule.round_robin_cursor.rem_euclid(pool.len() as i32) as usize)
                .copied()
        } else {
            None
        };
        Self {
            candidate_count: pool.len(),
            next_pool_user_id,
            condition_summary,
            rule,
        }
    }
}

/// A policy as the panel reads it, with its window in a shape the editor can bind to.
#[derive(Debug, serde::Serialize)]
pub struct PolicyBody {
    #[serde(flatten)]
    pub policy: SlaPolicy,
    /// `false` for a half-configured window, so the editor can say *why* the clock runs
    /// around the clock instead of the operator guessing from an empty form.
    pub window_configured: bool,
    pub window: Value,
}

impl From<SlaPolicy> for PolicyBody {
    fn from(policy: SlaPolicy) -> Self {
        let window = policy.business_hours.clone();
        let window_configured =
            omnion_module_crm_intake::assignment::BusinessHours::from_value(&window).is_some();
        Self {
            window_configured,
            window,
            policy,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Create or update a rule.
#[derive(Debug, Deserialize)]
pub struct RuleBody0 {
    pub name: String,
    /// A jsonb conditions document. Typed as `Value` because the closed set of keys is
    /// validated by name in the module — a struct here would make an unknown key a serde
    /// error the operator sees as "invalid JSON" instead of "'timezone' is not a condition a
    /// lead carries".
    #[serde(default)]
    pub conditions: Value,
    pub target_kind: String,
    pub target_user_id: Option<Uuid>,
    #[serde(default)]
    pub pool_user_ids: Vec<Uuid>,
    #[serde(default = "default_true")]
    pub active: bool,
}

impl RuleBody0 {
    fn into_new(self) -> NewRule {
        NewRule {
            name: self.name,
            conditions: if self.conditions.is_null() {
                json!({})
            } else {
                self.conditions
            },
            target_kind: self.target_kind,
            target_user_id: self.target_user_id,
            pool_user_ids: self.pool_user_ids,
            active: self.active,
        }
    }
}

/// Create or update an SLA policy.
#[derive(Debug, Deserialize)]
pub struct PolicyBody0 {
    pub name: String,
    pub first_response_minutes: i32,
    #[serde(default)]
    pub business_hours_only: bool,
    pub reminder_minutes: Option<i32>,
    pub escalate_to_user_id: Option<Uuid>,
    #[serde(default)]
    pub business_hours: Value,
    #[serde(default = "default_true")]
    pub active: bool,
}

impl PolicyBody0 {
    fn into_new(self) -> NewPolicy {
        NewPolicy {
            name: self.name,
            first_response_minutes: self.first_response_minutes,
            business_hours_only: self.business_hours_only,
            reminder_minutes: self.reminder_minutes,
            escalate_to_user_id: self.escalate_to_user_id,
            business_hours: if self.business_hours.is_null() {
                json!({})
            } else {
                self.business_hours
            },
            active: self.active,
        }
    }
}

/// A reorder: the caller's new prefix.
#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    /// The rules in their new order. Ids that are not this organization's are ignored, so a
    /// stale drag cannot fail the whole save.
    #[serde(default)]
    pub ids: Vec<Uuid>,
}

/// The simulator's payload: a pasted submission, evaluated against the live rules.
#[derive(Debug, Deserialize)]
pub struct SimulateBody {
    /// A lead-shaped object: `country`, `product_interest`, `budget_band`, `source_name`,
    /// `email`, … Both the lead's column names and the panel's friendlier aliases are read.
    #[serde(default)]
    pub payload: Value,
    /// When given, the rules are evaluated *as if* this rule were already saved — which is
    /// what makes the screen useful before the save rather than after it.
    pub draft: Option<RuleBody0>,
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/assignment/rules` — the chain, in evaluation order.
pub async fn list_rules(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<RuleBody>>, ApiError> {
    let organization_id = organization_of(&session)?;
    let rows = assignment_store::list_rules(state.db().pool(), organization_id)
        .await
        .map_err(map_store)?;
    Ok(Json(rows.into_iter().map(RuleBody::from).collect()))
}

/// `GET /api/v1/crm/assignment/targets` — the closed list the editor's target picker binds to.
///
/// It is its own route rather than a constant in the panel because the *database* refuses
/// anything outside it: a picker that offers `round_robin` and a constraint that refuses it
/// is a settings screen whose last option is a lie.
pub async fn list_targets() -> Json<Vec<String>> {
    Json(ASSIGNMENT_TARGETS.iter().map(|t| (*t).to_string()).collect())
}

/// `POST /api/v1/crm/assignment/rules` — append a rule at the bottom of the chain.
pub async fn create_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<RuleBody0>,
) -> Result<(axum::http::StatusCode, Json<RuleBody>), ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let rule = assignment_store::create_rule(pool, organization_id, &body.into_new())
        .await
        .map_err(map_store)?;
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.assignment.rule.created",
        rule.id,
        json!({ "name": rule.name, "position": rule.position, "target_kind": rule.target_kind }),
    )
    .await;
    emit(pool, organization_id, rule.id, json!({ "changed_keys": ["created"] })).await;
    Ok((axum::http::StatusCode::CREATED, Json(rule.into())))
}

/// `GET /api/v1/crm/assignment/rules/{id}` — one rule, or 404.
pub async fn get_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<RuleBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let rule = assignment_store::find_rule(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("assignment rule"))?;
    Ok(Json(rule.into()))
}

/// `PATCH /api/v1/crm/assignment/rules/{id}` — the editable fields. `position` is absent on
/// purpose: order changes through `/order`, so there is one way to move a rule and the
/// drag cannot disagree with a form field.
pub async fn update_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<RuleBody0>,
) -> Result<Json<RuleBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let updated = assignment_store::update_rule(pool, organization_id, id, &body.into_new())
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("assignment rule"))?;
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.assignment.rule.updated",
        id,
        json!({ "name": updated.name, "active": updated.active, "target_kind": updated.target_kind }),
    )
    .await;
    emit(pool, organization_id, id, json!({ "changed_keys": ["updated"] })).await;
    Ok(Json(updated.into()))
}

/// `DELETE /api/v1/crm/assignment/rules/{id}` — remove a rule. Leads it decided keep their
/// `assignment_reason`, so the history still explains itself.
pub async fn delete_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    if !assignment_store::delete_rule(pool, organization_id, id)
        .await
        .map_err(map_store)?
    {
        return Err(not_found("assignment rule"));
    }
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.assignment.rule.deleted",
        id,
        json!({}),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `PUT /api/v1/crm/assignment/rules/order` — move rules. The named ids become the new
/// prefix; the rest keep their relative order behind them and the whole chain is renumbered
/// from zero so the next append lands at the bottom.
pub async fn reorder_rules(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<ReorderBody>,
) -> Result<Json<Vec<RuleBody>>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let rows = assignment_store::reorder_rules(pool, organization_id, &body.ids)
        .await
        .map_err(map_store)?;
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.assignment.rules.reordered",
        Uuid::nil(),
        json!({ "moved": body.ids.len(), "order": rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>() }),
    )
    .await;
    Ok(Json(rows.into_iter().map(RuleBody::from).collect()))
}

/// `POST /api/v1/crm/assignment/simulate` — which rule would win, and why the others did not.
///
/// The evaluator is the same pure function the real claim uses, and the cursor is *read*,
/// never advanced: a simulator that consumed fairness would make opening the settings page
/// change who gets the next lead. With a `draft`, the draft is appended at position -1 so the
/// operator sees the effect of the unsaved rule without saving it.
pub async fn simulate(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<SimulateBody>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let mut rules = assignment_store::list_rules(pool, organization_id)
        .await
        .map_err(map_store)?;

    if let Some(draft) = body.draft {
        // Validate the draft through the same validator, so the simulator refuses exactly
        // what a save would refuse. A preview that accepts a rule the save rejects is worse
        // than no preview: the operator presses save and the error arrives afterwards.
        let new = draft.into_new();
        omnion_module_crm_intake::validate_rule(
            &new.name,
            &new.conditions,
            &new.target_kind,
            new.target_user_id,
            &new.pool_user_ids,
        )
        .map_err(map_store)?;
        let now = time::OffsetDateTime::now_utc();
        rules.insert(
            0,
            AssignmentRule {
                id: Uuid::nil(),
                organization_id,
                name: format!("{} (unsaved)", new.name),
                // -1 puts the draft above every saved rule, which is what "let me try this
                // one first" means to the person typing it.
                position: -1,
                conditions: new.conditions,
                target_kind: new.target_kind,
                target_user_id: new.target_user_id,
                pool_user_ids: new.pool_user_ids,
                round_robin_cursor: 0,
                active: new.active,
                created_at: now,
                updated_at: now,
            },
        );
    }

    let input = AssignmentInput::from_payload(&body.payload);
    let outcome = omnion_module_crm_intake::simulate(&rules, &input);
    Ok(Json(json!({
        "outcome": outcome,
        "input_read": {
            "country": input.country,
            "region": input.region,
            "product_interest": input.product_interest,
            "budget_band": input.budget_band,
            "source_name": input.source_name,
            "language": input.language,
            "has_email": input.has_email,
        },
        "rules_considered": rules.len(),
        "wrote_nothing": true,
    })))
}

// ---------------------------------------------------------------------------------------------
// SLA policies
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/sla/policies` — the policies and the organization's default.
pub async fn list_policies(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let rows = assignment_store::list_policies(pool, organization_id)
        .await
        .map_err(map_store)?;
    Ok(Json(json!({
        "policies": rows.into_iter().map(PolicyBody::from).collect::<Vec<_>>(),
        "default_policy_name": DEFAULT_POLICY_NAME,
        "default_rule_name": DEFAULT_RULE_NAME,
        "holidays": "out of scope in this version — a window is a weekly one",
    })))
}

/// `POST /api/v1/crm/sla/policies` — add a policy.
pub async fn create_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<PolicyBody0>,
) -> Result<(axum::http::StatusCode, Json<PolicyBody>), ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let policy = assignment_store::create_policy(pool, organization_id, &body.into_new())
        .await
        .map_err(map_store)?;
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.sla.policy.created",
        policy.id,
        json!({ "name": policy.name, "first_response_minutes": policy.first_response_minutes,
               "business_hours_only": policy.business_hours_only }),
    )
    .await;
    Ok((axum::http::StatusCode::CREATED, Json(policy.into())))
}

/// `GET /api/v1/crm/sla/policies/{id}` — one policy, or 404.
pub async fn get_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<PolicyBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let policy = assignment_store::find_policy(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("SLA policy"))?;
    Ok(Json(policy.into()))
}

/// `PATCH /api/v1/crm/sla/policies/{id}` — edit a policy. Leads already carrying its
/// deadline keep it: the promise was made when they arrived, and moving it afterwards would
/// let an operator erase a breach by editing the policy.
pub async fn update_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<PolicyBody0>,
) -> Result<Json<PolicyBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let updated = assignment_store::update_policy(pool, organization_id, id, &body.into_new())
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("SLA policy"))?;
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.sla.policy.updated",
        id,
        json!({ "name": updated.name, "first_response_minutes": updated.first_response_minutes,
               "business_hours_only": updated.business_hours_only, "active": updated.active }),
    )
    .await;
    Ok(Json(updated.into()))
}

/// `DELETE /api/v1/crm/sla/policies/{id}` — remove a policy. A lead already on it keeps its
/// deadline and reads "the policy is gone", not "the clock is off".
pub async fn delete_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    if !assignment_store::delete_policy(pool, organization_id, id)
        .await
        .map_err(map_store)?
    {
        return Err(not_found("SLA policy"));
    }
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.sla.policy.deleted",
        id,
        json!({}),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Emit the assignment-rule bus event. Best effort by design: an event that cannot be recorded
/// is logged and the request still succeeds, because a lead's assignment is not rolled back by
/// a bus that is briefly unavailable — and the reverse (refusing the assignment because the bus
/// is down) would lose the assignment itself.
///
/// **There is no `name` parameter, and that is the fix.** This helper used to take one
/// (`emit(pool, organization_id, name, target_id, payload)`) and both call sites passed the same
/// value, `crm.intake.rule.updated` — so the parameter could only ever carry that one name, and
/// it cost the name its place in the drift gate. `every_live_name_has_an_emitter` walks the
/// workspace for a quoted string at a `NewEvent::new(…)` constructor; a name that arrives
/// through a binding is invisible to it, so the row read as `Live` with no emitter behind it
/// while the worker emitted it correctly on every rule write. The picker therefore never
/// offered the name: **undeliverable by construction**, and indistinguishable from a
/// misconfigured endpoint to whoever wired one up.
///
/// Widening the scanner was rejected on purpose. A scanner that resolves a parameter would
/// resolve a `format!` too, and a gate that accepts a wire contract assembled at runtime cannot
/// name a contract that drifted — it would have replaced a blind spot with a licence. The cost
/// of the honest repair is one duplicated literal at two call sites, and both call sites are
/// *the same event*: a create and an update are the same fact ("the rules changed"), which is
/// why the payload carries `changed_keys` and why the catalogue row describes both. If a
/// future change needs two genuinely different names here, the helper grows a second
/// constructor rather than a parameter, so the gate keeps seeing them.
async fn emit(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    target_id: Uuid,
    payload: Value,
) {
    // `..payload` is struct-update syntax and is not a `json!` feature: it only compiles as
    // a map literal when `payload` is a `Map`, not a `Value`. Merging into an object by hand
    // is the version that works, and it also lets a caller-supplied key win deliberately
    // rather than by accident.
    let mut body = json!({ "rule_id": target_id.to_string() });
    if let (Some(target), Some(extra)) = (body.as_object_mut(), payload.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    if let Err(error) = bus::emit(
        pool,
        NewEvent::new("crm.intake.rule.updated")
            .organization(organization_id)
            .payload(body),
    )
    .await
    {
        tracing::warn!(
            error = %error,
            "the assignment event could not be recorded"
        );
    }
}

const fn default_true() -> bool {
    true
}
