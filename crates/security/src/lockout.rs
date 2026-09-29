//! The brute-force policy: how many failures lock an account, for how long, and how it is
//! released (REQ-012, slice 3).
//!
//! The lockout already exists in `crates/identity` — it is what protects sign-in today. What is
//! missing, and what this module adds, is the part an **operator** can see and reason about:
//! the thresholds are per-organization configuration, and today they are buried in a policy row
//! the security screen cannot read. So the document, its ranges and the evaluation live here as
//! a pure function, and the screen's "would this lock?" tester calls exactly the same code the
//! sign-in path calls.
//!
//! **Why the evaluation is a pure function over counts, not over a database.** The two questions
//! are different — "has this account been locked?" is a read, "how many failures does this row
//! imply?" is arithmetic — and the arithmetic is the one the panel wants to show *before* anyone
//! types a bad password. Splitting them means the panel's number and the platform's number
//! cannot disagree, which is the only way a tester on that screen is worth anything.
//!
//! **Progressive delay is the part that is easy to get wrong and is therefore explicit here.**
//! The backoff is `base_seconds * 2^(failures-1)`, capped, and the cap is a *cap on the wait*,
//! not on the count: the fifth failure still counts. An implementation that stops counting at
//! the cap lets an attacker keep guessing at a constant delay forever, which is a slower attack
//! but an endless one.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SecurityError};

/// The failure window's own bounds (`60`–`86400` seconds).
///
/// A minute is below any useful window — it cannot outlast a scripted burst — and a day is
/// above any useful window, past which the counter is history rather than a defence.
pub const MIN_WINDOW_SECONDS: i64 = 60;
/// Longest failure window the platform accepts.
pub const MAX_WINDOW_SECONDS: i64 = 86_400;

/// The attempt threshold's own bounds (`1`–`50`).
pub const MIN_ATTEMPTS: i32 = 1;
/// Most failures that may accumulate before an account locks.
pub const MAX_ATTEMPTS: i32 = 50;

/// The lockout duration's own bounds (`1`–`1440` minutes).
pub const MIN_LOCKOUT_MINUTES: i32 = 1;
/// Longest lockout the platform accepts, in minutes (a day).
pub const MAX_LOCKOUT_MINUTES: i32 = 1440;

/// The progressive-delay bounds (`1`–`60` seconds, doubling, capped).
pub const MIN_DELAY_SECONDS: i32 = 1;
/// The longest a single delay may become, whatever the attempt count.
pub const MAX_DELAY_SECONDS: i32 = 60;

/// One organization's brute-force policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockoutPolicy {
    /// How far back failures are counted, in seconds.
    pub window_seconds: i64,
    /// How many failures inside that window lock the account.
    pub attempts: i32,
    /// How long the lock lasts, in minutes.
    pub lockout_minutes: i32,
    /// Whether the delay between failures grows as the count does.
    pub progressive_delay: bool,
    /// The first delay, in seconds; the next failures double it.
    pub base_delay_seconds: i32,
    /// Whether a successful sign-in clears the counter.
    pub reset_on_success: bool,
}

impl Default for LockoutPolicy {
    fn default() -> Self {
        Self {
            window_seconds: 900,
            attempts: 5,
            lockout_minutes: 15,
            progressive_delay: true,
            base_delay_seconds: 2,
            reset_on_success: true,
        }
    }
}

impl LockoutPolicy {
    /// Validate the document, naming the field that is out of range.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] with `window_seconds`, `attempts`,
    /// `lockout_minutes` or `base_delay_seconds` in the message, so the form points at the
    /// input rather than saying "invalid settings".
    pub fn validated(self) -> Result<Self> {
        if !(MIN_WINDOW_SECONDS..=MAX_WINDOW_SECONDS).contains(&self.window_seconds) {
            return Err(SecurityError::invalid(format!(
                "the failure window must be {MIN_WINDOW_SECONDS}–{MAX_WINDOW_SECONDS} seconds — \
                 {} is outside that range",
                self.window_seconds
            )));
        }
        if !(MIN_ATTEMPTS..=MAX_ATTEMPTS).contains(&self.attempts) {
            return Err(SecurityError::invalid(format!(
                "the attempt threshold must be {MIN_ATTEMPTS}–{MAX_ATTEMPTS} — {} is outside \
                 that range",
                self.attempts
            )));
        }
        if !(MIN_LOCKOUT_MINUTES..=MAX_LOCKOUT_MINUTES).contains(&self.lockout_minutes) {
            return Err(SecurityError::invalid(format!(
                "the lockout duration must be {MIN_LOCKOUT_MINUTES}–{MAX_LOCKOUT_MINUTES} \
                 minutes — {} is outside that range",
                self.lockout_minutes
            )));
        }
        if !(MIN_DELAY_SECONDS..=MAX_DELAY_SECONDS).contains(&self.base_delay_seconds) {
            return Err(SecurityError::invalid(format!(
                "the base delay must be {MIN_DELAY_SECONDS}–{MAX_DELAY_SECONDS} seconds — {} is \
                 outside that range",
                self.base_delay_seconds
            )));
        }
        Ok(self)
    }

