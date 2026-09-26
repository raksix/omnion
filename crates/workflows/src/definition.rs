//! The definition shape a workflow author writes, and the rules it must satisfy.
//!
//! One structure serves both sides: it deserialises the JSON of `POST /api/v1/workflows`, and
//! it is what the engine stores in `workflows.steps` / the trigger columns. The engine holds a
//! parseable subset of what the visual builder (REQ-004) will produce: an ordered list of
//! steps, no conditions or branching yet — v0 keeps the model small but complete.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::actions;
use crate::cron::CronSchedule;
use crate::error::{Result, WorkflowError};
use crate::model::{StepKind, TriggerKind};

/// Most steps one definition may carry.
pub const MAX_STEPS: usize = 50;

/// Most conditions an event trigger may carry.
pub const MAX_CONDITIONS: usize = 10;

/// Longest event name an event trigger listens for.
pub const MAX_EVENT_NAME: usize = 96;

/// Longest single segment of an event name.
pub const MAX_EVENT_SEGMENT: usize = 32;

/// Most attempts a task step may be given (docs/09-N8N-TEARDOWN.md §13 lesson 4: the n8n cap).
pub const MAX_ATTEMPTS: i32 = 5;

/// Longest a wait step may park a run (one day).
pub const MAX_WAIT_SECONDS: i64 = 86_400;

/// Longest a step name may be.
pub const MAX_STEP_NAME: usize = 80;

/// How a workflow starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// `manual`, `schedule` or `event`.
    pub kind: TriggerKind,
    /// Cron expression, required for a schedule and refused for the other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// Event name (e.g. `page.published`), required for an event trigger and refused for the
    /// other kinds. The bus is the authority on which names exist — see
    /// `omnion_events::validation::validate_event_name`; the engine checks the shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

impl Trigger {
    /// A manual trigger.
    #[must_use]
    pub fn manual() -> Self {
        Self {
            kind: TriggerKind::Manual,
            cron: None,
            event: None,
        }
    }

    /// A cron schedule in UTC.
    #[must_use]
    pub fn schedule(cron: impl Into<String>) -> Self {
        Self {
            kind: TriggerKind::Schedule,
            cron: Some(cron.into()),
            event: None,
        }
    }

    /// An event trigger: the workflow runs when the platform records this event.
    #[must_use]
    pub fn event(name: impl Into<String>) -> Self {
        Self {
            kind: TriggerKind::Event,
            cron: None,
            event: Some(name.into()),
        }
    }

    /// Check the trigger against the engine's rules.
    ///
    /// Also answers the next due time of a schedule, so a definition and its first
    /// `next_run_at` are always derived from the same parse.
    pub fn validate(&self, now: time::OffsetDateTime) -> Result<Option<time::OffsetDateTime>> {
        match self.kind {
            TriggerKind::Manual => {
                if self.cron.is_some() {
                    return Err(WorkflowError::invalid(
                        "invalid_trigger",
                        "a manual trigger carries no cron expression",
                    ));
                }
                if self.event.is_some() {
                    return Err(WorkflowError::invalid(
                        "invalid_trigger",
                        "a manual trigger carries no event name",
                    ));
                }
                Ok(None)
            }
            TriggerKind::Schedule => {
                if self.event.is_some() {
                    return Err(WorkflowError::invalid(
                        "invalid_trigger",
                        "a schedule carries no event name",
                    ));
                }
                let raw = self.cron.as_deref().unwrap_or("").trim();
                if raw.is_empty() {
                    return Err(WorkflowError::invalid(
                        "invalid_trigger",
                        "a schedule needs a cron expression",
                    ));
                }
                let schedule = CronSchedule::parse(raw)?;
                Ok(Some(schedule.next_after(now)?))
            }
            TriggerKind::Event => {
                if self.cron.is_some() {
                    return Err(WorkflowError::invalid(
                        "invalid_trigger",
                        "an event trigger carries no cron expression",
                    ));
                }
                self.event_name()?;
                Ok(None)
            }
        }
    }

    /// The event name of an event trigger, shape-checked.
    pub fn event_name(&self) -> Result<&str> {
        let raw = self.event.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            return Err(WorkflowError::invalid(
                "invalid_trigger",
                "an event trigger needs the event name it listens for",
            ));
        }
        if raw.len() > MAX_EVENT_NAME {
            return Err(WorkflowError::invalid(
                "invalid_trigger",
                format!("an event name is at most {MAX_EVENT_NAME} characters"),
            ));
        }

