//! The built-in action set of the engine.
//!
//! v0 runs no user-supplied code (docs/09-N8N-TEARDOWN.md §13 lesson 14: no dynamic code in
//! the core process, ever). Two kinds of action exist and both are a closed set:
//!
//! * **synthetic actions** — no-op, echo, the always-failing step and the transient failure.
//!   The engine runs them itself; they are what the retry policy and the step machine are
//!   proven with, and they are the reason a definition can be exercised without any outside
//!   system.
//! * **host actions** — `send_email` and `comment_revision` (the first two real actions of the
//!   automation layer, docs/requests/REQ-003/P13). They touch the world (an SMTP server, the
//!   content store), so the engine never runs them itself: it hands them to the
//!   [`crate::ActionHandler`] the process installed. A definition may name them anywhere; a
//!   process without a handler fails the step with an honest message instead of pretending.
//!
//! HTTP and notification actions beyond these arrive with the integration hub (REQ-015).

use serde_json::Value;

use crate::error::{Result, WorkflowError};

/// One action the engine understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDef {
    /// Stable key used in a definition.
    pub key: &'static str,
    /// What it does, in product language.
    pub description: &'static str,
}

/// Actions the engine runs itself.
pub const ACTIONS: &[ActionDef] = &[
    ActionDef {
        key: "noop",
        description: "Does nothing and succeeds — a placeholder for the step order itself.",
    },
    ActionDef {
        key: "echo",
        description: "Copies its `value` parameter into the step output.",
    },
    ActionDef {
        key: "fail",
        description: "Always fails with its `message`; the way a terminal failure is modelled.",
    },
    ActionDef {
        key: "transient",
        description: "Fails its first `fail_times` attempts, then succeeds — proves retries.",
    },
];

/// Actions the host runs through its [`crate::ActionHandler`].
///
/// These are the automation layer's real actions: the engine knows their names and their
/// parameters (a definition is validated without a handler present), and refuses to run them
/// by itself. The three added by REQ-003 slice 2 are the ones that *leave the process* —
/// an outbound call, a publication and a chained rule — and each of them is bounded by
/// something the platform controls rather than by what the author typed.
pub const HOST_ACTIONS: &[ActionDef] = &[
    ActionDef {
        key: "send_email",
        description: "Sends one plain-text email through the configured SMTP server.",
    },
    ActionDef {
        key: "comment_revision",
        description: "Leaves a comment on a content revision.",
    },
    ActionDef {
        key: "http_request",
        description: "Calls an allowed host and signs the request with the rule's key.",
    },
    ActionDef {
        key: "publish_page",
        description: "Publishes a content page of this organization.",
    },
    ActionDef {
        key: "run_workflow",
        description: "Starts another rule's run as part of this one.",
    },
    ActionDef {
        key: "ai.prompt",
        description: "Sends one prompt template to a model and puts its answer in the step output.",
    },
];

/// Methods an outbound call may use.
///
/// The engine holds the list so a definition naming `TRACE` is refused when it is written;
/// the automation layer re-checks it against its own settings table at call time, because
/// the two answer different questions (is this a method at all / may this host be reached).
pub const OUTBOUND_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// Largest output budget one `ai.prompt` step may ask for.
///
/// A platform ceiling rather than a per-organization setting, for the same reason
/// `MAX_ANSWER_BYTES` is: a step that runs on a schedule multiplies every call, so an
/// unbounded budget is an unbounded bill. Ten thousand tokens answers the "summarise this,
/// rewrite that" prompts the action exists for, and a workflow that needs more is a
/// workflow that should be split into steps.
pub const MAX_AI_STEP_TOKENS: i64 = 10_000;

/// Every action key, in catalogue order (synthetic first, then host actions).
#[must_use]
pub fn keys() -> Vec<&'static str> {
    ACTIONS
        .iter()
        .chain(HOST_ACTIONS.iter())
        .map(|action| action.key)
        .collect()
}

/// `true` when the key names an action of the catalogue.
#[must_use]
pub fn is_action(key: &str) -> bool {
    ACTIONS.iter().any(|action| action.key == key)
        || HOST_ACTIONS.iter().any(|action| action.key == key)
}

/// `true` when the host runs this action, not the engine.
#[must_use]
pub fn is_host_action(key: &str) -> bool {
    HOST_ACTIONS.iter().any(|action| action.key == key)
}

