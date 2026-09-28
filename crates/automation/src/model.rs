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
use omnion_workflows::{OnError, TriggerKind, Workflow};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

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
    /// Account that created the rule — the authority it follows unless one is chosen.
    pub created_by: Option<Uuid>,
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
    /// The condition tree as it is stored: a group object, or the v0 flat array.
    pub stored_conditions: Value,
    /// Actions to run, in order.
    pub actions: Vec<StepDefinition>,
    /// Whether the rule is triggered by its own inbound webhook URL.
    pub hook_triggered: bool,
    /// Whether that URL has been minted — the token exists. A webhook rule without a
    /// token has a trigger the caller cannot yet call, which is a state the panel says out
    /// loud instead of showing an empty box.
    pub hook_configured: bool,
    /// The rule's own error policy: what a step's failure does when the step inherits it.
    pub on_error: OnError,
    /// Whose authority the rule's host actions run with (REQ-003 slice 3).
    ///
    /// `None` follows the author. Resolved at *run* time, never at write time: a rule whose
    /// author loses a permission must stop on its next run, not keep the snapshot it had when
    /// it was saved.
    pub run_as_user_id: Option<Uuid>,
    /// Runs this rule may start in a rolling hour (REQ-003 slice 4).
    ///
    /// Clamped on read, never trusted: the database refuses an out-of-range value on the
    /// way in, so a value that arrives out of range was written by hand — and a run that
    /// has already been authorised should not fail on a bad integer.
    pub rate_limit_per_hour: i32,
    /// What a second trigger does while a run of this rule is going (REQ-003 slice 4).
    pub concurrency: crate::limits::Concurrency,
    /// The last message a bound produced when it refused a run of this rule.
    ///
    /// `None` is the ordinary state. It is cleared the moment a run is admitted again, so
    /// it answers "the last thing that went wrong" rather than "something once went wrong",
    /// and the rule list can show it without a join against the run history.
    pub last_error: Option<String>,
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
    /// A row this layer wrote always reads back; a row a sibling tool wrote by hand is
    /// reported as an error rather than presented half-parsed.
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

        let actions = workflow.definitions()?;

        // Read the two hook facts before `event` moves into the struct: whether this rule is
        // a webhook rule (it listens for the reserved hook event) and whether its URL has
        // been minted. They are different questions and the panel asks them separately.
        let hook_triggered = event == crate::catalogue::HOOK_EVENT;
        let hook_configured = crate::hooks::has_token(workflow.hook_token_hash.as_deref());

        Ok(Some(Self {
            id: workflow.id,
            organization_id: workflow.organization_id,
            created_by: workflow.created_by,
            site_id: workflow.site_id,
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            enabled: workflow.enabled,
            event,
            // The tree is kept as stored and read on demand: a rule written by v0 carries a
            // flat array, and turning it into a group here would hide that from the audit
            // trail and from the panel's diff.
            stored_conditions: workflow.conditions.clone(),
            actions,
            // A webhook rule is one that listens for the reserved hook event — that is what
            // makes it a webhook rule. Whether its URL has been minted yet is a separate
            // question the panel asks separately (`hook.configured`).
            hook_triggered,
            hook_configured,
            on_error: OnError::parse(&workflow.on_error).unwrap_or(OnError::Stop),
            run_as_user_id: workflow.run_as_user_id,
            // The two bounds, read the same defensive way [`crate::limits::Policy`]
            // reads them: clamped and defaulted rather than trusted, because a guard that
            // trusts a hand-edited column is a guard that can be switched off by editing
            // a row.
            rate_limit_per_hour: crate::limits::Policy::from_columns(
                workflow.rate_limit_per_hour,
                &workflow.concurrency,
            )
            .rate_limit_per_hour,
            concurrency: crate::limits::Concurrency::parse_or_default(&workflow.concurrency),
            last_error: workflow.last_error.clone(),
            trigger_count: workflow.trigger_count,
            last_triggered_at: workflow.last_triggered_at,
            created_at: workflow.created_at,
            updated_at: workflow.updated_at,
        }))
    }

    /// Whose authority this rule's host actions run with.
    ///
    /// The account is resolved here, not at write time, so the panel's "runs as" line and
    /// the engine's own check can never disagree about which account is in play.
    #[must_use]
    pub fn authority(&self) -> crate::authority::Authority {
        crate::authority::Authority::of(self.run_as_user_id, self.created_by)
    }

    /// The conditions of this rule as a group tree.
    ///
    /// `None` when the stored value is unreadable — the matcher treats that as "does not
    /// fire" rather than guessing, and the panel reports the reason.
    #[must_use]
    pub fn condition_group(&self) -> Option<crate::groups::ConditionGroup> {
        crate::groups::read_tree(&self.stored_conditions).ok()
    }

    /// `true` when the rule's conditions hold against a payload.
    #[must_use]
    pub fn conditions_hold(&self, payload: &Value) -> bool {
        crate::groups::stored_holds(&self.stored_conditions, payload)
    }

    /// The definition to store for this rule.
    pub fn definition(&self) -> Result<WorkflowDefinition> {
        build_definition(
            &self.event,
            &self.stored_conditions,
            &self.actions,
            self.hook_triggered,
        )
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
    /// The condition tree as the author sent it: a group object, or a flat array.
    pub stored_conditions: Value,
    /// Actions to run.
    pub actions: Vec<StepDefinition>,
    /// Whether the rule is triggered by its own inbound webhook URL.
    pub hook_triggered: bool,
    /// The rule's own error policy; a step that inherits takes this.
    pub on_error: OnError,
    /// Whose authority the rule runs with. `None` follows the author.
    pub run_as_user_id: Option<Uuid>,
    /// Runs this rule may start in a rolling hour; `None` takes the default.
    ///
    /// `Option` on the way *in* and `i32` on the way *out* is deliberate: a create that
    /// names no limit gets the default without the caller having to know it, and a rule
    /// that is read back always has a number the guard can compare against.
    pub rate_limit_per_hour: Option<i32>,
    /// What a concurrent trigger does; `None` takes the default (`queue`).
    pub concurrency: Option<crate::limits::Concurrency>,
}

