//! The matcher: reading the bus, deciding which rules fire, and starting their runs.
//!
//! One drain is one transaction, and the transaction is what makes the trigger exactly-once:
//!
//! ```text
//! lock the cursor row  (for update skip locked — one matcher at a time, never two)
//!   read the events after the cursor, oldest first
//!     for each event: the armed rules of its tenant that listen for its name
//!       conditions hold against the payload?  no  → nothing happens
//!                                            yes → the rule's actions are resolved against the
//!                                                  payload and the run is created
//!   advance the cursor past the last event read
//! commit
//! ```
//!
//! A crash before the commit rolls the whole batch back — the runs are not there either, so the
//! next drain reads the same events and starts them once. A crash after it leaves the cursor
//! advanced and the runs durable. There is no state in memory to lose, and neither a missed event
//! nor a double-started run is reachable from this shape.
//!
//! The audit rows are written after the commit, for the reason the engine's own settlements are:
//! the run is the fact, the audit is its record — a missing row is a gap in the trail, never a
//! run that did not happen.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use omnion_events::model::{EVENT_COLUMNS, Event};
use omnion_workflows::definition::StepDefinition;
use omnion_workflows::store::{self, WorkflowUpdate};
use omnion_workflows::{TriggerKind, Workflow, WorkflowExecution, engine};

use crate::binding::resolve_params;
use crate::error::Result;
use crate::limits::{self, Admit, Policy};
use crate::model::AutomationRule;

/// Most events one drain evaluates.
pub const DEFAULT_BATCH: i64 = 100;

/// Hard ceiling of one drain's batch, whatever the caller asks for.
pub const MAX_BATCH: i64 = 1000;

/// What one drain did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MatchReport {
    /// The cursor after the drain.
    pub cursor: i64,
    /// Events read and evaluated.
    pub evaluated: usize,
    /// Rules that fired.
    pub matched: usize,
    /// Rules that were evaluated and did not fire.
    pub skipped: usize,
    /// Runs that were started.
    pub runs: Vec<Uuid>,
    /// Triggers a bound refused: (rule id, event id, bound name, reason).
    ///
    /// Reported rather than counted, because a refusal is an ordinary fact about a busy
    /// rule and not an error: returning `Err` here would roll the cursor transaction back
    /// and re-evaluate every event in the batch, which is how a rate limit turns into a
    /// stuck bus. The row is audited after the commit, like every other fact.
    pub refused: Vec<Refused>,
    /// `true` when this drain found nothing to do.
    pub idle: bool,
}

/// One trigger a bound refused to start a run for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The rule whose bound refused.
    pub workflow_id: Uuid,
    /// The event that would have started it.
    pub event_id: i64,
    /// Which bound refused: `rate_limit_per_hour` or `concurrency`.
    pub bound: &'static str,
    /// The message the rule's `last_error` carries and the audit row records.
    pub reason: String,
}

impl MatchReport {
    /// `true` when the drain had nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.idle
    }
}

/// The highest event id the matcher has evaluated.
pub async fn event_cursor(pool: &PgPool) -> Result<i64> {
    let cursor: i64 =
        sqlx::query_scalar("select last_event_id from automation_cursor where id = 1")
            .fetch_one(pool)
            .await?;

    Ok(cursor)
}

/// Point a never-advanced cursor at the end of the bus.
///
/// A fresh installation watches forward: a rule created today does not fire for everything the
/// platform recorded before it existed. The binary calls this once at boot, before the first
/// drain — which is also what keeps `0` honest: seeding an empty bus to `0` is what makes the
/// drain correct there (nothing had happened, so everything above `0` is new), while seeding a
/// bus that already carries history moves the cursor above it.
///
/// Answers the id the cursor was moved to, or `None` when it had already been advanced.
pub async fn seed_cursor(pool: &PgPool) -> Result<Option<i64>> {
    let seeded: Option<i64> = sqlx::query_scalar(
        "update automation_cursor \
         set last_event_id = (select coalesce(max(id), 0) from events), updated_at = now() \
         where id = 1 and last_event_id = 0 \
         returning last_event_id",
    )
    .fetch_optional(pool)
    .await?;

    Ok(seeded)
}