/// Check the parameters of one action.
pub fn validate_params(action: &str, params: &Value) -> Result<()> {
    match action {
        "noop" => Ok(()),
        "echo" => {
            if params.get("value").is_none() {
                return Err(WorkflowError::invalid(
                    "invalid_step_params",
                    "the echo action needs a `value` parameter",
                ));
            }
            Ok(())
        }
        "fail" => Ok(()),
        "transient" => {
            let fail_times = params.get("fail_times").and_then(Value::as_i64);
            match fail_times {
                Some(n) if (1..=4).contains(&n) => Ok(()),
                Some(n) => Err(WorkflowError::invalid(
                    "invalid_step_params",
                    format!(
                        "the transient action fails 1 to 4 times (and at most the step's \
                         attempts), got {n}"
                    ),
                )),
                None => Err(WorkflowError::invalid(
                    "invalid_step_params",
                    "the transient action needs an integer `fail_times` parameter",
                )),
            }
        }
        "send_email" => {
            require_text(params, "send_email", "to")?;
            require_text(params, "send_email", "subject")?;
            require_text(params, "send_email", "body")?;
            Ok(())
        }
        "comment_revision" => {
            require_text(params, "comment_revision", "revision_id")?;
            require_text(params, "comment_revision", "body")?;
            Ok(())
        }
        // The shape check only. Whether the *host* is allowed is the automation layer's
        // answer, because the allow-list is a row in its settings table rather than a fact
        // the engine could know — a definition with a URL the engine cannot run is refused
        // when the rule is written, with the host named.
        "http_request" => {
            require_text(params, "http_request", "url")?;
            if let Some(method) = params.get("method").and_then(Value::as_str) {
                let method = method.trim().to_ascii_uppercase();
                if !OUTBOUND_METHODS.contains(&method.as_str()) {
                    return Err(WorkflowError::invalid(
                        "invalid_step_params",
                        format!(
                            "`{method}` is not a method a rule may call; use one of: {}",
                            OUTBOUND_METHODS.join(", ")
                        ),
                    ));
                }
            }
            Ok(())
        }
        "publish_page" => {
            require_text(params, "publish_page", "page_id")?;
            Ok(())
        }
        "run_workflow" => {
            require_text(params, "run_workflow", "workflow_id")?;
            Ok(())
        }
        // A template, a model key and a token budget. The budget is bounded **here** rather
        // than at call time because a definition is validated without a provider present: a
        // rule that asks for a million tokens per step is refused when it is written, not
        // when the event arrives. The ceiling is the platform's, not the author's, because a
        // step that can spend an unbounded amount is a step nobody can safely put on a
        // schedule.
        "ai.prompt" => {
            require_text(params, "ai.prompt", "prompt")?;
            if let Some(model) = params.get("model") {
                if !model.is_string() {
                    return Err(WorkflowError::invalid(
                        "invalid_step_params",
                        "the `model` of an ai.prompt step is a model key as text, not a value \
                         of another type",
                    ));
                }
            }
            if let Some(max_tokens) = params.get("max_tokens") {
                let max_tokens = max_tokens.as_i64().ok_or_else(|| {
                    WorkflowError::invalid(
                        "invalid_step_params",
                        "the `max_tokens` of an ai.prompt step is an integer",
                    )
                })?;
                if max_tokens < 1 || max_tokens > MAX_AI_STEP_TOKENS {
                    return Err(WorkflowError::invalid(
                        "invalid_step_params",
                        format!(
                            "the `max_tokens` of an ai.prompt step is between 1 and \
                             {MAX_AI_STEP_TOKENS}, got {max_tokens}"
                        ),
                    ));
                }
            }
            Ok(())
        }
        other => Err(WorkflowError::invalid(
            "invalid_step_action",
            format!("`{other}` is not a built-in action"),
        )),
    }
}

/// Require a non-blank text parameter.
///
/// The value may be a template (`"{{event.revision_id}}"`) — resolution happens when a trigger
/// materialises the run, so validation can only check that something was given.
fn require_text(params: &Value, action: &str, key: &str) -> Result<()> {
    let text = params.get(key).and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return Err(WorkflowError::invalid(
            "invalid_step_params",
            format!("the {action} action needs a non-empty `{key}` parameter"),
        ));
    }
    Ok(())
}

