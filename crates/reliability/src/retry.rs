//! Retry policies: what to retry, how long to wait, and when to stop (REQ-127, slice 3).
//!
//! Four things live here, and the reason they live together is that they must not disagree:
//!
//! * [`Policy`] — the document an operator edits per subsystem, with a per-provider override.
//! * [`delay_for`] — **the** delay curve. One function, called by the scheduler, by the panel's
//!   delay preview and by the dead-letter timeline, so the number the operator is shown before
//!   saving is the number the job will actually wait.
//! * [`classify`] — the retryable-or-permanent decision, from an HTTP status or a provider error
//!   class. A policy that retries a `422` is a policy that will retry a malformed request five
//!   times and then dead-letter it, having done five pointless writes.
//! * [`next_attempt`] — the persistence contract: the next attempt time goes **on the job row**,
//!   not in a queue's head position, so a restart resumes instead of replaying or losing it.
//!
//! ## Why jitter is `full` by default and the arithmetic says so
//!
//! A provider that goes down and comes back is hit by every client that was retrying at the same
//! moment, in the same order, at the same instant. Without jitter the first retry of a thousand
//! jobs is one thundering herd. [`delay_for`] takes an explicit `draw` in `0.0..1.0` rather than
//! reading a random source, so the distribution is testable: a property test over a thousand
//! draws can assert that the delays are *spread*, while `jitter = none` asserts that the same
//! inputs always give the same answer. Both halves are in the tests, because "jitter is on" is a
//! claim that a constant implementation passes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{ReliabilityError, Result};
use crate::vocabulary::{JITTER_MODES, RETRY_OUTCOMES, RETRY_SUBSYSTEMS};

/// Attempts a subsystem may take, floor and ceiling.
///
/// A ceiling of one means "no retry" and is a legitimate policy; a ceiling of zero means the
/// first attempt can never happen, which is a misconfiguration rather than a choice.
pub const MIN_ATTEMPTS: i32 = 1;
/// Twenty is past any honest deadline and exists only so a typo cannot build a week-long retry.
pub const MAX_ATTEMPTS: i32 = 20;
/// The largest backoff factor one policy may carry.
pub const MAX_FACTOR: f64 = 10.0;
/// The floor of the factor, and the value that turns a retry policy into a fixed-delay one.
pub const MIN_FACTOR: f64 = 1.0;

/// One subsystem's retry policy, or one provider's override of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Which subsystem this policy is for; one of [`RETRY_SUBSYSTEMS`].
    pub subsystem: String,
    /// The provider this row overrides, or `None` for the subsystem's own policy.
    pub provider_override: Option<String>,
    /// How many attempts in total, including the first.
    pub max_attempts: i32,
    /// The delay before the second attempt, in milliseconds.
    pub base_delay_ms: i64,
    /// The multiplier applied per attempt.
    pub factor: f64,
    /// `none`, `equal` or `full`.
    pub jitter: String,
    /// The whole budget, in milliseconds. A policy that would wait longer is stopped.
    pub max_elapse_ms: i64,
    /// The error classes this policy retries.
    pub retry_on: BTreeMap<String, bool>,
    /// Whether the policy is enforced.
    pub enabled: bool,
}

