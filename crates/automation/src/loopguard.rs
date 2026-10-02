//! The endless-loop guard: a rule that repeats itself must stop itself, and say why.
//!
//! A `run_workflow` chain is bounded by its depth ([`crate::outbound::MAX_CHAIN_DEPTH`]),
//! and a self-chain is refused outright. Neither bound is enough, because the loop a rule
//! author actually writes is neither: it is **one rule whose own action starts the same
//! work again** — a rule on `user.created` that writes a user, a rule on `page.published`
//! that republishes, a `publish_page` step that emits the event the rule listens for. The
//! chain depth stays 1, the rule is not calling itself by name, and the run count grows
//! until something else in the platform notices.
//!
//! ## What counts as a repeat
//!
//! **The same step kind twice in a row with identical resolved parameters.** Narrow on
//! purpose, because the alternative — "a rule that ran N times recently" — is a rate
//! limit wearing a different hat, and the request already asks for one of those
//! ([`crate::limits`]). The two guards answer different questions:
//!
//! * the rate window answers *"is this rule firing more often than its author meant?"*
//! * this guard answers *"is this rule making itself do the same thing again?"*
//!
//! Identity is [`crate::limits`]'s window fingerprint: `kind` + `action` + the step's
//! resolved parameters. Parameters matter because two `publish_page` steps with different
//! pages are a rule that publishes two pages, not a loop — and a guard that only compared
//! kinds would refuse a perfectly ordinary rule.
//!
//! ## Where the guard sits
//!
//! **After the step succeeded, before the next one is claimed.** Two reasons, both
//! practical:
//!
//! * a guard that ran *before* the step could not tell a repeat from a first run, because
//!   the fingerprint it would compare is the fingerprint it is about to produce;
//! * the engine's own claim query already refuses to claim step N+1 while step N is open,
//!   so "the last two steps" is always the last two *settled* steps — the guard reads two
//!   rows, and there is no in-memory state to lose on a restart.
//!
//! ## What it does when it fires
//!
//! The run is settled `failed` with a message an operator can act on, and the *step after*
//! the repeat is closed in the same write. Not a retry: a loop is not a flaky host, and
//! retrying it would be the guard's own bug turning into the incident. The fingerprint
//! goes into the step's own error text, so the trace says which step repeated — the panel
//! can then show it without a second query.
//!
//! ## Why the comparison ignores `on_error` and `timeout_ms`
//!
//! Those are *policy*, not *work*. A rule that retries a step with a longer budget is
//! still doing the same thing, and a guard keyed on the whole row would let a step
//! alternate `stop`/`continue` forever. Identity is the work, and nothing else.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_workflows::guard::{GuardStep, GuardVerdict, RunGuard};

use crate::error::Result;

/// What the guard decided about a run that just finished a step.
///
/// `Clone` and not `Copy` because the repeated arm carries the step's *name* — the panel
/// shows which step repeated, so the name is part of the verdict rather than something
/// the caller re-reads. A `Copy` here would have meant an id and a second query, and a
/// name that had already been read is the thing a trace shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopVerdict {
    /// Not a repeat — the run goes on.
    Clear,
    /// The step repeated the one before it; the caller must stop the run.
    Repeated {
        /// The step that repeated.
        step_no: i32,
        /// Name of the step that repeated, for the message.
        name: String,
    },
}

impl LoopVerdict {
    /// `true` when the run may continue.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        matches!(self, Self::Clear)
    }

    /// The error a repeated step's run ends with.
    ///
    /// A `run_workflow` chain is a bounded thing that is *meant* to repeat a kind, so the
    /// message names that case explicitly rather than leaving an author to work out why
    /// their deliberate chain was refused — the bound for that is `MAX_CHAIN_DEPTH` and
    /// the guard refuses only when the parameters match too, which a chain's steps do not
    /// (a chain's second step runs a *different* rule with a different id).
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Clear => None,
            Self::Repeated { step_no, name } => Some(format!(
                "step {step_no} (\"{name}\") repeats the step before it with exactly the same \
                 inputs, so this rule is making itself do the same thing again and the run was \
                 stopped here. A rule that should call another rule once is a run_workflow step; \
                 a rule that may run this many times an hour is the rule's rate limit."
            )),
        }
    }
}