impl NewRule {
    /// Check the rule and turn it into the definition the engine stores.
    pub fn definition(&self) -> Result<WorkflowDefinition> {
        validate_name(&self.name)?;
        validate_description(&self.description)?;
        build_definition(
            &self.event,
            &self.stored_conditions,
            &self.actions,
            self.hook_triggered,
        )
    }
}

/// Build the workflow definition of a rule.
///
/// The layers check what they own: this layer checks the event name against the bus's rule,
/// the condition tree and the bindings inside the action parameters; the engine checks the
/// trigger shape, the action catalogue and the step limits.
pub fn build_definition(
    event: &str,
    stored_conditions: &Value,
    actions: &[StepDefinition],
    hook_triggered: bool,
) -> Result<WorkflowDefinition> {
    let event = validate_event(event, hook_triggered)?;

    // A rule with no conditions at all is stored as an empty `all` group rather than as
    // `[]`: one stored shape for a new rule, and the v0 array still reads back as the same
    // thing (migration 0020 widened the column's check for exactly this).
    let group = if matches!(stored_conditions, Value::Array(items) if items.is_empty()) {
        crate::groups::ConditionGroup::all(Vec::new())
    } else {
        crate::groups::validate_tree(stored_conditions)?
    };

    for action in actions {
        crate::binding::validate_bindings(&action.params)?;
    }

    let conditions = crate::groups::to_json(&group);

    let definition = WorkflowDefinition::new(Trigger::event(event), actions.to_vec())?
        .with_conditions(conditions);
    definition.validate()?;

    Ok(definition)
}

