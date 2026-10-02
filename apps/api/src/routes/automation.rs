//! `/api/v1/automations` — the automation surface (docs/requests/REQ-003, phase P13).
//!
//! An automation is **trigger → condition → action**, and it is stored as what it is: a workflow
//! whose trigger is an event (`omnion_workflows`), with the conditions an event payload must
//! satisfy and the actions to run. This module is the panel side of that layer — the matcher and
//! the actions themselves live in `omnion-automation`, and the runs are advanced by the same
//! background engine that drives manual and scheduled workflows.
//!
//! Two rules the handlers enforce on top of the permission guard (`crate::guards`):
//!
//! * tenancy — a rule belongs to one organization and, when it is site-scoped, to one site; an
//!   account with a primary organization only ever touches its own (`crate::scope`);
//! * vocabulary — the event name is checked against the bus's own rule, the conditions against
//!   the automation layer's comparison set and the action parameters (including their
//!   `{{event.field}}` bindings) against the layer's binding rules. A rule the matcher could not
//!   run is refused when it is written, not when its event arrives.
//!
//! Every definition change is audited; a match and a run's lifecycle are audited by the matcher
//! and the engine (`automation.rule.matched` / `workflow.execution.*`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_automation::condition::ConditionOperator;
use omnion_automation::matcher::update_from_rule;
use omnion_automation::model::{validate_description, validate_name};
use omnion_automation::{AutomationRule, Condition, NewRule};
use omnion_workflows::actions::{ACTIONS, ActionDef, HOST_ACTIONS};
use omnion_workflows::definition::StepDefinition;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Event names the platform records today.
///
/// A rule may listen for any name the bus's rule accepts (a module that starts emitting a new
/// event does not need this list updated); these are the ones whose payloads are documented, so
/// the panel can offer them as a starting point.
pub const KNOWN_EVENTS: [&str; 1] = ["page.published"];

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One automation rule.
#[derive(Debug, Serialize)]
pub struct AutomationBody {
    /// Rule id (the workflow id behind it — the run history is addressed by it).
    pub id: Uuid,
    /// Organization that owns the rule.
    pub organization_id: Uuid,
    /// Site the rule is bound to, when it is.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the rule fires.
    pub enabled: bool,
    /// Event the rule listens for.
    pub event: String,
    /// Conditions an event payload must satisfy.
    pub conditions: Vec<Condition>,
    /// Actions to run, in order.
    pub actions: Value,
    /// How many runs the trigger has started.
    pub trigger_count: i32,
    /// When the rule last fired.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_triggered_at: Option<OffsetDateTime>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl AutomationBody {
    /// Describe one rule.
    fn build(rule: &AutomationRule, actions: Value) -> Self {
        Self {
            id: rule.id,
            organization_id: rule.organization_id,
            site_id: rule.site_id,
            name: rule.name.clone(),
            description: rule.description.clone(),
            enabled: rule.enabled,
            event: rule.event.clone(),
            conditions: rule.conditions.clone(),
            actions,
            trigger_count: rule.trigger_count,
            last_triggered_at: rule.last_triggered_at,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
        }
    }
}

/// The list payload.
#[derive(Debug, Serialize)]
pub struct AutomationListResponse {
    /// Matching rules, newest first.
    pub automations: Vec<AutomationBody>,
}

/// One action of the catalogue, as the builder offers it.
#[derive(Debug, Serialize)]
pub struct CatalogueAction {
    /// Stable action key.
    pub key: &'static str,
    /// What it does, in product language.
    pub description: &'static str,
    /// `true` when the action touches the world (an email, a comment) rather than the run only.
    pub host: bool,
}

/// One condition operator of the catalogue.
#[derive(Debug, Serialize)]
pub struct CatalogueOperator {
    /// Stable operator key.
    pub key: &'static str,
    /// `true` when the operator compares against a value.
    pub needs_value: bool,
}

/// The vocabulary a rule is written in.
#[derive(Debug, Serialize)]
pub struct CatalogueResponse {
    /// Event names the platform documents today.
    pub events: Vec<&'static str>,
    /// The closed comparison set.
    pub condition_operators: Vec<CatalogueOperator>,
    /// The closed action set.
    pub actions: Vec<CatalogueAction>,
    /// How a payload field is named inside a condition or a binding.
    pub binding_syntax: &'static str,
}

