//! The rule as the panel sees it, and the workflow definition it becomes.
//!
//! An automation rule has no storage of its own: it *is* a workflow whose trigger is an event
//! (`workflows.trigger_kind = 'event'`, `trigger_event`, `conditions`). That is the whole point of
//! building the layer on the P09 engine — the run, its steps, its retries and its audit trail are
//! the same machine a manual or scheduled workflow uses, and there is no second definition to
//! keep in step with the first.
//!
//! This module is the translation in both directions: a request body becomes a
//! [`WorkflowDefinition`] to store, and a stored [`Workflow`] becomes an [`AutomationRule`] the
//! automations surface can present.

use omnion_workflows::definition::{StepDefinition, Trigger, WorkflowDefinition};
use omnion_workflows::{TriggerKind, Workflow};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::condition::{self, Condition};
use crate::error::{AutomationError, Result};

/// Longest rule name.
pub const MAX_NAME: usize = 80;

/// Longest rule description.
pub const MAX_DESCRIPTION: usize = 400;

/// One automation rule: an event, the conditions it must satisfy, and the actions to run.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationRule {
    /// Workflow id behind the rule (the run history is addressed by it).
    pub id: Uuid,
    /// Organization that owns the rule.
    pub organization_id: Uuid,
    /// Site the rule is bound to, when it is.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the rule fires. A switched-off rule keeps its history.
    pub enabled: bool,
    /// Event the rule listens for.
    pub event: String,
    /// Conditions the payload must satisfy, in order.
    pub conditions: Vec<Condition>,
    /// Actions to run, in order.
    pub actions: Vec<StepDefinition>,
    /// How many runs the trigger has started.
    pub trigger_count: i32,
    /// When it last fired.
    pub last_triggered_at: Option<OffsetDateTime>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl AutomationRule {
    /// Read a stored workflow as a rule; `None` when it is not an event-triggered workflow.
    ///
    /// A row this layer wrote always reads back; a row a sibling tool wrote by hand is reported
    /// as an error rather than presented half-parsed.
    pub fn from_workflow(workflow: &Workflow) -> Result<Option<Self>> {
        if workflow.trigger() != TriggerKind::Event {
            return Ok(None);
        }

        let event = workflow.trigger_event.clone().ok_or_else(|| {
            AutomationError::invalid(
                "rule_unreadable",
                format!(
                    "workflow {} is event-triggered but carries no event name",
                    workflow.id
                ),
            )
        })?;

        let conditions: Vec<Condition> = serde_json::from_value(workflow.conditions.clone())
            .map_err(|err| {
                AutomationError::invalid(
                    "rule_unreadable",
                    format!("the stored conditions are not readable: {err}"),
                )
            })?;

        let actions = workflow.definitions()?;

        Ok(Some(Self {
            id: workflow.id,
            organization_id: workflow.organization_id,
            site_id: workflow.site_id,
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            enabled: workflow.enabled,
            event,
            conditions,
            actions,
            trigger_count: workflow.trigger_count,
            last_triggered_at: workflow.last_triggered_at,
            created_at: workflow.created_at,
            updated_at: workflow.updated_at,
        }))
    }

    /// The definition to store for this rule.
    pub fn definition(&self) -> Result<WorkflowDefinition> {
        build_definition(&self.event, &self.conditions, &self.actions)
    }
}

/// A rule to be written, already parsed from a request body.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRule {
    /// Display name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Whether the rule fires straight away.
    pub enabled: bool,
    /// Site the rule is bound to, when it is.
    pub site_id: Option<Uuid>,
    /// Event the rule listens for.
    pub event: String,
    /// Conditions the payload must satisfy.
    pub conditions: Vec<Condition>,
    /// Actions to run.
    pub actions: Vec<StepDefinition>,
}

impl NewRule {
    /// Check the rule and turn it into the definition the engine stores.
    pub fn definition(&self) -> Result<WorkflowDefinition> {
        validate_name(&self.name)?;
        validate_description(&self.description)?;
        build_definition(&self.event, &self.conditions, &self.actions)
    }
}