        // Lower-case, dotted, at least a domain and an action: the same shape the bus stores.
        // Whether the name is one the platform actually emits is the bus's answer, asked by the
        // API layer; here the engine only refuses something no event could ever carry.
        let segments: Vec<&str> = raw.split('.').collect();
        let well_formed = segments.len() >= 2
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && segment.len() <= MAX_EVENT_SEGMENT
                    && segment.starts_with(|first: char| first.is_ascii_lowercase())
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        if !well_formed {
            return Err(WorkflowError::invalid(
                "invalid_trigger",
                format!("{raw:?} is not a lower-case dotted event name, e.g. page.published"),
            ));
        }

        Ok(raw)
    }
}

/// One step of a definition, exactly as it is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepDefinition {
    /// Display name; unique within the workflow.
    pub name: String,
    /// `task` or `wait`.
    pub kind: StepKind,
    /// Built-in action of a task step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Action parameters (task) or `{"seconds": n}` (wait).
    #[serde(default = "empty_params")]
    pub params: Value,
    /// Attempts allowed in total (1–5); a wait step is never retried.
    #[serde(default = "one")]
    pub max_attempts: i32,
}

/// Serde default for [`StepDefinition::max_attempts`]: one attempt, no retries.
fn one() -> i32 {
    1
}

/// Serde default for [`StepDefinition::params`]: an empty object, never `null`.
fn empty_params() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Read the parked seconds of a wait step's parameters.
///
/// Shared by the definition check and the engine, so a stored wait is read the same way the
/// validator accepted it.
pub fn wait_seconds_from(params: &Value) -> Result<i64> {
    let seconds = params
        .get("seconds")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            WorkflowError::invalid(
                "invalid_wait",
                "a wait step needs {\"seconds\": n} in its parameters",
            )
        })?;
    if !(1..=MAX_WAIT_SECONDS).contains(&seconds) {
        return Err(WorkflowError::invalid(
            "invalid_wait",
            format!("a wait step parks between 1 and {MAX_WAIT_SECONDS} seconds, got {seconds}"),
        ));
    }
    Ok(seconds)
}

impl StepDefinition {
    /// A task step.
    #[must_use]
    pub fn task(name: impl Into<String>, action: impl Into<String>, params: Value) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Task,
            action: Some(action.into()),
            params,
            max_attempts: 1,
        }
    }

    /// A wait step.
    #[must_use]
    pub fn wait(name: impl Into<String>, seconds: i64) -> Self {
        Self {
            name: name.into(),
            kind: StepKind::Wait,
            action: None,
            params: serde_json::json!({ "seconds": seconds }),
            max_attempts: 1,
        }
    }

    /// Allow more than one attempt.
    #[must_use]
    pub fn retrying(mut self, max_attempts: i32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Seconds a wait step parks for.
    pub fn wait_seconds(&self) -> Result<i64> {
        wait_seconds_from(&self.params)
    }

    /// Check the step against the engine's rules.
    fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(WorkflowError::invalid(
                "invalid_step_name",
                "every step needs a name",
            ));
        }
        if name.chars().count() > MAX_STEP_NAME {
            return Err(WorkflowError::invalid(
                "invalid_step_name",
                format!("a step name is at most {MAX_STEP_NAME} characters"),
            ));
        }
        if !self.params.is_object() {
            return Err(WorkflowError::invalid(
                "invalid_step_params",
                format!("step \"{name}\" needs a JSON object as its parameters"),
            ));
        }

        match self.kind {
            StepKind::Task => {
                let action = self.action.as_deref().unwrap_or("").trim();
                if !actions::is_action(action) {
                    return Err(WorkflowError::invalid(
                        "invalid_step_action",
                        format!(
                            "step \"{name}\" names the action \"{action}\", which is not one of: {}",
                            actions::keys().join(", ")
                        ),
                    ));
                }
                actions::validate_params(action, &self.params)?;
                self.validate_attempts(name)
            }
            StepKind::Wait => {
                if self.action.is_some() {
                    return Err(WorkflowError::invalid(
                        "invalid_wait",
                        format!("step \"{name}\" is a wait step and cannot name an action"),
                    ));
                }
                if self.max_attempts != 1 {
                    return Err(WorkflowError::invalid(
                        "invalid_max_attempts",
                        format!("step \"{name}\" is a wait step; waits are resumed, not retried"),
                    ));
                }
                self.wait_seconds().map(|_| ())
            }
        }
    }

    /// Attempts must sit inside the engine's cap.
    fn validate_attempts(&self, name: &str) -> Result<()> {
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(WorkflowError::invalid(
                "invalid_max_attempts",
                format!(
                    "step \"{name}\" allows {} attempts; the engine takes 1 to {MAX_ATTEMPTS}",
                    self.max_attempts
                ),
            ));
        }
        Ok(())
    }
}