    /// Whether `failures` inside the window locks the account.
    ///
    /// The comparison is `>=` and the test asserts it at the boundary: a threshold of five must
    /// lock on the fifth failure, and a policy that locks on the sixth is one guess weaker than
    /// the number the operator typed.
    #[must_use]
    pub fn locks_at(&self, failures: i64) -> bool {
        failures >= i64::from(self.attempts)
    }

    /// How long the account stays locked, in seconds.
    #[must_use]
    pub fn lock_seconds(&self) -> i64 {
        i64::from(self.lockout_minutes) * 60
    }

    /// The delay a request arriving after `failures` failures should wait.
    ///
    /// Zero when progressive delay is off, or when no failure has happened — "no delay" is the
    /// honest answer for both, and a caller that forgets to check the first case is a caller
    /// that delays the very first sign-in by two seconds.
    #[must_use]
    pub fn delay_for(&self, failures: i64) -> i64 {
        if !self.progressive_delay || failures < 1 {
            return 0;
        }
        // `min()` on the shift count, not on the result: a shift of 64 is undefined behaviour in
        // Rust and 63 saturates to a negative number, so the exponent itself has to be bounded.
        // This is the bug the test `the_delay_caps_without_overflowing_at_a_wild_count` pins.
        let exponent = (failures - 1).min(MAX_DELAY_SECONDS as i64) as u32;
        let delay = i64::from(self.base_delay_seconds) * 2_i64.pow(exponent.min(31));
        delay.min(i64::from(MAX_DELAY_SECONDS))
    }
}

/// What the policy says about one account, right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockoutState {
    /// Failures counted inside the window.
    pub failures: i64,
    /// Whether that count already locks the account.
    pub would_lock: bool,
    /// The delay the next attempt should wait, in seconds.
    pub next_delay_seconds: i64,
    /// How many failures are left before the lock, never negative.
    pub attempts_remaining: i64,
}

/// Evaluate the policy against a count.
///
/// # Errors
/// Returns [`SecurityError::Invalid`] when the policy is out of range — the same refusal the
/// save path gives, because a tester that quietly clamps an invalid policy is a tester that
/// reports a lock the platform would not perform.
pub fn evaluate(policy: &LockoutPolicy, failures: i64) -> Result<LockoutState> {
    let policy = policy.clone().validated()?;
    let failures = failures.max(0);
    Ok(LockoutState {
        failures,
        would_lock: policy.locks_at(failures),
        next_delay_seconds: policy.delay_for(failures),
        attempts_remaining: (i64::from(policy.attempts) - failures).max(0),
    })
}

/// One account's row on the "currently locked" table.
///
/// The columns are the ones `users` actually has — `failed_sign_in_count` is the counter
/// `crates/identity`'s `register_failure` increments, and reading a name that does not exist
/// would make the query fail at runtime on a screen whose entire job is to report a lockout that
/// is already in progress. `reason` is derived, not stored: `users` has no reason column, and
/// inventing one here would be a column this module believes in and the database does not have.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LockedAccount {
    /// The locked user.
    pub user_id: uuid::Uuid,
    /// Their address, for the row's context.
    pub email: String,
    /// When the lock ends. Always in the future for a row this query returned.
    pub locked_until: time::OffsetDateTime,
    /// Failures counted when the lock was applied.
    pub failed_sign_in_count: i32,
    /// Seconds the lock still has to run, so the panel can render "in 14 minutes" without the
    /// browser and the database disagreeing about the current time.
    pub seconds_remaining: i64,
}

/// The document the panel renders, or reads the baseline from an unwritten row.
#[must_use]
pub fn document(policy: &LockoutPolicy) -> serde_json::Value {
    serde_json::to_value(policy).unwrap_or_else(|_| serde_json::json!({}))
}

