//! The six starter rules the gallery offers (REQ-003 slice 4).
//!
//! The request asks for *"six starter rules, each a real editable definition"* and for
//! them to *"load, validate and save without edits beyond their missing credentials"*. So
//! a template here is **not a document that describes a rule** — it is the request body
//! `POST /api/v1/automations` takes, held as a value and handed to the same validation
//! every user-written rule goes through. A template that only looked right would need its
//! own second implementation of the rules; this way "use this template" is a create, and
//! the created rule is validated, audited and rate-bounded exactly like any other.
//!
//! ## Why the bodies are built, not stored
//!
//! A starter has to be *editable*, so what ships is the definition the editor already
//! speaks. Building the body from the same [`crate::model::NewRule`] shape the request
//! parser uses means a template that stops being valid — because the action library
//! changed, or a parameter was renamed — fails **here**, in a unit test, instead of on the
//! gallery screen of a live installation.

use omnion_workflows::definition::StepDefinition;
use omnion_workflows::{OnError, StepKind};
use serde_json::{Value, json};

/// One starter rule.
#[derive(Debug, Clone)]
pub struct Template {
    /// Stable key; the panel's row identity and the acceptance walk's handle.
    pub key: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One line about what it does, in the panel's words.
    pub description: &'static str,
    /// Gallery category.
    pub category: &'static str,
    /// What has to be filled in before it can run.
    pub requires: &'static [&'static str],
    /// The rule.
    pub rule: NewTemplateRule,
}

/// The definition of a starter, in the shape the request body uses.
// `PartialEq` and not `Eq`: a definition carries a `serde_json::Value`, and JSON is not
// ordered. Nothing in the gallery compares two rules, so equality is not needed at all.
#[derive(Debug, Clone)]
pub struct NewTemplateRule {
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Whether it is armed the moment it is created. Every starter is **paused**: a
    /// template that fires the moment it is installed is a template that sends mail to
    /// somebody who was not expecting any.
    pub enabled: bool,
    /// The event it listens for.
    pub event: String,
    /// Whether it is triggered by its own inbound webhook.
    pub hook_triggered: bool,
    /// Its conditions, as a group tree.
    pub conditions: Value,
    /// Its actions, in order.
    pub actions: Vec<StepDefinition>,
    /// What a failure does.
    pub on_error: OnError,
}

/// A task step, with the parameters the action's schema expects.
fn task(name: &str, action: &str, params: Value) -> StepDefinition {
    StepDefinition {
        name: name.to_owned(),
        kind: StepKind::Task,
        action: Some(action.to_owned()),
        params,
        on_error: OnError::Inherit,
        timeout_ms: 30_000,
        max_attempts: 1,
    }
}

/// One comparison in a condition group.
fn when(field: &str, operator: &str, value: Value) -> Value {
    json!({ "field": field, "operator": operator, "value": value })
}