/// A whole definition: the trigger, the conditions it must satisfy and the ordered steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    /// How the workflow starts.
    pub trigger: Trigger,
    /// Conditions an event trigger's payload must satisfy, in order. Empty for the other
    /// triggers — a manual run and a schedule have no payload to evaluate them against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Value>,
    /// Steps, in the order they run.
    pub steps: Vec<StepDefinition>,
}

impl WorkflowDefinition {
    /// Build a definition and check it.
    pub fn new(trigger: Trigger, steps: Vec<StepDefinition>) -> Result<Self> {
        let definition = Self {
            trigger,
            conditions: Vec::new(),
            steps,
        };
        definition.validate()?;
        Ok(definition)
    }

    /// Attach the conditions of an event trigger.
    #[must_use]
    pub fn with_conditions(mut self, conditions: Vec<Value>) -> Self {
        self.conditions = conditions;
        self
    }

    /// Check the definition against the engine's rules.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(WorkflowError::invalid(
                "invalid_steps",
                "a workflow needs at least one step",
            ));
        }
        if self.steps.len() > MAX_STEPS {
            return Err(WorkflowError::invalid(
                "invalid_steps",
                format!("a workflow carries at most {MAX_STEPS} steps"),
            ));
        }

        let mut seen: Vec<&str> = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            step.validate()?;
            let name = step.name.trim();
            if seen.contains(&name) {
                return Err(WorkflowError::invalid(
                    "invalid_step_name",
                    format!("two steps are both named \"{name}\"; names must be unique"),
                ));
            }
            seen.push(name);
        }

        self.validate_conditions()?;
        self.trigger.validate(time::OffsetDateTime::now_utc())?;
        Ok(())
    }

    /// Conditions belong to an event trigger, and every condition is an object.
    ///
    /// What a condition *means* — which field, which operator — is the automation layer's rule
    /// (`omnion-automation`), which validates the same list before it is stored; the engine
    /// holds the shape, because it is what writes them into `workflows.conditions`.
    fn validate_conditions(&self) -> Result<()> {
        if self.conditions.len() > MAX_CONDITIONS {
            return Err(WorkflowError::invalid(
                "invalid_conditions",
                format!("a trigger carries at most {MAX_CONDITIONS} conditions"),
            ));
        }
        if self.conditions.is_empty() {
            return Ok(());
        }
        if self.trigger.kind != TriggerKind::Event {
            return Err(WorkflowError::invalid(
                "invalid_conditions",
                "conditions belong to an event trigger; a manual run and a schedule have no \
                 payload to evaluate them against",
            ));
        }
        for condition in &self.conditions {
            if !condition.is_object() {
                return Err(WorkflowError::invalid(
                    "invalid_conditions",
                    "every condition is a JSON object",
                ));
            }
        }
        Ok(())
    }

    /// The stored JSON shape of the steps.
    pub fn steps_json(&self) -> Result<Value> {
        serde_json::to_value(&self.steps).map_err(|err| {
            WorkflowError::invalid(
                "invalid_steps",
                format!("the steps cannot be stored: {err}"),
            )
        })
    }

    /// The stored JSON shape of the conditions.
    pub fn conditions_json(&self) -> Result<Value> {
        serde_json::to_value(&self.conditions).map_err(|err| {
            WorkflowError::invalid(
                "invalid_conditions",
                format!("the conditions cannot be stored: {err}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_steps() -> Vec<StepDefinition> {
        vec![
            StepDefinition::task("prepare", "noop", serde_json::json!({})),
            StepDefinition::task("notify", "echo", serde_json::json!({ "value": "hello" })),
            StepDefinition::wait("pause", 5),
        ]
    }

    #[test]
    fn a_complete_definition_validates() {
        let definition = WorkflowDefinition::new(Trigger::manual(), three_steps())
            .expect("a manual three-step workflow is valid");
        assert_eq!(definition.steps.len(), 3);
        assert_eq!(definition.steps[2].wait_seconds().expect("wait"), 5);

        let stored = definition.steps_json().expect("steps serialise");
        assert_eq!(stored.as_array().map(Vec::len), Some(3));
        assert_eq!(stored[0]["kind"], "task");
        assert_eq!(stored[0]["max_attempts"], 1);
        assert_eq!(
            serde_json::from_value::<Vec<StepDefinition>>(stored).expect("steps read back"),
            three_steps()
        );
    }

    #[test]
    fn a_workflow_needs_steps_and_unique_names() {
        let error = WorkflowDefinition::new(Trigger::manual(), Vec::new())
            .expect_err("an empty workflow is refused");
        assert_eq!(error.code(), "invalid_steps");

        let doubled = vec![
            StepDefinition::task("same", "noop", serde_json::json!({})),
            StepDefinition::task("same", "noop", serde_json::json!({})),
        ];
        let error = WorkflowDefinition::new(Trigger::manual(), doubled)
            .expect_err("duplicate names are refused");
        assert_eq!(error.code(), "invalid_step_name");
    }

    #[test]
    fn unknown_actions_are_refused() {
        let steps = vec![StepDefinition::task(
            "call",
            "http_request",
            serde_json::json!({}),
        )];
        let error = WorkflowDefinition::new(Trigger::manual(), steps)
            .expect_err("v0 ships a closed action set");
        assert_eq!(error.code(), "invalid_step_action");
        assert!(error.to_string().contains("noop"), "{error}");
    }

    #[test]
    fn the_attempt_cap_is_enforced() {
        let steps =
            vec![StepDefinition::task("prepare", "noop", serde_json::json!({})).retrying(6)];
        let error = WorkflowDefinition::new(Trigger::manual(), steps)
            .expect_err("more than five attempts is refused");
        assert_eq!(error.code(), "invalid_max_attempts");

        let steps =
            vec![StepDefinition::task("prepare", "noop", serde_json::json!({})).retrying(5)];
        assert!(WorkflowDefinition::new(Trigger::manual(), steps).is_ok());
    }

    #[test]
    fn waits_are_bounded_and_never_retried() {
        let mut step = StepDefinition::wait("pause", 30);
        step.max_attempts = 2;
        let error = WorkflowDefinition::new(Trigger::manual(), vec![step])
            .expect_err("a wait step cannot be retried");
        assert_eq!(error.code(), "invalid_max_attempts");

        let error =
            WorkflowDefinition::new(Trigger::manual(), vec![StepDefinition::wait("pause", 0)])
                .expect_err("a zero-second wait is meaningless");
        assert_eq!(error.code(), "invalid_wait");

        let error = WorkflowDefinition::new(
            Trigger::manual(),
            vec![StepDefinition::wait("pause", 90_000)],
        )
        .expect_err("a wait longer than a day is refused");
        assert_eq!(error.code(), "invalid_wait");
    }

    #[test]
    fn a_schedule_needs_a_parseable_cron() {
        let definition = WorkflowDefinition::new(
            Trigger::schedule("*/15 * * * *"),
            vec![StepDefinition::task(
                "prepare",
                "noop",
                serde_json::json!({}),
            )],
        )
        .expect("a cron schedule is valid");
        let next = definition
            .trigger
            .validate(time::OffsetDateTime::UNIX_EPOCH)
            .expect("the schedule parses")
            .expect("a schedule has a next run");
        assert_eq!(next.minute(), 15);

        let error = WorkflowDefinition::new(
            Trigger::schedule("not a cron"),
            vec![StepDefinition::task(
                "prepare",
                "noop",
                serde_json::json!({}),
            )],
        )
        .expect_err("a broken cron is refused");
        assert_eq!(error.code(), "invalid_cron");
    }

    #[test]
    fn a_manual_trigger_refuses_a_cron() {
        let trigger = Trigger {
            kind: TriggerKind::Manual,
            cron: Some("0 0 * * *".to_owned()),
            event: None,
        };
        let error = WorkflowDefinition::new(
            trigger,
            vec![StepDefinition::task(
                "prepare",
                "noop",
                serde_json::json!({}),
            )],
        )
        .expect_err("a manual trigger carries no schedule");
        assert_eq!(error.code(), "invalid_trigger");
    }

    #[test]
    fn step_parameters_must_be_an_object() {
        let mut step = StepDefinition::task("prepare", "noop", serde_json::json!({}));
        step.params = serde_json::json!(["not", "an", "object"]);
        let error = WorkflowDefinition::new(Trigger::manual(), vec![step])
            .expect_err("parameters must be an object");
        assert_eq!(error.code(), "invalid_step_params");
    }

    #[test]
    fn a_step_without_parameters_gets_an_empty_object() {
        // A definition that leaves `params` out is the common case; it must not arrive as
        // `null`, which is what a bare `#[serde(default)]` on a `Value` would produce.
        let raw = serde_json::json!([{ "name": "prepare", "kind": "task", "action": "noop" }]);
        let steps: Vec<StepDefinition> =
            serde_json::from_value(raw).expect("a parameter-less step parses");
        assert_eq!(steps[0].params, serde_json::json!({}));
        assert!(steps[0].params.is_object());
    }

    #[test]
    fn unknown_step_fields_are_refused() {
        // A typo in a definition is reported, not silently dropped.
        let raw = serde_json::json!([
            { "name": "prepare", "kind": "task", "action": "noop", "runFor": 3 }
        ]);
        assert!(serde_json::from_value::<Vec<StepDefinition>>(raw).is_err());
    }

    #[test]
    fn an_event_trigger_carries_the_event_it_listens_for() {
        let definition = WorkflowDefinition::new(
            Trigger::event("page.published"),
            vec![StepDefinition::task(
                "announce",
                "echo",
                serde_json::json!({ "value": "published" }),
            )],
        )
        .expect("an event trigger is valid");
        assert_eq!(definition.trigger.kind, TriggerKind::Event);
        assert_eq!(
            definition.trigger.event_name().expect("the name reads"),
            "page.published"
        );

        // A cron expression belongs to a schedule, not to an event.
        let mut trigger = Trigger::event("page.published");
        trigger.cron = Some("0 0 * * *".to_owned());
        let error = WorkflowDefinition::new(
            trigger,
            vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
        )
        .expect_err("an event trigger carries no cron");
        assert_eq!(error.code(), "invalid_trigger");

        // An event name the bus could never record is refused with the engine's own message.
        for bad in [
            "PagePublished",
            "page",
            "page.published!",
            "1page.published",
            "",
        ] {
            let error = WorkflowDefinition::new(
                Trigger::event(bad),
                vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
            )
            .expect_err("a malformed event name is refused");
            assert_eq!(error.code(), "invalid_trigger", "{bad}");
        }

        let too_long = format!("page.{}", "a".repeat(MAX_EVENT_SEGMENT + 1));
        assert!(
            WorkflowDefinition::new(
                Trigger::event(too_long),
                vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
            )
            .is_err()
        );
    }

    #[test]
    fn conditions_belong_to_an_event_trigger() {
        let condition =
            serde_json::json!({ "field": "status", "operator": "equals", "value": "published" });

        let definition = WorkflowDefinition::new(
            Trigger::event("page.published"),
            vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
        )
        .expect("the trigger is valid")
        .with_conditions(vec![condition.clone()]);
        definition
            .validate()
            .expect("a condition on an event trigger is valid");
        assert_eq!(
            definition.conditions_json().expect("conditions store")[0]["field"],
            "status"
        );

        // The same condition on a manual workflow has nothing to evaluate it against.
        let manual = WorkflowDefinition::new(
            Trigger::manual(),
            vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
        )
        .expect("the trigger is valid")
        .with_conditions(vec![condition.clone()]);
        assert_eq!(
            manual
                .validate()
                .expect_err("conditions need an event")
                .code(),
            "invalid_conditions"
        );

        // The list is bounded and every entry is an object.
        let many = WorkflowDefinition::new(
            Trigger::event("page.published"),
            vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
        )
        .expect("the trigger is valid")
        .with_conditions(vec![condition; MAX_CONDITIONS + 1]);
        assert_eq!(
            many.validate().expect_err("the cap holds").code(),
            "invalid_conditions"
        );

        let not_an_object = WorkflowDefinition::new(
            Trigger::event("page.published"),
            vec![StepDefinition::task("step", "noop", serde_json::json!({}))],
        )
        .expect("the trigger is valid")
        .with_conditions(vec![serde_json::json!("status")]);
        assert_eq!(
            not_an_object
                .validate()
                .expect_err("a condition is an object")
                .code(),
            "invalid_conditions"
        );
    }
}