/// Run one synthetic action. `attempt` is the current attempt, counting from 1.
///
/// A failure is a message, not an error type of the engine: whether it is retried or ends the
/// run is the retry policy's decision, not the action's. Host actions never reach this path —
/// the engine hands them to the installed [`crate::ActionHandler`]; called here they report
/// exactly that instead of silently succeeding.
pub fn run(action: &str, params: &Value, attempt: i32) -> std::result::Result<Value, String> {
    if is_host_action(action) {
        return Err(format!(
            "`{action}` is a host action; the process must install an action handler to run it"
        ));
    }

    match action {
        "noop" => Ok(serde_json::json!({ "action": "noop" })),
        "echo" => Ok(serde_json::json!({
            "action": "echo",
            "value": params.get("value").cloned().unwrap_or(Value::Null),
        })),
        "fail" => Err(params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the fail action always fails")
            .to_owned()),
        "transient" => {
            let fail_times = params
                .get("fail_times")
                .and_then(Value::as_i64)
                .unwrap_or(i64::from(i32::MAX));
            if i64::from(attempt) <= fail_times {
                Err(format!(
                    "simulated transient failure on attempt {attempt} of {fail_times}"
                ))
            } else {
                Ok(serde_json::json!({ "action": "transient", "attempt": attempt }))
            }
        }
        other => Err(format!("`{other}` is not a built-in action")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_action_set_is_small_and_well_formed() {
        assert_eq!(
            keys(),
            vec![
                "noop",
                "echo",
                "fail",
                "transient",
                "send_email",
                "comment_revision",
                "http_request",
                "publish_page",
                "run_workflow",
                "ai.prompt"
            ]
        );
        for action in ACTIONS.iter().chain(HOST_ACTIONS.iter()) {
            assert!(!action.description.trim().is_empty(), "{}", action.key);
            assert!(is_action(action.key));
        }
        assert!(!is_action("smtp_send"), "the set is closed");

        // The two sets are disjoint and the host knows which is which. The three outbound
        // actions (REQ-003 slice 2) are host actions like the other two: they leave the
        // process, so the engine never runs them itself.
        assert!(is_host_action("send_email"));
        assert!(is_host_action("comment_revision"));
        assert!(is_host_action("http_request"));
        assert!(is_host_action("publish_page"));
        assert!(is_host_action("run_workflow"));
        assert!(!is_host_action("noop"));
        assert!(ACTIONS.iter().all(|action| !is_host_action(action.key)));
    }

    #[test]
    fn a_host_action_never_runs_inside_the_engine() {
        let message = run("send_email", &serde_json::json!({}), 1)
            .expect_err("the engine refuses to run a host action itself");
        assert!(message.contains("host action"), "{message}");
    }

    #[test]
    fn host_action_parameters_are_checked_before_the_run() {
        assert!(
            validate_params(
                "send_email",
                &serde_json::json!({ "to": "ada@example.com", "subject": "Hi", "body": "Hello" })
            )
            .is_ok()
        );
        for missing in ["to", "subject", "body"] {
            let mut params =
                serde_json::json!({ "to": "ada@example.com", "subject": "Hi", "body": "Hello" });
            params.as_object_mut().expect("object").remove(missing);
            let error =
                validate_params("send_email", &params).expect_err("every field is required");
            assert_eq!(error.code(), "invalid_step_params");
            assert!(error.to_string().contains(missing), "{error}");
        }

        assert!(
            validate_params(
                "comment_revision",
                &serde_json::json!({ "revision_id": "{{event.revision_id}}", "body": "Shipped" })
            )
            .is_ok(),
            "a template counts as a value; resolution happens when the run is materialised"
        );
        assert!(
            validate_params(
                "comment_revision",
                &serde_json::json!({ "body": "Shipped" })
            )
            .is_err()
        );
        assert!(
            validate_params(
                "comment_revision",
                &serde_json::json!({ "revision_id": " " , "body": "x" })
            )
            .is_err()
        );
    }

    #[test]
    fn the_outbound_actions_check_their_shape_before_a_handler_ever_sees_them() {
        // A definition is validated without a handler installed, so a rule that could not
        // possibly run is refused when it is written rather than when its event arrives.
        assert!(
            validate_params(
                "http_request",
                &serde_json::json!({ "url": "https://a.test/x" })
            )
            .is_ok()
        );
        assert!(validate_params("http_request", &serde_json::json!({})).is_err());
        assert!(validate_params("http_request", &serde_json::json!({ "url": "  " })).is_err());
        assert!(
            validate_params(
                "http_request",
                &serde_json::json!({ "url": "https://a.test/x", "method": "trace" })
            )
            .is_err(),
            "a method outside the list is refused when the rule is written"
        );
        assert!(
            validate_params(
                "http_request",
                &serde_json::json!({ "url": "https://a.test/x", "method": "post" })
            )
            .is_ok(),
            "the method is case-insensitive"
        );

        assert!(
            validate_params(
                "publish_page",
                &serde_json::json!({ "page_id": "{{event.page_id}}" })
            )
            .is_ok(),
            "a template counts as a value"
        );
        assert!(validate_params("publish_page", &serde_json::json!({})).is_err());

        assert!(
            validate_params(
                "run_workflow",
                &serde_json::json!({ "workflow_id": "{{event.workflow_id}}" })
            )
            .is_ok()
        );
        assert!(validate_params("run_workflow", &serde_json::json!({})).is_err());
    }

    #[test]
    fn the_ai_step_is_a_host_action_with_a_bounded_budget() {
        // It is a HOST action, not a synthetic one, for the same reason `send_email` is: it
        // leaves the process. The engine must refuse to run it itself rather than answering
        // with a stub, because a stub that "succeeds" would let a rule be armed that can never
        // do what its canvas says it does.
        assert!(is_action("ai.prompt"));
        assert!(is_host_action("ai.prompt"), "the provider lives outside the engine");
        assert!(!ACTIONS.iter().any(|a| a.key == "ai.prompt"), "and never in the synthetic set");
        let refusal = run("ai.prompt", &serde_json::json!({ "prompt": "hi" }), 1)
            .expect_err("the engine must not run a host action itself");
        assert!(refusal.contains("action handler"), "{refusal}");

        // A prompt is required; a template in it is the normal case.
        assert!(validate_params("ai.prompt", &serde_json::json!({ "prompt": "Summarise {{event.body}}" })).is_ok());
        assert!(validate_params("ai.prompt", &serde_json::json!({})).is_err());
        assert!(validate_params("ai.prompt", &serde_json::json!({ "prompt": "  " })).is_err());

        // `model` and `max_tokens` are optional, but a present one must be the right TYPE.
        // A `model` of `42` would otherwise reach the router as a string and produce a
        // "model not found" message about a model nobody typed.
        assert!(validate_params("ai.prompt", &serde_json::json!({ "prompt": "p", "model": "gpt-4o-mini" })).is_ok());
        assert!(validate_params("ai.prompt", &serde_json::json!({ "prompt": "p", "model": 42 })).is_err());

        // The budget is bounded at VALIDATION time, so a rule that would spend a million
        // tokens per run is refused when it is written, not when its trigger fires. The
        // boundaries are asserted on both sides because "at most" is where an off-by-one
        // hides: a ceiling of 10_000 must accept 10_000 and refuse 10_001.
        for (tokens, ok) in [
            (1, true),
            (MAX_AI_STEP_TOKENS, true),
            (MAX_AI_STEP_TOKENS + 1, false),
            (0, false),
            (-5, false),
        ] {
            let error = validate_params(
                "ai.prompt",
                &serde_json::json!({ "prompt": "p", "max_tokens": tokens }),
            );
            assert_eq!(error.is_ok(), ok, "max_tokens {tokens}: {error:?}");
        }
        // A non-integer budget is refused rather than rounded — 1000.5 tokens is a mistake,
        // and silently flooring it would let an author believe they asked for more.
        assert!(validate_params(
            "ai.prompt",
            &serde_json::json!({ "prompt": "p", "max_tokens": 1000.5 })
        )
        .is_err());
    }

    #[test]
    fn noop_and_echo_succeed() {
        let output = run("noop", &serde_json::json!({}), 1).expect("noop succeeds");
        assert_eq!(output["action"], "noop");

        let output =
            run("echo", &serde_json::json!({ "value": "hello" }), 1).expect("echo succeeds");
        assert_eq!(output["value"], "hello");
    }

    #[test]
    fn fail_always_reports_its_message() {
        let message = run("fail", &serde_json::json!({ "message": "no route" }), 1)
            .expect_err("fail never succeeds");
        assert_eq!(message, "no route");

        let message = run("fail", &serde_json::json!({}), 3).expect_err("fail never succeeds");
        assert!(message.contains("always fails"), "{message}");
    }

    #[test]
    fn transient_fails_until_its_attempts_are_used_up() {
        let params = serde_json::json!({ "fail_times": 2 });
        assert!(run("transient", &params, 1).is_err());
        assert!(run("transient", &params, 2).is_err());
        let output = run("transient", &params, 3).expect("the third attempt succeeds");
        assert_eq!(output["attempt"], 3);
    }

    #[test]
    fn parameters_are_checked_before_the_run() {
        assert!(validate_params("echo", &serde_json::json!({})).is_err());
        assert!(validate_params("echo", &serde_json::json!({ "value": 1 })).is_ok());
        assert!(validate_params("transient", &serde_json::json!({})).is_err());
        assert!(validate_params("transient", &serde_json::json!({ "fail_times": 9 })).is_err());
        assert!(validate_params("transient", &serde_json::json!({ "fail_times": 1 })).is_ok());
        assert!(validate_params("noop", &serde_json::json!({})).is_ok());
        assert_eq!(
            validate_params("nope", &serde_json::json!({}))
                .expect_err("unknown actions are refused")
                .code(),
            "invalid_step_action"
        );
    }
}