/// Build the catalogue of the closed vocabulary.
#[must_use]
pub fn catalogue() -> CatalogueResponse {
    let catalogue_action = |action: &ActionDef, host: bool| CatalogueAction {
        key: action.key,
        description: action.description,
        host,
    };

    CatalogueResponse {
        events: KNOWN_EVENTS.to_vec(),
        condition_operators: ConditionOperator::ALL
            .into_iter()
            .map(|operator| CatalogueOperator {
                key: operator.as_str(),
                needs_value: operator.needs_value(),
            })
            .collect(),
        actions: ACTIONS
            .iter()
            .map(|action| catalogue_action(action, false))
            .chain(
                HOST_ACTIONS
                    .iter()
                    .map(|action| catalogue_action(action, true)),
            )
            .collect(),
        binding_syntax: "{{event.field}} — the field is read from the event payload",
    }
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

/// Query of a rule list.
#[derive(Debug, Deserialize)]
pub struct AutomationListQuery {
    /// Organization to read; a platform account must name one to narrow the list.
    pub organization_id: Option<Uuid>,
    /// Site to narrow the list to.
    pub site_id: Option<Uuid>,
}

/// A rule to create or replace.
#[derive(Debug, Deserialize)]
pub struct AutomationInput {
    /// Organization that owns the rule.
    pub organization_id: Option<Uuid>,
    /// Site the rule is bound to.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    #[serde(default)]
    pub description: String,
    /// Whether the rule fires straight away.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Event the rule listens for.
    pub event: String,
    /// Conditions an event payload must satisfy.
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Actions to run, in order.
    pub actions: Vec<StepDefinition>,
}

/// Serde default for [`AutomationInput::enabled`]: a new rule is armed.
fn enabled_by_default() -> bool {
    true
}

impl AutomationInput {
    /// The rule this request describes, checked.
    fn rule(&self, organization_id: Uuid) -> Result<NewRule, ApiError> {
        let rule = NewRule {
            name: validate_name(&self.name)?,
            description: validate_description(&self.description)?,
            enabled: self.enabled,
            site_id: self.site_id,
            event: self.event.clone(),
            conditions: self.conditions.clone(),
            actions: self.actions.clone(),
        };

        // The definition check is the full one: the event name against the bus's rule, the
        // conditions and bindings against the layer's, and the trigger/actions/steps against the
        // engine's. A definition the matcher could not run never reaches the store.
        rule.definition()?;

        // The organization travels with the definition, not inside it; this only proves the
        // caller asked for one the scope check accepted.
        let _ = organization_id;

        Ok(rule)
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/automations/catalogue` — the closed vocabulary a rule is written in.
pub async fn get_catalogue(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<CatalogueResponse>, ApiError> {
    Ok(Json(catalogue()))
}

/// `GET /api/v1/automations` — the rules of one scope.
pub async fn list_automations(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<AutomationListQuery>,
) -> Result<Json<AutomationListResponse>, ApiError> {
    let organization_id = match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(&current, query.organization_id)?;
            Some(own)
        }
        None => query.organization_id,
    };

    let workflows =
        store::list_event_workflows(state.db().pool(), organization_id, query.site_id).await?;

    let mut automations = Vec::with_capacity(workflows.len());
    for workflow in &workflows {
        let Some(rule) = AutomationRule::from_workflow(workflow)? else {
            continue;
        };
        automations.push(AutomationBody::build(&rule, workflow.steps.clone()));
    }

    Ok(Json(AutomationListResponse { automations }))
}

/// `POST /api/v1/automations` — write one rule.
pub async fn create_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<AutomationInput>,
) -> Result<(StatusCode, Json<AutomationBody>), ApiError> {
    let organization_id = resolve_organization(&current, input.organization_id)?;
    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let rule = input.rule(organization_id)?;
    let definition = rule.definition()?;

    let workflow = store::insert_workflow(
        state.db().pool(),
        NewWorkflow {
            organization_id,
            site_id: rule.site_id,
            name: rule.name.clone(),
            description: rule.description.clone(),
            enabled: rule.enabled,
            trigger: definition.trigger.kind,
            schedule: None,
            trigger_event: definition.trigger.event.clone(),
            conditions: definition.conditions_json()?,
            next_run_at: None,
            steps: definition.steps_json()?,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.created")
            .organization(organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "name": workflow.name,
                "event": workflow.trigger_event,
                "conditions": workflow.conditions.as_array().map(Vec::len).unwrap_or_default(),
                "actions": workflow.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let stored = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;
    Ok((
        StatusCode::CREATED,
        Json(AutomationBody::build(&stored, workflow.steps.clone())),
    ))
}

/// `GET /api/v1/automations/{id}` — one rule.
pub async fn get_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
) -> Result<Json<AutomationBody>, ApiError> {
    let workflow = automation_in_scope(&state, &current, automation_id).await?;
    let rule = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;
    Ok(Json(AutomationBody::build(&rule, workflow.steps.clone())))
}

/// `PUT /api/v1/automations/{id}` — replace a rule.
///
/// A rule is replaced, not patched: its conditions and actions are edited as a whole, and the
/// runs that already started keep the steps they materialised — editing never rewrites history.
pub async fn update_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(automation_id): Path<Uuid>,
    Json(input): Json<AutomationInput>,
) -> Result<Json<AutomationBody>, ApiError> {
    let existing = automation_in_scope(&state, &current, automation_id).await?;
    ensure_same_organization(
        &current,
        input.organization_id.or(Some(existing.organization_id)),
    )?;

    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }

    let rule = input.rule(existing.organization_id)?;
    let definition = rule.definition()?;

    let update = update_from_rule(&AutomationRule {
        id: existing.id,
        organization_id: existing.organization_id,
        site_id: rule.site_id,
        name: rule.name.clone(),
        description: rule.description.clone(),
        enabled: rule.enabled,
        event: rule.event.clone(),
        conditions: rule.conditions.clone(),
        actions: rule.actions.clone(),
        trigger_count: existing.trigger_count,
        last_triggered_at: existing.last_triggered_at,
        created_at: existing.created_at,
        updated_at: existing.updated_at,
    })
    .ok_or_else(|| ApiError::bad_request("invalid_request", "the rule could not be stored"))?;

    let workflow = store::update_workflow(state.db().pool(), existing.id, update)
        .await?
        .ok_or_else(automation_not_found)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.updated")
            .organization(existing.organization_id)
            .target("workflow", existing.id.to_string())
            .metadata(json!({
                "name": workflow.name,
                "event": workflow.trigger_event,
                "enabled": workflow.enabled,
                "conditions": workflow.conditions.as_array().map(Vec::len).unwrap_or_default(),
                "actions": workflow.steps.as_array().map(Vec::len).unwrap_or_default(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    let stored = AutomationRule::from_workflow(&workflow)?.ok_or_else(automation_not_found)?;
    let _ = definition;
    Ok(Json(AutomationBody::build(&stored, workflow.steps.clone())))
}

/// `DELETE /api/v1/automations/{id}` — remove a rule and its run history.
pub async fn delete_automation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(automation_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let existing = automation_in_scope(&state, &current, automation_id).await?;

    if !store::delete_workflow(state.db().pool(), existing.id).await? {
        return Err(automation_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "automation.deleted")
            .organization(existing.organization_id)
            .target("workflow", existing.id.to_string())
            .metadata(json!({ "name": existing.name, "event": existing.trigger_event }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load a site and refuse it when it lives outside the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<(), ApiError> {
    let site = omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))?;
    ensure_same_organization(current, Some(site.organization_id))
}

/// Load a rule and refuse it when its organization is out of the caller's scope.
///
/// A workflow that is not event-triggered is `404` here rather than `403`: it is not this
/// surface's object at all, and answering "no such automation" keeps the two surfaces apart.
async fn automation_in_scope(
    state: &AppState,
    current: &CurrentSession,
    automation_id: Uuid,
) -> Result<omnion_workflows::Workflow, ApiError> {
    let workflow = store::find_workflow(state.db().pool(), automation_id)
        .await?
        .ok_or_else(automation_not_found)?;

    if workflow.trigger() != omnion_workflows::TriggerKind::Event {
        return Err(automation_not_found());
    }

    ensure_same_organization(current, Some(workflow.organization_id))?;
    Ok(workflow)
}

fn automation_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "automation_not_found",
        "no such automation",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_lists_the_closed_vocabulary() {
        let catalogue = catalogue();
        assert_eq!(catalogue.events, vec!["page.published"]);

        let operators: Vec<&str> = catalogue
            .condition_operators
            .iter()
            .map(|operator| operator.key)
            .collect();
        assert_eq!(
            operators,
            vec![
                "equals",
                "not_equals",
                "contains",
                "not_contains",
                "starts_with",
                "ends_with",
                "in",
                "exists",
                "not_exists"
            ]
        );
        assert!(
            catalogue
                .condition_operators
                .iter()
                .any(|operator| operator.key == "exists" && !operator.needs_value)
        );

        let actions: Vec<&str> = catalogue.actions.iter().map(|action| action.key).collect();
        assert_eq!(
            actions,
            vec![
                "noop",
                "echo",
                "fail",
                "transient",
                "send_email",
                "comment_revision"
            ]
        );
        assert!(
            catalogue
                .actions
                .iter()
                .any(|action| action.key == "send_email" && action.host),
            "the two real actions are marked as host actions"
        );
        for action in &catalogue.actions {
            assert!(!action.description.trim().is_empty(), "{}", action.key);
        }
        assert!(catalogue.binding_syntax.contains("{{event."));
    }

    #[test]
    fn a_request_becomes_a_checked_rule() {
        let input = AutomationInput {
            organization_id: None,
            site_id: None,
            name: "  Welcome the editor  ".to_owned(),
            description: "  announces a publication  ".to_owned(),
            enabled: true,
            event: "page.published".to_owned(),
            conditions: vec![Condition::compare(
                "status",
                ConditionOperator::Equals,
                json!("published"),
            )],
            actions: vec![StepDefinition::task(
                "tell the editor",
                "send_email",
                json!({
                    "to": "editor@example.com",
                    "subject": "Published: {{event.title}}",
                    "body": "{{event.slug}} is live.",
                }),
            )],
        };

        let rule = input.rule(Uuid::nil()).expect("the rule is valid");
        assert_eq!(rule.name, "Welcome the editor");
        assert_eq!(rule.description, "announces a publication");
        assert_eq!(rule.conditions.len(), 1);
        assert_eq!(rule.actions.len(), 1);
        assert!(rule.definition().is_ok());
    }

    #[test]
    fn a_rule_the_matcher_could_not_run_is_refused_when_it_is_written() {
        let base = |event: &str, conditions: Vec<Condition>, actions: Vec<StepDefinition>| {
            AutomationInput {
                organization_id: None,
                site_id: None,
                name: "rule".to_owned(),
                description: String::new(),
                enabled: true,
                event: event.to_owned(),
                conditions,
                actions,
            }
        };
        let action = || {
            vec![StepDefinition::task(
                "comment",
                "comment_revision",
                json!({ "revision_id": "{{event.revision_id}}", "body": "live" }),
            )]
        };

        // An event the bus could never record.
        let error = base("Page.Published", vec![], action())
            .rule(Uuid::nil())
            .expect_err("event name");
        assert_eq!(error.code(), "invalid_event");

        // A condition without its value.
        let error = base(
            "page.published",
            vec![Condition::presence("status", ConditionOperator::Equals)],
            action(),
        )
        .rule(Uuid::nil())
        .expect_err("condition");
        assert_eq!(error.code(), "invalid_conditions");

        // A binding that names something other than the event.
        let error = base(
            "page.published",
            vec![],
            vec![StepDefinition::task(
                "comment",
                "comment_revision",
                json!({ "revision_id": "{{site.revision_id}}", "body": "live" }),
            )],
        )
        .rule(Uuid::nil())
        .expect_err("binding");
        assert_eq!(error.code(), "invalid_binding");

        // A rule without actions.
        let error = base("page.published", vec![], vec![])
            .rule(Uuid::nil())
            .expect_err("no actions");
        assert_eq!(error.code(), "invalid_steps");

        // A blank name.
        let mut named = base("page.published", vec![], action());
        named.name = "   ".to_owned();
        let error = named.rule(Uuid::nil()).expect_err("name");
        assert_eq!(error.code(), "invalid_name");
    }
}
