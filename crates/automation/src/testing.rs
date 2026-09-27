//! Test fire: a dry run against a hand-written payload, and a one-shot listener for the next
//! real event a rule matches.
//!
//! Both answer the same question — "would this rule do what I think it does?" — from
//! opposite ends, and neither of them touches the world:
//!
//! * **A dry run** evaluates a payload the author typed against the rule's conditions and
//!   its actions, and reports what each action **would** do. Host actions are simulated:
//!   the report says `would_send` with the recipient and subject, and no SMTP connection is
//!   opened, no page is published, no URL is called. That is what makes it safe to press
//!   "Send test event" on a rule that is already armed.
//! * **A listener** is armed for the next matching real event. It stores nothing until one
//!   arrives; the matcher fills the row in and the panel shows the payload that actually
//!   came — which is the only honest way to learn a payload shape the documentation got
//!   wrong. One shot: the first match fills it and the listener is done.
//!
//! Dry runs are written to `automation_test_events` so the panel can re-read a report after
//! a reload, and pruned after 24 hours by the caller that reads them (the same window the
//! request names) — a test report is not a record worth keeping forever.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AutomationError, Result};
use crate::model::AutomationRule;

/// Longest a test report or a captured payload may be.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// How long a test row stays readable.
pub const REPORT_TTL: time::Duration = time::Duration::hours(24);

/// What kind of row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    /// A dry run against a hand-written payload.
    Test,
    /// A one-shot listener waiting for the next real event.
    Listen,
}

impl TestKind {
    /// Canonical name stored in `automation_test_events.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Listen => "listen",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "test" => Some(Self::Test),
            "listen" => Some(Self::Listen),
            _ => None,
        }
    }
}

/// One comparison the dry run evaluated, and what it answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConditionReport {
    /// The comparison as the author wrote it, so the row is readable on its own.
    pub condition: Value,
    /// `true` when the payload satisfied it.
    pub holds: bool,
    /// The payload's value for the field, when the payload had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<Value>,
}

/// One group of the tree the dry run evaluated, and what it answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupReport {
    /// `all` or `any`.
    pub mode: &'static str,
    /// What the whole group answered.
    pub holds: bool,
    /// The members, in order.
    pub nodes: Vec<NodeReport>,
}

/// One member of a group: a comparison or a nested group.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum NodeReport {
    /// A single comparison and its answer.
    Comparison(ConditionReport),
    /// A nested group and its answer.
    Group(GroupReport),
}

/// What one action of a dry run would have done.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionReport {
    /// Step name, so the report lines up with the editor.
    pub name: String,
    /// The action key.
    pub action: String,
    /// `true` when the action touches the world (an email, a comment) — and therefore
    /// `true` when this report is a simulation rather than a run.
    pub host: bool,
    /// `would_send`, `would_call`, `would_run`… — what the step would do, in words.
    pub outcome: String,
    /// The parameters after the payload was resolved into them.
    pub params: Value,
    /// A readable one-line summary ("to editor@example.com, subject …"), when the action
    /// has a natural one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The whole dry-run report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRunReport {
    /// `true` when the rule's conditions held and the actions would have run.
    pub would_run: bool,
    /// Why the rule would not run, when it would not — the same sentence the matcher records.
    pub reason: Option<String>,
    /// The condition tree, answered row by row.
    pub conditions: GroupReport,
    /// The actions, in order, with their resolved parameters.
    pub actions: Vec<ActionReport>,
    /// Nothing in this report was sent, published or called.
    pub simulated: bool,
}

/// Evaluate a rule against a payload without touching anything.
///
/// This is the *only* place a dry run's evaluation lives, and it deliberately shares
/// [`crate::groups`] and [`crate::binding`] with the matcher: a dry run that evaluated
/// conditions any other way would answer a different question than the one that matters.
#[must_use]
pub fn dry_run(rule: &AutomationRule, payload: &Value) -> DryRunReport {
    let Ok(group) = crate::groups::read_tree(&rule.stored_conditions) else {
        return unreadable_report();
    };

    let holds = group.holds(payload);
    let conditions = report_group(&group, payload);

    let actions = rule
        .actions
        .iter()
        .map(|action| {
            // A binding the payload cannot fill is a definition problem, not a reason to
            // fail the report: the unresolved parameters are shown as the definition
            // carries them, so the author sees *which* binding is wrong.
            let params = crate::binding::resolve_params(&action.params, payload)
                .unwrap_or_else(|_| action.params.clone());
            let key = action.action.as_deref().unwrap_or("noop");
            ActionReport {
                name: action.name.clone(),
                action: key.to_owned(),
                host: omnion_workflows::actions::is_host_action(key),
                outcome: outcome_for(key),
                summary: summary_for(key, &params),
                params,
            }
        })
        .collect();

    DryRunReport {
        would_run: holds,
        reason: (!holds).then(|| "the conditions did not hold against this payload".to_owned()),
        conditions,
        actions,
        simulated: true,
    }
}