/// Build the workflow definition of a rule.
///
/// The layers check what they own: this layer checks the event name against the bus's own rule,
/// the conditions and the bindings inside the action parameters; the engine checks the trigger
/// shape, the action catalogue and the step limits.
pub fn build_definition(
    event: &str,
    conditions: &[Condition],
    actions: &[StepDefinition],
) -> Result<WorkflowDefinition> {
    let event = validate_event(event)?;
    condition::validate(conditions)?;

    for action in actions {
        crate::binding::validate_bindings(&action.params)?;
    }

    let conditions = conditions
        .iter()
        .map(condition::to_json)
        .collect::<Vec<Value>>();

    let definition = WorkflowDefinition::new(Trigger::event(event), actions.to_vec())?
        .with_conditions(conditions);
    definition.validate()?;

    Ok(definition)
}

/// Check an event name against the rule the bus itself records by.
pub fn validate_event(raw: &str) -> Result<String> {
    omnion_events::validation::validate_event_name(raw).map_err(|err| {
        AutomationError::invalid(
            "invalid_event",
            format!("this is not an event the platform can trigger on: {err}"),
        )
    })
}

/// Check a rule name.
pub fn validate_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AutomationError::invalid(
            "invalid_name",
            "every rule needs a name",
        ));
    }
    if name.chars().count() > MAX_NAME {
        return Err(AutomationError::invalid(
            "invalid_name",
            format!("a rule name is at most {MAX_NAME} characters"),
        ));
    }
    Ok(name.to_owned())
}

/// Check a rule description.
pub fn validate_description(raw: &str) -> Result<String> {
    let description = raw.trim();
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(AutomationError::invalid(
            "invalid_description",
            format!("a rule description is at most {MAX_DESCRIPTION} characters"),
        ));
    }
    Ok(description.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::ConditionOperator;
    use serde_json::json;

    fn actions() -> Vec<StepDefinition> {
        vec![StepDefinition::task(
            "tell the editor",
            "send_email",
            json!({
                "to": "editor@example.com",
                "subject": "Published: {{event.title}}",
                "body": "{{event.slug}} is live.",
            }),
        )]
    }

    #[test]
    fn a_rule_becomes_a_definition_the_engine_accepts() {
        // A comparison without its value is refused by this layer before the engine sees it.
        let broken = build_definition(
            "page.published",
            &[Condition::presence("status", ConditionOperator::Equals)],
            &actions(),
        );
        assert_eq!(broken.expect_err("no value").code(), "invalid_conditions");

        let definition = build_definition(
            "page.published",
            &[Condition::compare(
                "status",
                ConditionOperator::Equals,
                json!("published"),
            )],
            &actions(),
        )
        .expect("a complete rule is valid");
        assert_eq!(definition.trigger.kind, TriggerKind::Event);
        assert_eq!(definition.trigger.event.as_deref(), Some("page.published"));
        assert_eq!(definition.conditions.len(), 1);
        assert_eq!(definition.steps.len(), 1);
    }

    #[test]
    fn the_event_name_must_be_one_the_bus_could_record() {
        assert_eq!(
            validate_event("page.published").expect("valid"),
            "page.published"
        );
        for broken in ["PagePublished", "page", "", "page.published!"] {
            assert_eq!(
                validate_event(broken).expect_err("refused").code(),
                "invalid_event",
                "{broken:?}"
            );
        }
    }

    #[test]
    fn names_and_descriptions_are_bounded() {
        assert_eq!(
            validate_name("  Welcome the editor  ").expect("valid"),
            "Welcome the editor"
        );
        assert_eq!(validate_name("").expect_err("empty").code(), "invalid_name");
        assert_eq!(
            validate_name(&"n".repeat(MAX_NAME + 1))
                .expect_err("long")
                .code(),
            "invalid_name"
        );
        assert!(
            validate_description("").is_ok(),
            "a description is optional"
        );
        assert_eq!(
            validate_description(&"d".repeat(MAX_DESCRIPTION + 1))
                .expect_err("long")
                .code(),
            "invalid_description"
        );
    }

    #[test]
    fn a_bad_binding_is_caught_when_the_rule_is_written() {
        let actions = vec![StepDefinition::task(
            "comment",
            "comment_revision",
            json!({ "revision_id": "{{site.revision}}", "body": "hi" }),
        )];
        let error =
            build_definition("page.published", &[], &actions).expect_err("the namespace is wrong");
        assert_eq!(error.code(), "invalid_binding");
    }

    #[test]
    fn a_rule_with_no_actions_is_refused_by_the_engine() {
        let error = build_definition("page.published", &[], &[]).expect_err("no steps");
        assert_eq!(error.code(), "invalid_steps");
    }
}