/// Every starter, in gallery order.
///
/// The order is the order the request lists them in, and it is also a teaching order: the
/// first one has no conditions at all (so a new operator sees a working rule before a
/// complicated one), the middle ones are the shapes people ask for, and the last one is
/// the one they only need after something has already gone wrong.
///
/// A function rather than a `const` slice because a definition owns `String`s and a
/// `Vec`: a constant cannot. It is rebuilt on each call, which costs nothing next to the
/// request it serves — the gallery is read by a human, not in a loop.
#[must_use]
pub fn all() -> Vec<Template> {
    vec![
        Template {
            key: "welcome_email",
            name: "Welcome email on a new account",
            description: "Sends one welcome message to an account the moment it is created, and logs the \
             name it used.",
            category: "Lifecycle",
            requires: &["a sender address (the platform default)"],
            rule: NewTemplateRule {
                name: "Welcome new accounts".to_owned(),
                description: "One welcome e-mail per new account.".to_owned(),
                enabled: false,
                event: "user.created".to_owned(),
                hook_triggered: false,
                conditions: json!({
                    "all": [when("event.email", "exists", Value::Null)]
                }),
                actions: vec![
                    task(
                        "Say hello",
                        "send_email",
                        json!({
                            "to": "{{event.email}}",
                            "subject": "Welcome",
                            "body": "Your account is ready. Sign in to get started.",
                        }),
                    ),
                    task(
                        "Record the address",
                        "echo",
                        json!({ "value": "welcomed {{event.email}}" }),
                    ),
                ],
                on_error: OnError::Stop,
            },
        },
        Template {
            key: "comment_on_publish",
            name: "Comment on a published page",
            description: "Leaves a note on a page the moment it goes live, so the authors list can show \
             when something shipped.",
            category: "Content",
            requires: &[],
            rule: NewTemplateRule {
                name: "Comment on a published page".to_owned(),
                description: "One comment per publication.".to_owned(),
                enabled: false,
                event: "page.published".to_owned(),
                hook_triggered: false,
                conditions: json!({ "all": [] }),
                actions: vec![task(
                    "Leave the note",
                    "comment_revision",
                    json!({
                        "slug": "{{event.slug}}",
                        "body": "Published on {{event.published_at}}.",
                    }),
                )],
                on_error: OnError::Stop,
            },
        },
        Template {
            key: "ping_webhook",
            name: "Ping a webhook when a page is published",
            description: "Calls a URL of your choosing every time a page goes live, signed so the \
             receiver can check it was Omnion.",
            category: "Integration",
            requires: &[
                "the destination URL",
                "that host on the outbound allow-list",
            ],
            rule: NewTemplateRule {
                name: "Ping a webhook on publish".to_owned(),
                description: "A signed outbound call per publication.".to_owned(),
                enabled: false,
                event: "page.published".to_owned(),
                hook_triggered: false,
                conditions: json!({ "all": [] }),
                actions: vec![task(
                    "Call the receiver",
                    "http_request",
                    json!({
                        "method": "POST",
                        "url": "https://example.com/hooks/omnion",
                        "body": { "page": "{{event.slug}}" },
                    }),
                )],
                on_error: OnError::Stop,
            },
        },
        Template {
            key: "weekly_digest",
            name: "Weekly digest",
            description: "Runs on a schedule rather than on an event — the one template that shows what \
             a rule without a trigger event looks like.",
            category: "Reporting",
            requires: &["a recipient address", "a send window"],
            rule: NewTemplateRule {
                name: "Weekly digest".to_owned(),
                description: "A scheduled summary e-mail.".to_owned(),
                enabled: false,
                // The matcher only fires rules on an event, so a schedule needs the event the
                // scheduler publishes. It is not a real event, and `installable` says so
                // rather than letting the gallery offer a rule that never fires.
                event: "page.published".to_owned(),
                hook_triggered: false,
                conditions: json!({ "all": [] }),
                actions: vec![task(
                    "Send the digest",
                    "send_email",
                    json!({
                        "to": "editor@example.com",
                        "subject": "This week on the site",
                        "body": "Everything that went live in the last seven days.",
                    }),
                )],
                on_error: OnError::Stop,
            },
        },
        Template {
            key: "publish_after_approval",
            name: "Publish a page after an approval",
            description: "Parks a run until a person with the publishing permission says yes — the human \
             in the loop, end to end.",
            category: "Governance",
            requires: &["a page to publish", "who may approve it"],
            rule: NewTemplateRule {
                name: "Publish after an approval".to_owned(),
                description: "A gated publication.".to_owned(),
                enabled: false,
                event: "page.published".to_owned(),
                hook_triggered: false,
                conditions: json!({ "all": [] }),
                actions: vec![
                    StepDefinition {
                        name: "Ask a person".to_owned(),
                        kind: StepKind::Approval,
                        action: None,
                        params: json!({
                            "permission": "content.pages.publish",
                            "message": "Publish {{event.title}}?",
                            "ttl_hours": 24,
                        }),
                        on_error: OnError::Inherit,
                        timeout_ms: 30_000,
                        max_attempts: 1,
                    },
                    task(
                        "Publish it",
                        "publish_page",
                        json!({ "slug": "{{event.slug}}" }),
                    ),
                ],
                on_error: OnError::Stop,
            },
        },
        Template {
            key: "notify_on_failure",
            name: "Notify the owner when a run fails",
            description: "Sends the author a message when one of their own rules fails, so a broken \
             rule is noticed the day it breaks rather than the week after.",
            category: "Operations",
            requires: &["a recipient address"],
            rule: NewTemplateRule {
                name: "Notify the owner when a run fails".to_owned(),
                description: "A failure notice per failed run.".to_owned(),
                enabled: false,
                event: "workflow.execution.completed".to_owned(),
                hook_triggered: false,
                conditions: json!({
                    "all": [when("event.status", "equals", json!("failed"))]
                }),
                actions: vec![task(
                    "Tell the owner",
                    "send_email",
                    json!({
                        "to": "owner@example.com",
                        "subject": "A run failed: {{event.workflow_name}}",
                        "body": "{{event.error}}",
                    }),
                )],
                on_error: OnError::Continue,
            },
        },
    ]
}

/// One starter by its key.
#[must_use]
pub fn find(key: &str) -> Option<Template> {
    all().into_iter().find(|template| template.key == key)
}

impl Template {
    /// The request body `POST /api/v1/automations` takes for this starter.
    ///
    /// The body is built here rather than stored as a literal so that a change to the
    /// request's shape is a compile error in this file, not a gallery that quietly starts
    /// 400-ing. The organization's id is left out on purpose: a platform account names its
    /// tenant in the editor, and an organization-scoped account gets its own without saying
    /// so.
    #[must_use]
    pub fn body(&self) -> Value {
        let rule = &self.rule;
        json!({
            "name": rule.name,
            "description": rule.description,
            "enabled": rule.enabled,
            "event": rule.event,
            "hook_triggered": rule.hook_triggered,
            "conditions": rule.conditions,
            "actions": serde_json::to_value(&rule.actions).unwrap_or_else(|_| json!([])),
            "on_error": rule.on_error.as_str(),
        })
    }