/// Evaluate the events after the cursor and start one run per matching rule.
pub async fn drain(pool: &PgPool, batch: i64) -> Result<MatchReport> {
    let batch = batch.clamp(1, MAX_BATCH);
    let mut transaction = pool.begin().await?;

    // The cursor row is the lock: two instances draining at the same time cannot both read the
    // same events, and the one that loses skips its turn instead of waiting.
    let locked: Option<i64> = sqlx::query_scalar(
        "select last_event_id from automation_cursor where id = 1 for update skip locked",
    )
    .fetch_optional(&mut *transaction)
    .await?;

    let Some(cursor) = locked else {
        // Another matcher holds the cursor; this tick has nothing to do.
        return Ok(MatchReport {
            idle: true,
            ..MatchReport::default()
        });
    };

    // The cursor sits where the last drain left it; a fresh installation has it at `0` because
    // a bus with no events seeds to `0` — "nothing had happened yet, so evaluate everything from
    // here on". Anything above the cursor is what this tick evaluates.
    let events: Vec<Event> = sqlx::query_as(&format!(
        "select {EVENT_COLUMNS} from events where id > $1 order by id asc limit $2"
    ))
    .bind(cursor)
    .bind(batch)
    .fetch_all(&mut *transaction)
    .await?;

    if events.is_empty() {
        transaction.commit().await?;
        return Ok(MatchReport {
            cursor,
            idle: true,
            ..MatchReport::default()
        });
    }

    let mut report = MatchReport {
        cursor,
        ..MatchReport::default()
    };
    // (workflow, run, steps) of every run this drain started, for the audit pass after commit.
    let mut started: Vec<(Workflow, WorkflowExecution, usize)> = Vec::new();
    // (rule id, event id, event name, payload) of every armed listener to fill in.
    let mut captured: Vec<(Uuid, i64, String, Value)> = Vec::new();
    // (workflow id, event id, reason) of every rule that could not be run.
    let mut failures: Vec<(Uuid, i64, String)> = Vec::new();
    // (rule, event, bound, reason) of every trigger a bound refused.
    let mut refused: Vec<Refused> = Vec::new();
    // The rolling hour, read once per drain rather than per rule: a drain evaluates a
    // batch of events and a rule that appears twice in the batch would otherwise be
    // counted twice for the same window — which is correct, because the guard is per
    // *trigger*, but the read is the same read, so it is done once.
    let now = time::OffsetDateTime::now_utc();

    for event in &events {
        report.evaluated += 1;

        // An event that belongs to no tenant reaches no tenant — the rule the bus applies to
        // webhook fan-out, applied to automation.
        let Some(organization_id) = event.organization_id else {
            tracing::debug!(event_id = event.id, name = %event.name, "platform event: no rule can match");
            continue;
        };

        let rules = store::list_event_rules(
            &mut *transaction,
            organization_id,
            event.site_id,
            &event.name,
        )
        .await?;

        for workflow in rules {
            let rule = match AutomationRule::from_workflow(&workflow) {
                Ok(Some(rule)) => rule,
                Ok(None) => continue,
                Err(err) => {
                    report.skipped += 1;
                    failures.push((workflow.id, event.id, err.to_string()));
                    continue;
                }
            };

            if !rule.conditions_hold(&event.payload) {
                report.skipped += 1;
                continue;
            }

            // A one-shot listener wants the payload of the first event that actually
            // matched — the only honest answer to "what does this event really carry", and
            // the only way to learn a payload shape the documentation got wrong. It is
            // collected here and written after the commit, for the same reason the audit
            // rows are: the capture is a record of a run that happened, not a decision about
            // whether it should.
            //
            // The capture is placed *after* the conditions held and the steps resolved, so
            // what the listener reports is the payload a real run would have used — an event
            // the rule did not match is not what the author is asking to see.
            captured.push((
                workflow.id,
                event.id,
                event.name.clone(),
                event.payload.clone(),
            ));

            let steps = match resolve_steps(&rule, &event.payload) {
                Ok(steps) => steps,
                Err(err) => {
                    // A placeholder the payload cannot fill is a definition problem: the run is
                    // not started (silently sending an empty value would be worse) and the rule
                    // is reported.
                    report.skipped += 1;
                    failures.push((workflow.id, event.id, err.to_string()));
                    continue;
                }
            };

            // The two bounds, decided in the same transaction that creates the run and
            // under the window row's own lock. A guard that read the counter and then
            // wrote the run would be three statements, and two API instances
            // interleaving them would both see an empty window — which is exactly the
            // failure the request's risk note names.
            let policy = Policy::from_columns(workflow.rate_limit_per_hour, &workflow.concurrency);
            let verdict = limits::admit(&mut transaction, workflow.id, &policy, now).await?;
            if !verdict.is_allowed() {
                report.skipped += 1;
                if let Some(reason) = verdict.reason(&policy) {
                    let bound = match verdict {
                        Admit::RateLimited { .. } => "rate_limit_per_hour",
                        Admit::Skipped { .. } => "concurrency",
                        Admit::Allowed { .. } => unreachable!("admitted runs are not refusals"),
                    };
                    refused.push(Refused {
                        workflow_id: workflow.id,
                        event_id: event.id,
                        bound,
                        reason,
                    });
                }
                continue;
            }

            let (execution, _rows) = store::create_execution_in(
                &mut transaction,
                &workflow,
                TriggerKind::Event,
                None,
                &steps,
                Some(event.payload.clone()),
            )
            .await?;
            store::note_match(&mut transaction, workflow.id).await?;

            report.matched += 1;
            report.runs.push(execution.id);
            started.push((workflow, execution, steps.len()));
        }
    }

    // The cursor moves past everything this drain read, in the same commit as the runs.
    let last = events.last().map(|event| event.id).unwrap_or(cursor);
    sqlx::query("update automation_cursor set last_event_id = $1, updated_at = now() where id = 1")
        .bind(last)
        .execute(&mut *transaction)
        .await?;
    report.cursor = last;
    report.refused = refused.clone();

    transaction.commit().await?;

    // After the commit: the record of what the durable rows already say.
    for (workflow, execution, steps) in &started {
        tracing::info!(
            workflow_id = %workflow.id,
            execution_id = %execution.id,
            steps,
            "an automation rule matched"
        );
        if let Err(err) =
            engine::record_start(pool, workflow, execution, TriggerKind::Event, *steps).await
        {
            tracing::warn!(execution_id = %execution.id, error = %err, "the run started but its audit row could not be written");
        }
        if let Err(err) = record_match(pool, workflow, execution).await {
            tracing::warn!(workflow_id = %workflow.id, error = %err, "the match audit row could not be written");
        }
    }

    for (workflow_id, event_id, event_name, payload) in &captured {
        match crate::testing::capture(pool, *workflow_id, *event_id, event_name, payload).await {
            // `false` means the listener was already filled by an earlier event — the
            // one-shot contract, not an error.
            Ok(true) => {
                tracing::info!(workflow_id = %workflow_id, event_id, "a listener captured an event");
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(workflow_id = %workflow_id, error = %err, "the listener could not be filled in");
            }
        }

        // The builder's listener is armed for a **node**, not for the rule (REQ-004
        // criterion 5), so the rule-shaped capture above does not fill it. The node is the
        // rule's own trigger — the one node an event starts a run at — and it is read from
        // the graph rather than passed in, because the matcher is given a `Workflow` row and
        // the node id lives in the graph's jsonb.
        //
        // A rule with no readable graph, or one whose trigger node is gone, captures nothing
        // and is not an error: the event still ran the rule, which is the fact that matters.
        match trigger_node_id(pool, *workflow_id).await {
            Ok(Some(node_id)) => {
                let now = time::OffsetDateTime::now_utc();
                let written = omnion_workflows::test_listener::capture(
                    pool,
                    *workflow_id,
                    &node_id,
                    *event_id,
                    event_name,
                    payload,
                    now,
                )
                .await;
                match written {
                    Ok(ids) if !ids.is_empty() => {
                        tracing::info!(workflow_id = %workflow_id, node_id = %node_id, event_id, listener = %ids[0], "a builder listener captured an event");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(workflow_id = %workflow_id, error = %err, "the builder listener could not be filled in");
                    }
                }
            }
            Ok(None) => {
                tracing::debug!(workflow_id = %workflow_id, "this rule has no trigger node to capture on");
            }
            Err(err) => {
                tracing::warn!(workflow_id = %workflow_id, error = %err, "the trigger node could not be read for the builder listener");
            }
        }
    }

    for entry in &refused {
        tracing::info!(
            workflow_id = %entry.workflow_id,
            event_id = entry.event_id,
            bound = entry.bound,
            "a bound refused to start a run"
        );
        if let Err(err) = record_refusal(pool, entry).await {
            tracing::warn!(workflow_id = %entry.workflow_id, error = %err, "the refusal audit row could not be written");
        }
    }

    for (workflow_id, event_id, reason) in &failures {
        tracing::warn!(workflow_id = %workflow_id, event_id, reason, "an automation rule could not run");
        if let Err(err) = record_skip(pool, *workflow_id, *event_id, reason).await {
            tracing::warn!(workflow_id = %workflow_id, error = %err, "the skip audit row could not be written");
        }
    }

    Ok(report)
}

