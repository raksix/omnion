//! The built-in action set of the engine.
//!
//! v0 runs no user-supplied code (docs/09-N8N-TEARDOWN.md §13 lesson 14: no dynamic code in
//! the core process, ever) and no integration yet — HTTP/notification actions arrive with the
//! integration hub (REQ-015) and the automation layer (REQ-003/P13). What ships now is the
//! closed set the engine itself needs to be a complete, testable machine: a no-op, an echo, a
//! step that always fails, and a step that fails a fixed number of times before it succeeds —
//! the synthetic failure the retry policy is proven with.

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

/// Every built-in action.
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

/// Every action key, in catalogue order.
#[must_use]
pub fn keys() -> Vec<&'static str> {
    ACTIONS.iter().map(|action| action.key).collect()
}

/// `true` when the key names a built-in action.
#[must_use]
pub fn is_action(key: &str) -> bool {
    ACTIONS.iter().any(|action| action.key == key)
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
        other => Err(WorkflowError::invalid(
            "invalid_step_action",
            format!("`{other}` is not a built-in action"),
        )),
    }
}

/// Run one action. `attempt` is the current attempt, counting from 1.
///
/// A failure is a message, not an error type of the engine: whether it is retried or ends the
/// run is the retry policy's decision, not the action's.
pub fn run(action: &str, params: &Value, attempt: i32) -> std::result::Result<Value, String> {
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
        assert_eq!(keys(), vec!["noop", "echo", "fail", "transient"]);
        for action in ACTIONS {
            assert!(!action.description.trim().is_empty(), "{}", action.key);
            assert!(is_action(action.key));
        }
        assert!(!is_action("http_request"));
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