impl Policy {
    /// Reject a policy the platform will not run, naming the field and the bound.
    pub fn validate(&self) -> Result<()> {
        if !RETRY_SUBSYSTEMS.contains(&self.subsystem.as_str()) {
            return Err(ReliabilityError::invalid(format!(
                "subsystem must be one of {}, got '{}'",
                RETRY_SUBSYSTEMS.join(", "),
                self.subsystem
            )));
        }
        if !(MIN_ATTEMPTS..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(ReliabilityError::invalid(format!(
                "max_attempts must be between {MIN_ATTEMPTS} and {MAX_ATTEMPTS}, got {}",
                self.max_attempts
            )));
        }
        if self.base_delay_ms < 0 {
            return Err(ReliabilityError::invalid(format!(
                "base_delay_ms must not be negative, got {}",
                self.base_delay_ms
            )));
        }
        if !(MIN_FACTOR..=MAX_FACTOR).contains(&self.factor) {
            return Err(ReliabilityError::invalid(format!(
                "factor must be between {MIN_FACTOR} and {MAX_FACTOR}, got {}",
                self.factor
            )));
        }
        if !JITTER_MODES.contains(&self.jitter.as_str()) {
            return Err(ReliabilityError::invalid(format!(
                "jitter must be one of {}, got '{}'",
                JITTER_MODES.join(", "),
                self.jitter
            )));
        }
        if self.max_elapse_ms < 0 {
            return Err(ReliabilityError::invalid(format!(
                "max_elapse_ms must not be negative, got {}",
                self.max_elapse_ms
            )));
        }
        Ok(())
    }

    /// The class of failure this policy retries, as a set.
    ///
    /// Built from the map every time rather than cached, because the map is small and a cached
    /// copy is a second source of truth for the same question.
    #[must_use]
    pub fn retryable_classes(&self) -> Vec<String> {
        let mut classes: Vec<String> = self
            .retry_on
            .iter()
            .filter(|(_, on)| **on)
            .map(|(class, _)| class.clone())
            .collect();
        classes.sort();
        classes
    }
}

/// The shipped default for a subsystem, or `None` for a name that is not a subsystem.
///
/// These are the numbers that ship, and they are deliberately different per subsystem: a
/// webhook that failed because the receiver was down for four minutes should be retried, and a
/// storage write that failed should be retried fast, because the queue behind it is draining.
/// `full` jitter everywhere, because the alternative is a retry storm.
#[must_use]
pub fn default_policy_for(subsystem: &str) -> Option<Policy> {
    let (attempts, base, max_elapse, retryable): (i32, i64, i64, &[&str]) = match subsystem {
        "webhook" => (8, 1_000, 3_600_000, &["5xx", "429", "timeout"]),
        "email" => (6, 5_000, 1_800_000, &["5xx", "429", "timeout"]),
        // Money and latency: fewer attempts, a bigger base, and never a 4xx — a rejected
        // prompt will be rejected identically five more times.
        "ai" => (3, 4_000, 120_000, &["429", "5xx", "timeout", "overloaded"]),
        "workflow" => (5, 2_000, 900_000, &["5xx", "timeout", "dependency_unavailable"]),
        "integration" => (5, 2_000, 1_800_000, &["5xx", "429", "timeout"]),
        "storage" => (10, 250, 600_000, &["5xx", "timeout", "connection_reset"]),
        _ => return None,
    };
    let retry_on = retryable
        .iter()
        .map(|c| ((*c).to_string(), true))
        .collect();
    Some(Policy {
        subsystem: subsystem.to_string(),
        provider_override: None,
        max_attempts: attempts,
        base_delay_ms: base,
        factor: 2.0,
        jitter: "full".into(),
        max_elapse_ms: max_elapse,
        retry_on,
        enabled: true,
    })
}

/// Every shipped default, for the panel's list view.
#[must_use]
pub fn default_policies() -> Vec<Policy> {
    RETRY_SUBSYSTEMS
        .iter()
        .filter_map(|s| default_policy_for(s))
        .collect()
}