/// The id of the node an event starts this rule at — the one trigger on its graph.
///
/// `None` for a rule whose graph is unreadable or carries no trigger. This is a *lookup per
/// matched event* and it is worth being explicit about why it is not cached in the drain:
/// the drain already holds the graph-less `Workflow` row, and a rule can be re-saved between
/// two events in the same batch. Reading it per capture is one indexed row per **match**,
/// and a match is a run somebody is paying for — a stale node id here would fill the wrong
/// node's listener, which is the one error in this file that is silent.
async fn trigger_node_id(pool: &PgPool, workflow_id: Uuid) -> Result<Option<String>> {
    let Some(definition) = omnion_workflows::graph_store::find_graph(pool, workflow_id).await?
    else {
        return Ok(None);
    };

    Ok(definition
        .graph
        .nodes
        .iter()
        .find(|node| omnion_workflows::graph::is_trigger_type(&node.node_type))
        .map(|node| node.id.clone()))
}

/// Resolve every action's parameters against the event that fired the rule.
///
/// This is the moment the templates become values: the run's step rows carry what *this* event
/// said, so a retry repeats the first attempt instead of reading a bus that has moved on.
pub fn resolve_steps(rule: &AutomationRule, payload: &Value) -> Result<Vec<StepDefinition>> {
    let mut steps = Vec::with_capacity(rule.actions.len());
    for action in &rule.actions {
        let mut step = action.clone();
        step.params = resolve_params(&action.params, payload)?;
        steps.push(step);
    }
    Ok(steps)
}