/// Read a stored document into a policy, falling back to the default for an empty one.
///
/// # Errors
/// Returns [`SecurityError::Invalid`] when the document is present but unreadable, **partial** or
/// out of range.
///
/// **Why a partial document is an error and not a default.** `#[derive(Default)]` plus `serde`
/// fills an absent field from the default, so a document that once carried `attempts: 9` and now
/// carries only `window_seconds` would read as `attempts: 5` — a value nobody chose, silently
/// substituted into a policy about to be enforced against real sign-in attempts. The check below
/// counts the fields, so "every field is present" is a precondition rather than a hope.
pub fn parse_document(value: &serde_json::Value) -> Result<LockoutPolicy> {
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(LockoutPolicy::default());
    }
    let Some(object) = value.as_object() else {
        return Err(SecurityError::invalid(
            "the sign-in protection policy could not be read: it is not a set of settings",
        ));
    };
    let required: [&str; 6] = [
        "window_seconds",
        "attempts",
        "lockout_minutes",
        "progressive_delay",
        "base_delay_seconds",
        "reset_on_success",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|field| !object.contains_key(*field))
        .collect();
    if !missing.is_empty() {
        return Err(SecurityError::invalid(format!(
            "the sign-in protection policy could not be read: {} missing. A partial policy would \
             be completed from defaults, which is how a threshold nobody chose ends up in force",
            missing.join(", ")
        )));
    }
    let policy: LockoutPolicy = serde_json::from_value(value.clone()).map_err(|err| {
        SecurityError::invalid(format!(
            "the sign-in protection policy could not be read: {err}"
        ))
    })?;
    policy.validated()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_threshold_of_five_locks_on_the_fifth_failure_and_not_the_fourth() {
        let policy = LockoutPolicy::default();
        assert_eq!(policy.attempts, 5);
        for (failures, expected) in [(0, false), (4, false), (5, true), (6, true)] {
            assert_eq!(
                policy.locks_at(failures),
                expected,
                "{failures} failures should {}lock",
                if expected { "" } else { " not " }
            );
        }
    }

    #[test]
    fn the_delay_caps_without_overflowing_at_a_wild_count() {
        // A count of 10_000 failures would ask for `2^9999`. The shift is bounded before it is
        // taken, so the arithmetic stays defined; the test is that it does not panic and does not
        // wrap into a negative delay.
        let policy = LockoutPolicy::default();
        let delay = policy.delay_for(10_000);
        assert_eq!(delay, i64::from(MAX_DELAY_SECONDS));
        assert!(delay > 0, "a capped delay is still a delay, not a wrap");

        for failures in 1..=500_i64 {
            let delay = policy.delay_for(failures);
            assert!(delay > 0, "{failures} failures gave a non-positive delay");
            assert!(
                delay <= i64::from(MAX_DELAY_SECONDS),
                "{failures} failures exceeded the cap"
            );
        }
    }

    #[test]
    fn the_first_failure_is_already_delayed_and_the_zeroth_is_not() {
        // "No failures yet" must cost a legitimate user nothing, and "one failure" must already
        // cost an attacker something — those are different cases and both are asserted.
        let policy = LockoutPolicy::default();
        assert_eq!(policy.delay_for(0), 0, "no history, no delay");
        assert_eq!(policy.delay_for(1), 2, "the base delay is the first wait");
        assert_eq!(policy.delay_for(2), 4);
        assert_eq!(policy.delay_for(3), 8);
    }

    #[test]
    fn progressive_delay_off_means_no_delay_at_any_count() {
        let policy = LockoutPolicy {
            progressive_delay: false,
            ..LockoutPolicy::default()
        };
        for failures in 0..=20_i64 {
            assert_eq!(policy.delay_for(failures), 0, "{failures} failures");
        }
    }

    #[test]
    fn every_range_refusal_names_its_own_field() {
        // The form's whole contract is that a rejected value arrives attached to the input that
        // caused it, so each refusal has to name a different field.
        let cases: [(LockoutPolicy, &str); 4] = [
            (
                LockoutPolicy {
                    window_seconds: 10,
                    ..LockoutPolicy::default()
                },
                "window",
            ),
            (
                LockoutPolicy {
                    attempts: 500,
                    ..LockoutPolicy::default()
                },
                "threshold",
            ),
            (
                LockoutPolicy {
                    lockout_minutes: 0,
                    ..LockoutPolicy::default()
                },
                "lockout duration",
            ),
            (
                LockoutPolicy {
                    base_delay_seconds: 900,
                    ..LockoutPolicy::default()
                },
                "base delay",
            ),
        ];
        for (policy, field) in cases {
            let error = policy.validated().expect_err("out of range");
            assert!(
                error.to_string().contains(field),
                "the refusal should name {field}: {error}"
            );
        }
    }

    #[test]
    fn the_attempts_remaining_counter_never_goes_negative() {
        // A row showing "-2 attempts left" is a display bug with a security meaning: it reads
        // as though the attacker had room to spare.
        let policy = LockoutPolicy::default();
        assert_eq!(evaluate(&policy, 0).expect("valid").attempts_remaining, 5);
        assert_eq!(evaluate(&policy, 5).expect("valid").attempts_remaining, 0);
        assert_eq!(evaluate(&policy, 99).expect("valid").attempts_remaining, 0);
    }

    #[test]
    fn the_tester_refuses_an_invalid_policy_rather_than_clamping_it() {
        // A tester that clamped would report a lock the platform would not perform, which is the
        // one answer that makes a security screen actively misleading.
        let policy = LockoutPolicy {
            attempts: 0,
            ..LockoutPolicy::default()
        };
        assert!(evaluate(&policy, 1).is_err());
    }

    #[test]
    fn an_unwritten_settings_row_reads_as_the_default_policy() {
        // `{}` is what the migration inserts and `null` is what a missing row reads as.
        for value in [serde_json::json!({}), serde_json::Value::Null] {
            assert_eq!(
                parse_document(&value).expect("the baseline is always readable"),
                LockoutPolicy::default()
            );
        }
    }

    #[test]
    fn a_stored_policy_that_no_longer_validates_is_an_error_not_a_silent_reset() {
        // The difference that matters: an empty row is "never configured" and gets the default;
        // a row with a value outside the range is somebody's saved edit, and replacing it with a
        // default would make the panel describe a policy that is not in force.
        //
        // `serde`'s default behaviour is what makes this worth writing down: a missing field in
        // a JSON object is filled from `#[derive(Default)]` rather than refused, so a document
        // that once had `attempts` and now has only `window_seconds` reads as `attempts: 5` —
        // the default, not the value that was saved. That is why this parse goes through
        // `#[serde(deny_unknown_fields)]`-shaped strictness in `parse_document` rather than
        // trusting the derive.
        let value = serde_json::json!({ "attempts": 9999, "window_seconds": 900 });
        let error = parse_document(&value).expect_err("out of range");
        assert!(error.to_string().contains("threshold"), "{error}");

        // A *partial* document is refused too, for the reason above: reading it as the default
        // would show the operator a threshold nobody chose.
        let partial = serde_json::json!({ "window_seconds": 900 });
        let error = parse_document(&partial).expect_err("a partial document is not a policy");
        assert!(
            error.to_string().contains("could not be read"),
            "the refusal should say the document is unreadable, not out of range: {error}"
        );
    }

    #[test]
    fn a_complete_document_reads_back_exactly_what_was_saved() {
        // The other side of the same door: every field present means the derive is not guessing.
        let complete = serde_json::json!({
            "window_seconds": 120,
            "attempts": 9,
            "lockout_minutes": 30,
            "progressive_delay": false,
            "base_delay_seconds": 5,
            "reset_on_success": false
        });
        let parsed = parse_document(&complete).expect("a complete document is readable");
        assert_eq!(parsed.attempts, 9);
        assert_eq!(parsed.window_seconds, 120);
        assert!(!parsed.progressive_delay);
    }

    #[test]
    fn the_lock_is_expressed_in_minutes_and_stored_in_seconds_so_the_math_happens_once() {
        let policy = LockoutPolicy {
            lockout_minutes: 15,
            ..LockoutPolicy::default()
        };
        assert_eq!(policy.lock_seconds(), 900);
    }

    #[test]
    fn a_round_trip_through_the_document_preserves_every_field() {
        let policy = LockoutPolicy {
            window_seconds: 120,
            attempts: 9,
            lockout_minutes: 30,
            progressive_delay: false,
            base_delay_seconds: 5,
            reset_on_success: false,
        };
        let stored = document(&policy);
        assert_eq!(parse_document(&stored).expect("a round trip"), policy);
    }
}