/// The report for a rule whose stored conditions cannot be read at all.
fn unreadable_report() -> DryRunReport {
    DryRunReport {
        would_run: false,
        reason: Some("the stored conditions are not readable".to_owned()),
        conditions: GroupReport {
            mode: "all",
            holds: false,
            nodes: Vec::new(),
        },
        actions: Vec::new(),
        simulated: true,
    }
}

/// What an action *would* do, in one word.
fn outcome_for(action: &str) -> String {
    match action {
        "send_email" => "would_send",
        "comment_revision" => "would_comment",
        "http_request" => "would_call",
        "publish_page" => "would_publish",
        "wait_for_approval" => "would_wait",
        "run_workflow" => "would_run",
        "fail" => "would_fail",
        "wait" => "would_wait",
        _ => "would_run",
    }
    .to_owned()
}

/// The one-line summary an action's parameters suggest.
fn summary_for(action: &str, params: &Value) -> Option<String> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    match action {
        "send_email" => {
            let to = text("to")?;
            let subject = text("subject");
            Some(match subject {
                Some(subject) => format!("to {to}, subject “{subject}”"),
                None => format!("to {to}"),
            })
        }
        "http_request" => {
            let url = text("url")?;
            let method = text("method").unwrap_or_else(|| "POST".to_owned());
            Some(format!("{method} {url}"))
        }
        "comment_revision" => text("body").map(|body| format!("comment “{body}”")),
        "publish_page" => text("page_id").map(|page| format!("publish page {page}")),
        "wait_for_approval" => Some("parks the run for a decision".to_owned()),
        "run_workflow" => text("workflow_id").map(|id| format!("start workflow {id}")),
        "wait" => params
            .get("seconds")
            .and_then(Value::as_i64)
            .map(|seconds| format!("wait {seconds}s")),
        _ => None,
    }
}

/// Answer one group row by row, for the panel's per-row display.
fn report_group(group: &crate::groups::ConditionGroup, payload: &Value) -> GroupReport {
    GroupReport {
        mode: group.mode().as_str(),
        holds: group.holds(payload),
        nodes: group
            .nodes()
            .iter()
            .map(|node| match node {
                crate::groups::ConditionNode::Comparison(comparison) => {
                    let found =
                        crate::condition::resolve_field(payload, &comparison.field).cloned();
                    NodeReport::Comparison(ConditionReport {
                        condition: serde_json::to_value(comparison).unwrap_or(Value::Null),
                        holds: comparison.holds(payload),
                        found,
                    })
                }
                crate::groups::ConditionNode::Group(inner) => {
                    NodeReport::Group(report_group(inner, payload))
                }
            })
            .collect(),
    }
}