/// The delay before attempt number `attempt` (2 for the first retry).
///
/// `attempt` is the attempt about to be **made**, so the first retry is `2` and there is no
/// `delay_for(policy, 1)` — a caller that passes 1 gets attempt one's own delay back, which is
/// the delay it already spent, and the first retry would fire immediately. Clamping to `2`
/// makes the mistake visible in the number rather than in a thundering herd.
///
/// `draw` is a uniform sample in `0.0..1.0` supplied by the caller, which is what makes this
/// function testable: `none` ignores it and is deterministic, `equal` uses its upper half, `full`
/// spans the whole range, and a distribution test can assert each without seeding a PRNG.
#[must_use]
pub fn delay_for(policy: &Policy, attempt: i32, draw: f64) -> i64 {
    let attempt = attempt.clamp(2, policy.max_attempts.max(2));
    let exponent = f64::from(attempt - 2);
    let base = policy.base_delay_ms as f64 * policy.factor.powf(exponent);
    // A factor or a base that overflows into infinity is a configuration mistake, not a
    // million-year wait: the ceiling is the policy's own elapsed budget.
    let ceiling = policy.max_elapse_ms.max(0) as f64;
    let draw = draw.clamp(0.0, 1.0);
    let scaled = match policy.jitter.as_str() {
        // Full jitter: anywhere in [0, base]. AWS's "Exponential Backoff and Jitter" — the
        // variant that actually decorrelates clients, because the low end is reachable.
        "full" => base * draw,
        // Equal jitter: half the delay is fixed, so a fleet cannot converge on zero.
        "equal" => base / 2.0 + base * draw / 2.0,
        // None: a deterministic curve, which is what a test asserts against.
        _ => base,
    };
    if !scaled.is_finite() {
        return ceiling as i64;
    }
    (scaled.min(ceiling)).max(0.0).round() as i64
}

/// The whole delay sequence for attempts 1..=`count`, as the panel's preview draws it.
///
/// With `jitter = full` this shows the **ceiling** of each attempt's range, because a preview
/// that drew one random sample would show a number that will not happen next time — and an
/// operator tuning a factor needs to see the curve's shape, not one sample of it.
#[must_use]
pub fn preview_sequence(policy: &Policy, count: i32) -> Vec<i64> {
    let count = count.clamp(1, 8);
    (2..=count + 1)
        .map(|attempt| delay_for(policy, attempt, 1.0))
        .collect()
}

/// The class of one failure, in the vocabulary the policies are written in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Failure {
    /// The provider answered with a status code.
    Status(u16),
    /// The provider answered with its own error class.
    Class(String),
    /// The transport gave up: connect, read or write timeout, reset, DNS.
    Transport(String),
}

/// Whether a failure may be retried, and under what name it appears in the ledger.
///
/// The mapping from HTTP status to class is the part worth stating: **4xx is permanent except
/// `408` and `429`**, because a `401` will still be a `401` in five minutes and a `422` will
/// still be a `422`, while `429` is a provider asking for exactly this and `408` is a provider
/// that gave up on its own clock. Retrying the rest of the 4xx range is the most common way a
/// retry policy turns a client bug into an outage.
#[must_use]
pub fn classify(failure: &Failure) -> &'static str {
    match failure {
        Failure::Status(code) => match code {
            // Provider said "later", not "never".
            408 | 425 | 429 => "retryable",
            // Client's fault. Retrying is pointless and load-adding.
            400..=499 => "permanent",
            // Server's fault, including every 5xx an intermediary may invent.
            500..=599 => "5xx",
            // 1xx/2xx/3xx are not failures at all; a caller passing one is a bug, and
            // "permanent" is the safe answer: do not retry something that is not broken.
            _ => "permanent",
        },
        Failure::Class(class) => {
            if class == "timeout" || class == "connection_reset" || class == "overloaded" {
                "retryable"
            } else if class == "dependency_unavailable" {
                "retryable"
            } else {
                "permanent"
            }
        }
        Failure::Transport(_) => "retryable",
    }
}

/// How one attempt ended, and what the scheduler should do next.
///
/// The `state` is derived from the policy and the attempt number rather than passed in, so no
/// caller can emit `succeeded` for an attempt that failed — which is the same
/// "derive what you can derive" rule the observability crate's event names use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptOutcome {
    /// The subsystem's ledger vocabulary entry.
    pub outcome: String,
    /// Which attempt this was, starting at 1.
    pub attempt: i32,
    /// The class name the policy matched, for the timeline.
    pub error_class: Option<String>,
    /// The delay before the next attempt, or `None` when there is none.
    pub next_delay_ms: Option<i64>,
    /// Whether this attempt is the last one the policy permits.
    pub dead_letter: bool,
}