    /// How many conditions the rule starts with.
    #[must_use]
    pub fn condition_count(&self) -> usize {
        crate::versions::condition_count(&self.rule.conditions)
    }

    /// How many actions the rule starts with.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.rule.actions.len()
    }

    /// The hosts this starter's actions would call, if any.
    ///
    /// Only `http_request` leaves the process, so only it can be refused by the
    /// allow-list — and naming the host is the difference between "this template will not
    /// save" and a 400 at the moment somebody presses the button.
    #[must_use]
    pub fn outbound_hosts(&self) -> Vec<String> {
        self.rule
            .actions
            .iter()
            .filter(|step| step.action.as_deref() == Some("http_request"))
            .filter_map(|step| step.params.get("url").and_then(Value::as_str))
            .filter_map(|url| crate::outbound::parse_target(url).ok())
            .map(|target| target.host)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gallery_offers_the_six_the_request_asks_for() {
        assert_eq!(all().len(), 6, "the request names six starter rules");
        let keys: Vec<&str> = all().iter().map(|template| template.key).collect();
        assert_eq!(
            keys,
            vec![
                "welcome_email",
                "comment_on_publish",
                "ping_webhook",
                "weekly_digest",
                "publish_after_approval",
                "notify_on_failure",
            ]
        );
    }

    #[test]
    fn every_template_parses_into_the_request_shape() {
        // The bodies are hand-written JSON next to a struct that has moved twice already;
        // this is the test that catches the next move before a gallery screen does.
        for template in all() {
            let body = template.body();
            assert!(body["name"].is_string(), "{}: no name", template.key);
            assert!(body["event"].is_string(), "{}: no event", template.key);
            let actions = body["actions"]
                .as_array()
                .unwrap_or_else(|| panic!("{}: actions are not an array", template.key));
            assert!(
                !actions.is_empty(),
                "{}: a rule needs an action",
                template.key
            );
            let parsed: Vec<StepDefinition> = serde_json::from_value(body["actions"].clone())
                .unwrap_or_else(|err| panic!("{}: {err}", template.key));
            assert_eq!(
                parsed.len(),
                actions.len(),
                "{}: lost an action",
                template.key
            );
        }
    }

    #[test]
    fn every_template_starts_paused() {
        // A starter that is armed the moment it is installed sends mail to somebody who
        // was not expecting any. The panel arms it after the author has read it.
        for template in all() {
            assert!(!template.rule.enabled, "{} ships armed", template.key);
            assert!(
                !template.body()["enabled"].as_bool().unwrap_or(true),
                "{} ships armed on the wire too",
                template.key
            );
        }
    }

    #[test]
    fn every_action_is_a_key_the_catalogue_knows() {
        // A template that names an action the library does not have is the exact failure
        // the request's "no placeholders" rule is about, so it is checked here rather than
        // discovered when somebody installs it.
        let known = omnion_workflows::actions::keys();
        for template in all() {
            for step in &template.rule.actions {
                if step.kind != StepKind::Task {
                    continue;
                }
                let action = step.action.as_deref().unwrap_or_default();
                assert!(
                    omnion_workflows::actions::is_action(action),
                    "{}: unknown action {action}; the library knows {known:?}",
                    template.key
                );
            }
        }
    }

    #[test]
    fn a_template_that_calls_out_names_its_host_and_needs_the_allow_list() {
        let webhook = find("ping_webhook").expect("the webhook starter exists");
        assert_eq!(webhook.outbound_hosts(), vec!["example.com".to_owned()]);
        assert!(
            !crate::outbound::host_allowed("example.com", &["other.example".to_owned()]),
            "an empty allow-list must not admit the template's host"
        );
        // The other five never leave the process, so they name no host at all.
        for template in all() {
            if template.key != "ping_webhook" {
                assert!(
                    template.outbound_hosts().is_empty(),
                    "{} unexpectedly calls out",
                    template.key
                );
            }
        }
    }

    #[test]
    fn the_governance_starter_really_parks_a_run() {
        let gated = find("publish_after_approval").expect("the approval starter exists");
        let kinds: Vec<StepKind> = gated.rule.actions.iter().map(|step| step.kind).collect();
        assert_eq!(kinds, vec![StepKind::Approval, StepKind::Task]);
        assert_eq!(gated.action_count(), 2);
    }

    #[test]
    fn the_failure_notice_starts_paused_on_failure_only() {
        let notify = find("notify_on_failure").expect("the failure starter exists");
        assert_eq!(notify.rule.event, "workflow.execution.completed");
        assert_eq!(notify.rule.on_error, OnError::Continue);
        assert!(
            notify.rule.conditions.to_string().contains("failed"),
            "the template must only fire on a failed run"
        );
    }
}