/// Check an event name against the rule the bus itself records by.
///
/// A hook-triggered rule listens for the reserved `automation.hook.received` name whatever
/// the caller sent, and nothing else may claim it: the caller's body is the payload, and a
/// module that starts emitting that name would collide with every hook rule.
pub fn validate_event(raw: &str, hook_triggered: bool) -> Result<String> {
    if hook_triggered {
        return Ok(crate::catalogue::HOOK_EVENT.to_owned());
    }

    let event = raw.trim();
    if event == crate::catalogue::HOOK_EVENT {
        return Err(AutomationError::invalid(
            "invalid_event",
            format!(
                "{} is reserved for inbound-webhook triggers; switch the trigger to a webhook",
                crate::catalogue::HOOK_EVENT
            ),
        ));
    }

    omnion_events::validation::validate_event_name(event).map_err(|err| {
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

    fn one_condition() -> Value {
        json!({ "all": [{ "field": "status", "operator": "equals", "value": "published" }] })
    }

    #[test]
    fn a_rule_becomes_a_definition_the_engine_accepts() {
        // A comparison without its value is refused by this layer before the engine sees it.
        let broken = build_definition(
            "page.published",
            &json!({ "all": [{ "field": "status", "operator": "equals" }] }),
            &actions(),
            false,
        );
        assert_eq!(broken.expect_err("no value").code(), "invalid_conditions");

        let definition = build_definition("page.published", &one_condition(), &actions(), false)
            .expect("a complete rule is valid");
        assert_eq!(definition.trigger.kind, TriggerKind::Event);
        assert_eq!(definition.trigger.event.as_deref(), Some("page.published"));
        // The stored conditions are the one-key group object, not the v0 array.
        let conditions = definition.conditions_json().expect("stores");
        assert!(conditions["all"].is_array(), "{conditions}");
        assert_eq!(definition.steps.len(), 1);
    }

    #[test]
    fn an_empty_condition_list_is_stored_as_one_all_group() {
        // Both the v0 `[]` and the panel's empty group mean the same thing, and both are
        // stored the same way — one shape for a new rule, and the old array still reads.
        for empty in [json!([]), json!({ "all": [] })] {
            let definition =
                build_definition("page.published", &empty, &actions(), false).expect("valid");
            assert_eq!(
                definition.conditions_json().expect("stores"),
                json!({ "all": [] })
            );
        }
    }

    #[test]
    fn a_nested_group_survives_the_round_trip_into_the_engine() {
        let nested = json!({
            "any": [
                { "all": [
                    { "field": "status", "operator": "equals", "value": "published" },
                    { "field": "tags", "operator": "contains", "value": "news" }
                ] },
                { "field": "author.email", "operator": "ends_with", "value": "@example.com" }
            ]
        });
        let definition = build_definition("page.published", &nested, &actions(), false)
            .expect("a nested tree is valid");
        assert_eq!(definition.conditions_json().expect("stores"), nested);
    }

    #[test]
    fn a_tree_the_picker_could_not_lay_out_is_refused() {
        let too_deep = json!({
            "all": [{ "all": [{ "all": [{ "all": [{
                "field": "status", "operator": "equals", "value": "published"
            }] }] }] }]
        });
        assert_eq!(
            build_definition("page.published", &too_deep, &actions(), false)
                .expect_err("four levels")
                .code(),
            "invalid_conditions"
        );

        // A shape that is neither a list nor a group is not guessed at.
        assert_eq!(
            build_definition("page.published", &json!("status"), &actions(), false)
                .expect_err("scalar")
                .code(),
            "rule_unreadable"
        );
    }

    #[test]
    fn the_event_name_must_be_one_the_bus_could_record() {
        assert_eq!(
            validate_event("page.published", false).expect("valid"),
            "page.published"
        );
        for broken in ["PagePublished", "page", "", "page.published!"] {
            assert_eq!(
                validate_event(broken, false).expect_err("refused").code(),
                "invalid_event",
                "{broken:?}"
            );
        }
    }

    #[test]
    fn the_hook_event_is_reserved_for_a_webhook_trigger() {
        // A webhook rule always listens for the reserved name, whatever it was sent.
        assert_eq!(
            validate_event("page.published", true).expect("webhook"),
            crate::catalogue::HOOK_EVENT
        );
        // …and nothing else may claim it, or a module emitting it would fire every hook.
        let error =
            validate_event(crate::catalogue::HOOK_EVENT, false).expect_err("reserved for hooks");
        assert_eq!(error.code(), "invalid_event");
        assert!(error.to_string().contains("reserved"));
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
        let error = build_definition("page.published", &json!([]), &actions, false)
            .expect_err("the namespace is wrong");
        assert_eq!(error.code(), "invalid_binding");
    }

    #[test]
    fn a_rule_with_no_actions_is_refused_by_the_engine() {
        let error =
            build_definition("page.published", &json!([]), &[], false).expect_err("no steps");
        assert_eq!(error.code(), "invalid_steps");
    }
}