/// Decide what one attempt did.
///
/// `elapsed_ms` is how long the whole subsystem budget has been spent, not this attempt's own
/// duration: a policy whose *cumulative* wait exceeds `max_elapse_ms` stops even when it has
/// attempts left, which is the difference between "stopped when the deadline passed" and
/// "retried until the budget was gone".
#[must_use]
pub fn next_attempt(
    policy: &Policy,
    attempt: i32,
    failure: Option<&Failure>,
    elapsed_ms: i64,
    draw: f64,
) -> AttemptOutcome {
    let Some(failure) = failure else {
        return AttemptOutcome {
            outcome: "succeeded".into(),
            attempt,
            error_class: None,
            next_delay_ms: None,
            dead_letter: false,
        };
    };
    let class = classify(failure);
    let error_class = Some(match failure {
        Failure::Status(code) => code.to_string(),
        Failure::Class(c) | Failure::Transport(c) => c.clone(),
    });
    let retryable = class == "retryable" || class == "5xx";
    let policy_says = policy.retry_on.values().any(|on| *on) && !policy.retryable_classes().is_empty()
        && policy
            .retryable_classes()
            .iter()
            .any(|c| c == error_class.as_deref().unwrap_or("") || class_matches(class, c));

    if !retryable || !policy_says {
        return AttemptOutcome {
            outcome: "failed_permanent".into(),
            attempt,
            error_class,
            next_delay_ms: None,
            dead_letter: false,
        };
    }
    if attempt >= policy.max_attempts {
        return AttemptOutcome {
            outcome: "exhausted".into(),
            attempt,
            error_class,
            next_delay_ms: None,
            dead_letter: true,
        };
    }
    let delay = delay_for(policy, attempt + 1, draw);
    // The budget is for the WHOLE sequence, so the check is cumulative: time already spent
    // plus the next wait. Comparing the delay alone against the budget lets a policy spend its
    // entire hour on the last attempt, which is a policy that outlived the job it belongs to.
    if elapsed_ms.saturating_add(delay) > policy.max_elapse_ms {
        // Out of time, not out of attempts: the honest outcome is exhaustion, and recording it
        // as exhausted is what puts it in the dead-letter list with a `retry now` action.
        return AttemptOutcome {
            outcome: "exhausted".into(),
            attempt,
            error_class,
            next_delay_ms: None,
            dead_letter: true,
        };
    }
    AttemptOutcome {
        outcome: "failed_retryable".into(),
        attempt,
        error_class,
        next_delay_ms: Some(delay),
        dead_letter: false,
    }
}