/// The identity of one step's *work*: what it does, not how it is written.
///
/// Deliberately excludes `name`, `on_error`, `timeout_ms` and `max_attempts`: an author who
/// renames a step has not changed what the run does, and a guard keyed on the name would
/// let a copy-pasted step be renamed to dodge it.
#[must_use]
pub fn fingerprint(kind: &str, action: Option<&str>, params: &Value) -> String {
    let mut hasher = Sha256::new();
    // Length-prefixed rather than joined, so two different pairs cannot collide into one
    // string: `{kind: "task", action: "publish_page"}` and `{kind: "task", action:
    // "publish"}` + stray params must not hash the same as the obvious concatenation.
    for part in [kind, action.unwrap_or(""), &canonical(params)] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// A stable string for a JSON value.
///
/// `serde_json` preserves the order of an object's keys as written, so hashing the raw
/// text would make `{"a":1,"b":2}` and `{"b":2,"a":1}` two different steps. The map is
/// rebuilt sorted at every level before it is written out.
#[must_use]
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let inner: Vec<String> = keys
                .iter()
                .map(|key| format!("{}:{}", Value::from((*key).clone()), canonical(&map[*key])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// Decide whether the run has just repeated itself.
///
/// Reads the two most recent *settled* steps of the run. `for update` is **not** taken and
/// is not needed: the run is claimed by exactly one runner (the claim query filters on
/// `status = 'running'` steps, and a step is claimed once), so the two rows cannot change
/// between this read and the caller's write. Adding a lock here would buy nothing and cost
/// a round trip on the hot path of every successful step.
///
/// `Ok(LoopVerdict::Clear)` for a run with fewer than two settled steps — a first step has
/// nothing to repeat, and refusing a run's first step would be a guard that stops every
/// rule.
pub async fn check(
    pool: &PgPool,
    execution_id: Uuid,
    step_no: i32,
    kind: &str,
    action: Option<&str>,
    params: &Value,
) -> Result<LoopVerdict> {
    let previous: Option<(i32, String, String, Option<String>, Value)> = sqlx::query_as(
        "select step_no, name, kind, action, coalesce(params, 'null'::jsonb) as params \
         from workflow_steps \
         where execution_id = $1 and step_no < $2 and status = 'succeeded' \
         order by step_no desc limit 1",
    )
    .bind(execution_id)
    .bind(step_no)
    .fetch_optional(pool)
    .await?;

    let Some((previous_no, previous_name, previous_kind, previous_action, previous_params)) =
        previous
    else {
        return Ok(LoopVerdict::Clear);
    };

    // Adjacent *settled* steps need not be adjacent numbers: a run whose step 3 failed and
    // was told to continue reached step 4, so the last two successes are 2 and 4. The
    // guard asks "did the last successful step do exactly this", not "was it numbered one
    // lower", because a rule that publishes page A, fails on a webhook, then publishes
    // page A again is a loop even though a step came between them.
    if fingerprint(&previous_kind, previous_action.as_deref(), &previous_params)
        != fingerprint(kind, action, params)
    {
        return Ok(LoopVerdict::Clear);
    }

    Ok(LoopVerdict::Repeated {
        step_no,
        name: name_or(&previous_name, previous_no),
    })
}

/// The display name of the step, falling back to its number.
fn name_or(name: &str, step_no: i32) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        format!("step {step_no}")
    } else {
        trimmed.to_owned()
    }
}

/// Stop a run that repeated itself: the **write itself** lives in the engine.
///
/// This used to be a function here, and it was a lie the compiler could not catch: nothing
/// called it. The engine owns the stopping — after a step succeeds it consults the guard
/// (see [`crate::guard::check_run`]) and, on a stop verdict, calls `store::fail_step` with
/// the reason and then `store::end_run_after_branch`, which closes every later step in the
/// same write and lets the run settle `failed` from its own rows.
///
/// Two things follow, and both are worth stating because the duplicate version asserted the
/// opposite:
///
/// * the reason lives on **the repeated step's `error`**, not on the run's. The engine writes
///   it where the trace will render it, beside the step that repeated;
/// * the steps *after* it carry the engine's own "the run ended before this step", so a reader
///   (or a test) that joins every error in a stopped run and expects them all to name the
///   loop is looking at the wrong column.
///
/// A second implementation of a state transition is never safer than one: it is a second
/// place for the truth to be stale, and this one was stale enough that a test written against
/// it read an empty run error and concluded the guard was mute.

/// The `output` a repeated step's row carries, so the trace shows *why* without a second
/// query and the panel can render it as the failure it is.
#[must_use]
pub fn repeated_output(verdict: &LoopVerdict) -> Option<Value> {
    verdict.reason().map(|reason| {
        json!({
            "loop_guard": {
                "repeated": true,
                "reason": reason,
            }
        })
    })
}

/// The audit metadata of a stopped run, for `workflow.step.loop_detected`.
#[must_use]
pub fn loop_metadata(verdict: &LoopVerdict, workflow_id: Uuid) -> Option<Value> {
    let reason = verdict.reason()?;
    let LoopVerdict::Repeated { step_no, name } = verdict else {
        return None;
    };
    Some(json!({
        "workflow_id": workflow_id,
        "step_no": step_no,
        "step": name,
        "bound": "endless_loop_guard",
        "reason": reason,
    }))
}

/// When the last repeat was stopped, for the rule's own "why is it not firing" line.
///
/// The guard's own audit rows are the honest source: a run that was stopped is a fact the
/// trace already carries, and a *second* place recording it would be a second thing to
/// fall behind. The rule's `last_error` is the summary, and this reads the newest one.
pub async fn last_stopped_at(
    pool: &PgPool,
    workflow_id: Uuid,
) -> Result<Option<(OffsetDateTime, String)>> {
    let row: Option<(OffsetDateTime, String)> = sqlx::query_as(
        "select started_at, coalesce(error, '') from workflow_executions \
         where workflow_id = $1 and status = 'failed' \
           and error like 'step % repeats the step before it%' \
         order by started_at desc limit 1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// The engine's guard, backed by this module.
///
/// The engine asks through [`RunGuard`] because the dependency runs the other way: the
/// automation layer knows what a repeated step means, the engine only knows when a step
/// finished. The process installs this one and every run it advances is checked.
///
/// The pool is a *clone* of the process's, not a borrow: the engine's own guard call has
/// a `&'a` lifetime tied to the step, and holding a `&PgPool` across a `Send` future would
/// force the whole tick onto a single thread.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    /// The process's pool, used only to read the previous step.
    pool: PgPool,
}

impl LoopGuard {
    /// Build the guard for a process that runs the automation engine.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl RunGuard for LoopGuard {
    fn check<'a>(
        &'a self,
        step: GuardStep<'a>,
    ) -> Pin<Box<dyn Future<Output = GuardVerdict> + Send + 'a>> {
        // The parameters are read into an owned value *before* the async block, because the
        // step's borrow cannot cross into a `'static`-shaped future the way a reference
        // into a row can. The value is small (a step's resolved parameters) and the
        // alternative — holding `&'a Value` across the await — would make the guard
        // future non-Send whenever the caller's row lives on the stack.
        let params = step.params.clone();
        let pool = self.pool.clone();

        Box::pin(async move {
            let verdict = check(
                &pool,
                step.execution_id,
                step.step_no,
                step.kind,
                step.action,
                &params,
            )
            .await
            .unwrap_or(LoopVerdict::Clear);

            match verdict.reason() {
                Some(reason) => GuardVerdict::stop(reason),
                None => GuardVerdict::clear(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(value: Value) -> Value {
        value
    }

    #[test]
    fn the_same_step_twice_is_the_same_step() {
        let step = params(json!({ "url": "https://example.com/hook", "method": "POST" }));
        assert_eq!(
            fingerprint("task", Some("http_request"), &step),
            fingerprint("task", Some("http_request"), &step)
        );
    }

    #[test]
    fn a_different_kind_or_action_is_not_a_repeat() {
        let step = params(json!({ "title": "Published" }));
        assert_ne!(
            fingerprint("task", Some("publish_page"), &step),
            fingerprint("task", Some("send_email"), &step),
            "a different action is a different thing to do"
        );
        assert_ne!(
            fingerprint("task", Some("publish_page"), &step),
            fingerprint("stop", None, &step),
            "a control step is not a task"
        );
    }

    #[test]
    fn different_parameters_are_a_different_thing_to_do() {
        // The case a kind-only guard gets wrong: one rule that publishes two pages is
        // ordinary, and refusing it would be a guard that stops real work.
        let first = params(json!({ "slug": "home" }));
        let second = params(json!({ "slug": "pricing" }));
        assert_ne!(
            fingerprint("task", Some("publish_page"), &first),
            fingerprint("task", Some("publish_page"), &second)
        );

        // A parameter that is the same in a *different type* is different work: `1` and
        // `"1"` are not the same argument to an action.
        assert_ne!(
            fingerprint("task", Some("wait"), &params(json!({ "seconds": 1 }))),
            fingerprint("task", Some("wait"), &params(json!({ "seconds": "1" }))),
            "the wire type is part of the work"
        );
    }

    #[test]
    fn the_order_of_an_objects_keys_is_not_part_of_the_work() {
        // JSON preserves key order as written, so hashing the raw text would make
        // `{"a":1,"b":2}` and `{"b":2,"a":1}` two different steps — and a panel that
        // rebuilt a params object in a different order would silently un-lock a loop.
        let one = params(json!({ "a": 1, "b": 2 }));
        let other = params(json!({ "b": 2, "a": 1 }));
        assert_eq!(
            fingerprint("task", Some("x"), &one),
            fingerprint("task", Some("x"), &other)
        );
        // …at every level, not just the top.
        let nested_one = params(json!({ "outer": { "a": 1, "b": [3, 2] } }));
        let nested_other = params(json!({ "outer": { "b": [3, 2], "a": 1 } }));
        assert_eq!(
            fingerprint("task", Some("x"), &nested_one),
            fingerprint("task", Some("x"), &nested_other)
        );
        // Array order *is* meaningful, though: [3,2] is not [2,3].
        assert_ne!(
            fingerprint("task", Some("x"), &params(json!({ "a": [3, 2] }))),
            fingerprint("task", Some("x"), &params(json!({ "a": [2, 3] }))),
            "a list is ordered"
        );
    }

    #[test]
    fn no_two_different_steps_share_one_fingerprint() {
        // The length-prefixed encoding exists for this: a naive join would make
        // kind="task" + action="publish" and kind="task" + action="publis" + a
        // parameter that absorbs the difference hash the same.
        assert_ne!(
            fingerprint("task", Some("publish_page"), &json!({})),
            fingerprint("task", Some("publish_page_extra"), &json!({}))
        );
        assert_ne!(
            fingerprint("task", Some("ab"), &json!({})),
            fingerprint("taska", Some("b"), &json!({})),
            "the kind and the action are separate fields, not one string"
        );
    }

    #[test]
    fn a_renamed_step_is_still_the_same_work() {
        // Renaming is not an escape hatch: the fingerprint is kind + action + params and
        // the name is not in it.
        assert_eq!(
            fingerprint(
                "task",
                Some("send_email"),
                &json!({ "to": "a@example.com" })
            ),
            fingerprint(
                "task",
                Some("send_email"),
                &json!({ "to": "a@example.com" })
            )
        );
    }

    #[test]
    fn a_clear_verdict_has_no_reason_to_report() {
        assert!(LoopVerdict::Clear.is_clear());
        assert!(LoopVerdict::Clear.reason().is_none());
        assert!(repeated_output(&LoopVerdict::Clear).is_none());
        assert!(loop_metadata(&LoopVerdict::Clear, Uuid::nil()).is_none());
    }

    #[test]
    fn a_repeat_is_stopped_with_a_message_an_operator_can_act_on() {
        let verdict = LoopVerdict::Repeated {
            step_no: 4,
            name: "publish the home page".to_owned(),
        };
        assert!(!verdict.is_clear());

        let reason = verdict.reason().expect("a reason");
        assert!(reason.contains("step 4"), "{reason}");
        assert!(reason.contains("publish the home page"), "{reason}");
        // The two things an author can actually do about it must both be named, or the
        // message is a complaint rather than an instruction.
        assert!(reason.contains("run_workflow"), "{reason}");
        assert!(reason.contains("rate limit"), "{reason}");

        let output = repeated_output(&verdict).expect("the trace shows why");
        assert_eq!(output["loop_guard"]["repeated"], true);
        assert!(output["loop_guard"]["reason"].as_str().is_some());

        let metadata = loop_metadata(&verdict, Uuid::nil()).expect("an audit row");
        assert_eq!(metadata["bound"], "endless_loop_guard");
        assert_eq!(metadata["step_no"], 4);
        assert_eq!(metadata["step"], "publish the home page");
    }

    #[test]
    fn a_step_with_no_name_is_named_by_its_number() {
        // An empty name is stored (the engine trims it) and a trace that says
        // "step \"\" repeats" is worse than useless.
        assert_eq!(name_or("  ", 3), "step 3");
        assert_eq!(name_or("publish", 3), "publish");
    }
}