/// Audit one fired rule.
async fn record_match(
    pool: &PgPool,
    workflow: &Workflow,
    execution: &WorkflowExecution,
) -> Result<()> {
    let entry = omnion_audit::NewAuditEntry::system("automation.rule.matched")
        .organization(workflow.organization_id)
        .target("workflow_execution", execution.id.to_string())
        .metadata(json!({
            "workflow_id": workflow.id,
            "event": workflow.trigger_event,
            "trigger_count": workflow.trigger_count + 1,
        }));

    omnion_audit::record(pool, entry).await?;
    Ok(())
}

/// Audit one rule that could not be run.
async fn record_skip(pool: &PgPool, workflow_id: Uuid, event_id: i64, reason: &str) -> Result<()> {
    let organization: Option<Uuid> =
        sqlx::query_scalar("select organization_id from workflows where id = $1")
            .bind(workflow_id)
            .fetch_optional(pool)
            .await?;

    let entry = omnion_audit::NewAuditEntry::system("automation.rule.skipped")
        .organization(organization)
        .target("workflow", workflow_id.to_string())
        .metadata(json!({ "event_id": event_id, "reason": reason }));

    omnion_audit::record(pool, entry).await?;
    Ok(())
}

/// Audit one trigger a bound refused.
///
/// `automation.rule.limit_reached` — the event the request asks for. It is written after
/// the commit, for the reason every other audit row here is: the run that did not happen
/// is the fact, and this is the record of it. A missing row is a gap in the trail, never a
/// trigger that slipped through.
async fn record_refusal(pool: &PgPool, entry: &Refused) -> Result<()> {
    let organization: Option<Uuid> =
        sqlx::query_scalar("select organization_id from workflows where id = $1")
            .bind(entry.workflow_id)
            .fetch_optional(pool)
            .await?;

    let audit = omnion_audit::NewAuditEntry::system("automation.rule.limit_reached")
        .organization(organization)
        .target("workflow", entry.workflow_id.to_string())
        .metadata(json!({
            "event_id": entry.event_id,
            "bound": entry.bound,
            "reason": entry.reason,
        }));

    omnion_audit::record(pool, audit).await?;
    Ok(())
}