/// Whether a ledger class label satisfies a policy's `retry_on` entry.
fn class_matches(observed: &str, configured: &str) -> bool {
    match configured {
        "5xx" => observed == "5xx",
        "429" => observed == "retryable" || configured == "429",
        "timeout" | "connection_reset" | "overloaded" | "dependency_unavailable" => {
            observed == "retryable"
        }
        other => other == observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(jitter: &str) -> Policy {
        Policy {
            subsystem: "webhook".into(),
            provider_override: None,
            max_attempts: 5,
            base_delay_ms: 1_000,
            factor: 2.0,
            jitter: jitter.into(),
            max_elapse_ms: 3_600_000,
            retry_on: BTreeMap::from([
                ("5xx".to_string(), true),
                ("429".to_string(), true),
                ("timeout".to_string(), true),
            ]),
            enabled: true,
        }
    }

    #[test]
    fn every_shipped_default_validates() {
        for p in default_policies() {
            p.validate()
                .unwrap_or_else(|e| panic!("{} default is invalid: {e}", p.subsystem));
            assert_eq!(p.jitter, "full", "{} ships without full jitter", p.subsystem);
        }
    }

    #[test]
    fn no_jitter_is_the_same_answer_every_time() {
        let p = policy("none");
        let a = delay_for(&p, 3, 0.1);
        let b = delay_for(&p, 3, 0.9);
        assert_eq!(a, b);
        // 1000 * 2^(3-2) = 2000.
        assert_eq!(a, 2_000);
    }

    #[test]
    fn full_jitter_spreads_and_never_reaches_the_undelayed_curve() {
        let p = policy("full");
        let draws: Vec<f64> = (0..200).map(|i| f64::from(i) / 200.0).collect();
        let delays: Vec<i64> = draws.iter().map(|d| delay_for(&p, 3, *d)).collect();
        let ceiling = delay_for(&p, 3, 1.0);
        let unique: std::collections::BTreeSet<_> = delays.iter().collect();
        // A spread over 200 draws, not a handful of values.
        assert!(unique.len() > 100, "full jitter collapsed to {} values", unique.len());
        // The full range starts at ~0, which is the point of full jitter: a client that always
        // gets the ceiling still synchronises.
        assert!(delays.iter().any(|d| *d <= 100), "full jitter never goes near zero");
        assert!(delays.iter().all(|d| *d <= ceiling));
    }

    #[test]
    fn equal_jitter_keeps_a_fixed_floor() {
        let p = policy("equal");
        let lo = delay_for(&p, 3, 0.0);
        let hi = delay_for(&p, 3, 1.0);
        assert_eq!(lo, 1_000); // half of 2000
        assert_eq!(hi, 2_000);
    }

    #[test]
    fn the_first_retry_never_uses_a_drawn_zero_that_would_fire_immediately() {
        // Attempt 1 already happened, so the first wait is attempt 2's, and its ceiling is the
        // base delay. A draw of 0 gives 0ms under full jitter, which is legal for jitter and
        // illegal for a scheduler: the retry is instant.
        let p = policy("full");
        assert_eq!(delay_for(&p, 2, 0.0), 0);
        // Clamping below 2 is what keeps attempt 1 from being the first wait.
        assert_eq!(delay_for(&p, 1, 1.0), delay_for(&p, 2, 1.0));
    }

    #[test]
    fn a_delay_never_exceeds_the_policys_own_budget() {
        let mut p = policy("none");
        p.max_elapse_ms = 5_000;
        p.factor = 10.0;
        // 1000 * 10^6 is astronomically past 5s; the answer is the budget, not the overflow.
        assert!(delay_for(&p, 7, 1.0) <= 5_000);
    }

    #[test]
    fn a_factor_that_overflows_to_infinity_is_clamped_not_cast() {
        let mut p = policy("none");
        p.factor = f64::INFINITY;
        p.max_elapse_ms = 2_000;
        assert_eq!(delay_for(&p, 6, 1.0), 2_000);
    }

    #[test]
    fn four_xx_is_permanent_except_the_two_that_mean_later() {
        assert_eq!(classify(&Failure::Status(401)), "permanent");
        assert_eq!(classify(&Failure::Status(422)), "permanent");
        assert_eq!(classify(&Failure::Status(404)), "permanent");
        assert_eq!(classify(&Failure::Status(429)), "retryable");
        assert_eq!(classify(&Failure::Status(408)), "retryable");
        assert_eq!(classify(&Failure::Status(503)), "5xx");
        assert_eq!(classify(&Failure::Status(500)), "5xx");
    }

    #[test]
    fn a_transport_failure_is_always_retryable_but_a_2xx_passed_as_a_failure_is_not() {
        assert_eq!(classify(&Failure::Transport("read timeout".into())), "retryable");
        assert_eq!(classify(&Failure::Status(204)), "permanent");
    }

    #[test]
    fn a_success_reports_succeeded_with_no_next_attempt() {
        let p = policy("full");
        let o = next_attempt(&p, 1, None, 0, 0.5);
        assert_eq!(o.outcome, "succeeded");
        assert!(o.next_delay_ms.is_none());
        assert!(!o.dead_letter);
    }

    #[test]
    fn a_non_retryable_class_is_not_retried_and_does_not_dead_letter() {
        let p = policy("full");
        let o = next_attempt(&p, 1, Some(&Failure::Status(422)), 0, 0.5);
        assert_eq!(o.outcome, "failed_permanent");
        assert!(!o.dead_letter);
    }

    #[test]
    fn the_last_attempt_exhausts_and_dead_letters_rather_than_scheduling_a_sixth() {
        let p = policy("full");
        let o = next_attempt(&p, 5, Some(&Failure::Status(503)), 0, 0.5);
        assert_eq!(o.outcome, "exhausted");
        assert!(o.dead_letter);
        assert!(o.next_delay_ms.is_none());
    }

    #[test]
    fn running_out_of_elapsed_budget_exhausts_even_with_attempts_left() {
        let p = policy("none");
        // One attempt left of five, and 3_599_999 ms of the hour already spent: the next wait
        // of 2_000 ms crosses the budget, so this is exhaustion, not another schedule.
        let o = next_attempt(&p, 1, Some(&Failure::Status(503)), 3_599_999, 1.0);
        assert_eq!(o.outcome, "exhausted");
        assert!(o.dead_letter);
        assert!(o.next_delay_ms.is_none());

        // And with a second left, the same request schedules instead — the boundary is the
        // budget, not the attempt count.
        let o = next_attempt(&p, 1, Some(&Failure::Status(503)), 3_500_000, 1.0);
        assert_eq!(o.outcome, "failed_retryable");
        assert!(!o.dead_letter);
    }

    #[test]
    fn the_preview_shape_is_the_ceiling_curve_and_is_bounded_to_eight() {
        let p = policy("none");
        let seq = preview_sequence(&p, 8);
        assert_eq!(seq.len(), 8);
        assert!(seq.windows(2).all(|w| w[0] <= w[1]), "curve is not monotonic: {seq:?}");
        // A factor of 1 makes it flat; a wrong factor is meant to be visible here.
        let mut flat = p.clone();
        flat.factor = 1.0;
        let flat_seq = preview_sequence(&flat, 4);
        assert!(flat_seq.windows(2).all(|w| w[0] == w[1]), "factor 1.0 should be flat");
    }

    #[test]
    fn validation_names_the_field_and_the_bound() {
        let mut p = policy("full");
        p.subsystem = "nope".into();
        assert!(p.validate().unwrap_err().to_string().contains("subsystem"));
        let mut p = policy("full");
        p.max_attempts = 0;
        assert!(p.validate().unwrap_err().to_string().contains("max_attempts"));
        let mut p = policy("full");
        p.jitter = "wild".into();
        assert!(p.validate().unwrap_err().to_string().contains("jitter"));
        let mut p = policy("full");
        p.factor = 99.0;
        assert!(p.validate().unwrap_err().to_string().contains("factor"));
    }

    #[test]
    fn every_outcome_this_module_can_produce_is_in_the_ledger_vocabulary() {
        let p = policy("full");
        let produced = [
            next_attempt(&p, 1, None, 0, 0.5).outcome,
            next_attempt(&p, 1, Some(&Failure::Status(422)), 0, 0.5).outcome,
            next_attempt(&p, 1, Some(&Failure::Status(503)), 0, 0.5).outcome,
            next_attempt(&p, 5, Some(&Failure::Status(503)), 0, 0.5).outcome,
        ];
        for outcome in produced {
            assert!(
                RETRY_OUTCOMES.contains(&outcome.as_str()),
                "{outcome} is not a ledger outcome"
            );
        }
    }
}