/// Check a hand-written test payload before it is evaluated.
pub fn validate_payload(payload: &Value) -> Result<()> {
    if !payload.is_object() {
        return Err(AutomationError::invalid(
            "invalid_test_payload",
            "a test payload is a JSON object, e.g. {\"status\": \"published\"}",
        ));
    }
    let size = serde_json::to_string(payload)
        .map(|text| text.len())
        .unwrap_or(0);
    if size > MAX_PAYLOAD_BYTES {
        return Err(AutomationError::invalid(
            "invalid_test_payload",
            format!("a test payload is at most {MAX_PAYLOAD_BYTES} bytes, got {size}"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The rows: a dry-run report and an armed listener
// ---------------------------------------------------------------------------------------------

/// One stored test row.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TestEvent {
    /// Row id.
    pub id: Uuid,
    /// Organization that owns the rule.
    pub organization_id: Uuid,
    /// Rule the row belongs to.
    pub workflow_id: Uuid,
    /// `test` or `listen`.
    pub kind: String,
    /// The payload: the hand-written one, or the captured one once a listener fired.
    pub payload: Option<Value>,
    /// Bus event id, when a listener captured one.
    pub event_id: Option<i64>,
    /// Event name, when a listener captured one.
    pub event_name: Option<String>,
    /// Account that armed the row.
    pub created_by: Option<Uuid>,
    /// When the row was written.
    pub created_at: OffsetDateTime,
    /// When a listener filled in.
    pub captured_at: Option<OffsetDateTime>,
}

impl TestEvent {
    /// What kind of row this is.
    #[must_use]
    pub fn test_kind(&self) -> Option<TestKind> {
        TestKind::parse(&self.kind)
    }

    /// `true` when a listener is armed and has not fired yet.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.kind == TestKind::Listen.as_str() && self.payload.is_none()
    }
}

/// Store a dry-run report for a rule.
pub async fn record_test(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    payload: &Value,
    created_by: Uuid,
) -> Result<TestEvent> {
    let row: TestEvent = sqlx::query_as(
        "insert into automation_test_events \
             (organization_id, workflow_id, kind, payload, created_by) \
         values ($1, $2, 'test', $3, $4) \
         returning id, organization_id, workflow_id, kind, payload, event_id, event_name, \
                   created_by, created_at, captured_at",
    )
    .bind(organization_id)
    .bind(workflow_id)
    .bind(payload)
    .bind(created_by)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Arm a one-shot listener for a rule's next matching event.
pub async fn arm_listener(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    created_by: Uuid,
) -> Result<TestEvent> {
    // One listener at a time: a second press replaces the first, so the panel never shows
    // two armed rows for a rule and the matcher only ever has one row to fill.
    sqlx::query(
        "delete from automation_test_events \
         where workflow_id = $1 and kind = 'listen' and payload is null",
    )
    .bind(workflow_id)
    .execute(pool)
    .await?;

    let row: TestEvent = sqlx::query_as(
        "insert into automation_test_events \
             (organization_id, workflow_id, kind, created_by) \
         values ($1, $2, 'listen', $3) \
         returning id, organization_id, workflow_id, kind, payload, event_id, event_name, \
                   created_by, created_at, captured_at",
    )
    .bind(organization_id)
    .bind(workflow_id)
    .bind(created_by)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// A rule's test rows, newest first.
pub async fn list_for_workflow(
    pool: &PgPool,
    workflow_id: Uuid,
    limit: i64,
) -> Result<Vec<TestEvent>> {
    let rows = sqlx::query_as::<_, TestEvent>(
        "select id, organization_id, workflow_id, kind, payload, event_id, event_name, \
                created_by, created_at, captured_at \
         from automation_test_events where workflow_id = $1 \
         order by created_at desc, id desc limit $2",
    )
    .bind(workflow_id)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Fill a listener in with the event that fired it, if it is still armed.
///
/// The `payload is null` predicate is what makes the listener one-shot under concurrency:
/// two matchers racing the same event both call this, and only the first one updates a row.
pub async fn capture(
    pool: &PgPool,
    workflow_id: Uuid,
    event_id: i64,
    event_name: &str,
    payload: &Value,
) -> Result<bool> {
    let captured = sqlx::query(
        "update automation_test_events \
         set payload = $2, event_id = $3, event_name = $4, captured_at = now() \
         where workflow_id = $1 and kind = 'listen' and payload is null",
    )
    .bind(workflow_id)
    .bind(payload)
    .bind(event_id)
    .bind(event_name)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(captured > 0)
}

/// Drop test rows older than the report window.
pub async fn prune(pool: &PgPool, now: OffsetDateTime) -> Result<u64> {
    let removed = sqlx::query("delete from automation_test_events where created_at < $1")
        .bind(now - REPORT_TTL)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::{Condition, ConditionOperator};
    use crate::groups::{ConditionGroup, ConditionNode};
    use omnion_workflows::definition::StepDefinition;
    use serde_json::json;
    use time::OffsetDateTime as Offset;

    fn rule_with(conditions: Value, actions: Vec<StepDefinition>) -> AutomationRule {
        AutomationRule {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            name: "rule".to_owned(),
            description: String::new(),
            enabled: true,
            event: "page.published".to_owned(),
            stored_conditions: conditions,
            actions,
            hook_triggered: false,
            hook_configured: false,
            trigger_count: 0,
            last_triggered_at: None,
            created_at: Offset::UNIX_EPOCH,
            updated_at: Offset::UNIX_EPOCH,
        }
    }

    fn payload() -> Value {
        json!({
            "status": "published",
            "slug": "home",
            "revision_id": "6f1a4a3c-0a2f-4a52-9d3a-3f4da1b1f0a2",
            "title": "Release notes",
        })
    }

    fn mail_step() -> StepDefinition {
        StepDefinition::task(
            "tell the editor",
            "send_email",
            json!({
                "to": "editor@example.com",
                "subject": "Published: {{event.title}}",
                "body": "{{event.slug}} is live.",
            }),
        )
    }

    #[test]
    fn a_dry_run_resolves_the_payload_into_every_action() {
        let rule = rule_with(json!({ "all": [] }), vec![mail_step()]);
        let report = dry_run(&rule, &payload());

        assert!(report.would_run, "no conditions always hold");
        assert!(report.simulated, "a dry run is always a simulation");
        assert_eq!(report.actions.len(), 1);

        let action = &report.actions[0];
        assert_eq!(action.action, "send_email");
        assert!(action.host);
        assert_eq!(action.outcome, "would_send");
        // The bindings resolved against the payload the author wrote — the same resolution
        // the matcher performs, so the report is the run that did not happen.
        assert_eq!(action.params["subject"], "Published: Release notes");
        assert_eq!(action.params["body"], "home is live.");
        assert_eq!(action.params["to"], "editor@example.com");
        let summary = action.summary.as_deref().expect("a summary line");
        assert!(summary.contains("editor@example.com"), "{summary}");
        assert!(summary.contains("Release notes"), "{summary}");
    }

    #[test]
    fn a_dry_run_reports_each_condition_row_against_the_payload() {
        let group = ConditionGroup::any(vec![
            ConditionNode::Comparison(Condition::compare(
                "status",
                ConditionOperator::Equals,
                json!("draft"),
            )),
            ConditionNode::Group(ConditionGroup::all(vec![ConditionNode::Comparison(
                Condition::compare("status", ConditionOperator::Equals, json!("published")),
            )])),
        ]);
        let rule = rule_with(crate::groups::to_json(&group), vec![mail_step()]);

        let report = dry_run(&rule, &payload());
        assert!(report.would_run);
        assert_eq!(report.conditions.mode, "any");
        assert!(report.conditions.holds);
        assert_eq!(report.conditions.nodes.len(), 2);

        // The first row is answered false, the nested group true.
        let NodeReport::Comparison(first) = &report.conditions.nodes[0] else {
            panic!("a comparison row");
        };
        assert!(!first.holds);
        assert_eq!(first.found, Some(json!("published")));
        let NodeReport::Group(nested) = &report.conditions.nodes[1] else {
            panic!("a group row");
        };
        assert!(nested.holds);
    }

    #[test]
    fn a_dry_run_whose_conditions_do_not_hold_says_why_and_runs_nothing() {
        let group = ConditionGroup::all(vec![ConditionNode::Comparison(Condition::compare(
            "status",
            ConditionOperator::Equals,
            json!("archived"),
        ))]);
        let rule = rule_with(crate::groups::to_json(&group), vec![mail_step()]);

        let report = dry_run(&rule, &payload());
        assert!(!report.would_run);
        assert!(report.reason.is_some_and(|why| why.contains("conditions")));
        // The actions are still reported — the author wants to see what *would* have run.
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].outcome, "would_send");
    }

    #[test]
    fn a_dry_run_never_claims_it_sent_anything() {
        let rule = rule_with(json!([]), vec![mail_step()]);
        for action in dry_run(&rule, &payload()).actions {
            assert!(
                action.outcome.starts_with("would_"),
                "{} claims {}",
                action.action,
                action.outcome
            );
        }
    }

    #[test]
    fn an_unreadable_stored_tree_never_reports_a_run() {
        let rule = rule_with(json!("status = published"), vec![mail_step()]);
        let report = dry_run(&rule, &payload());
        assert!(!report.would_run);
        assert!(
            report
                .reason
                .is_some_and(|why| why.contains("not readable"))
        );
    }

    #[test]
    fn a_binding_the_payload_cannot_fill_is_reported_rather_than_panicking() {
        let rule = rule_with(
            json!([]),
            vec![StepDefinition::task(
                "comment",
                "comment_revision",
                json!({ "revision_id": "{{event.nope}}", "body": "live" }),
            )],
        );
        // The dry run must answer, not blow up: the unresolved parameters are shown as the
        // definition carries them, so the author sees which binding is the problem.
        let report = dry_run(&rule, &payload());
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].params["revision_id"], "{{event.nope}}");
    }

    #[test]
    fn a_test_payload_must_be_an_object_and_bounded() {
        assert!(validate_payload(&json!({ "a": 1 })).is_ok());
        assert_eq!(
            validate_payload(&json!("nope")).expect_err("scalar").code(),
            "invalid_test_payload"
        );
        assert_eq!(
            validate_payload(&json!([1, 2, 3]))
                .expect_err("list")
                .code(),
            "invalid_test_payload"
        );
        let big = json!({ "blob": "x".repeat(MAX_PAYLOAD_BYTES + 10) });
        assert_eq!(
            validate_payload(&big).expect_err("too big").code(),
            "invalid_test_payload"
        );
    }

    #[test]
    fn the_test_kinds_round_trip() {
        assert_eq!(TestKind::Test.as_str(), "test");
        assert_eq!(TestKind::Listen.as_str(), "listen");
        assert_eq!(TestKind::parse("listen"), Some(TestKind::Listen));
        assert_eq!(TestKind::parse("other"), None);
    }
}