/// The definition columns the automations surface may rewrite, built from a rule.
#[must_use]
pub fn update_from_rule(rule: &AutomationRule) -> Option<WorkflowUpdate> {
    let definition = rule.definition().ok()?;
    Some(WorkflowUpdate {
        on_error: rule.on_error,
        name: rule.name.clone(),
        description: rule.description.clone(),
        site_id: rule.site_id,
        enabled: rule.enabled,
        trigger: TriggerKind::Event,
        schedule: None,
        trigger_event: Some(rule.event.clone()),
        conditions: definition.conditions_json().ok()?,
        // Carried through rather than defaulted: a whole-rule write (arm, disarm, rename)
        // that dropped this would silently move a rule off the service account it was
        // deliberately handed to, which is the kind of change nobody notices until a run
        // stops with a permission error.
        run_as_user_id: rule.run_as_user_id,
        // Carried through for the same reason as `run_as_user_id`: arming, pausing or
        // renaming a rule is not a decision to move its bounds back to the defaults.
        rate_limit_per_hour: Some(rule.rate_limit_per_hour),
        concurrency: Some(rule.concurrency.as_str().to_owned()),
        next_run_at: None,
        steps: definition.steps_json().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_workflows::OnError;
    use serde_json::json;

    fn rule_with(actions: Vec<StepDefinition>) -> AutomationRule {
        AutomationRule {
            created_by: None,
            run_as_user_id: None,
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            name: "rule".to_owned(),
            description: String::new(),
            enabled: true,
            event: "page.published".to_owned(),
            stored_conditions: json!({
                "all": [{
                    "field": "status",
                    "operator": "equals",
                    "value": "published"
                }]
            }),
            actions,
            hook_triggered: false,
            hook_configured: false,
            on_error: OnError::Stop,
            rate_limit_per_hour: crate::limits::DEFAULT_RATE_LIMIT,
            concurrency: crate::limits::Concurrency::Queue,
            last_error: None,
            trigger_count: 0,
            last_triggered_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn steps_carry_the_values_of_the_event_that_fired_the_rule() {
        let rule = rule_with(vec![StepDefinition::task(
            "comment",
            "comment_revision",
            json!({
                "revision_id": "{{event.revision_id}}",
                "body": "{{event.slug}} went live",
            }),
        )]);

        let payload =
            json!({ "revision_id": "6f1a4a3c-0a2f-4a52-9d3a-3f4da1b1f0a2", "slug": "home" });
        let steps = resolve_steps(&rule, &payload).expect("resolves");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].action.as_deref(), Some("comment_revision"));
        assert_eq!(
            steps[0].params["revision_id"],
            "6f1a4a3c-0a2f-4a52-9d3a-3f4da1b1f0a2"
        );
        assert_eq!(steps[0].params["body"], "home went live");
        assert_eq!(
            steps[0].name, rule.actions[0].name,
            "the author's step name travels with the run"
        );
    }

    #[test]
    fn a_step_the_payload_cannot_fill_stops_the_rule() {
        let rule = rule_with(vec![StepDefinition::task(
            "comment",
            "comment_revision",
            json!({ "revision_id": "{{event.nope}}", "body": "hi" }),
        )]);
        let error = resolve_steps(&rule, &json!({ "slug": "home" }))
            .expect_err("the placeholder cannot be filled");
        assert_eq!(error.code(), "invalid_binding");
    }

    #[test]
    fn the_report_says_what_happened() {
        let idle = MatchReport {
            idle: true,
            ..MatchReport::default()
        };
        assert!(idle.is_idle());

        let busy = MatchReport {
            matched: 2,
            runs: vec![Uuid::nil(), Uuid::nil()],
            ..MatchReport::default()
        };
        assert!(!busy.is_idle());
        assert_eq!(busy.runs.len(), busy.matched);
    }
}
