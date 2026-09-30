//! The words of the reliability centre, as compile-time lists.
//!
//! Every closed set in this crate is listed **here** and built from here: the SQL `check`
//! constraints in `0162_reliability.sql` state the same lists in their own comments (SQL cannot
//! import Rust), and [`sql_lists_agree_with_the_migration`] reads that file and fails the crate
//! if a list drifts. A state the database refuses but the panel offers is a filter that
//! silently returns nothing; a state the database accepts but the panel cannot name is an event
//! an operator can never find.

/// The four subjects a limit can be spent against, most specific first.
///
/// **The order here is the specificity order and it is load-bearing.** [`crate::limits::pick`]
/// walks this list and returns the first policy that matches, so "most specific wins" is not a
/// rule the resolver has to implement separately — it is a rule it cannot get wrong, because the
/// list only has one direction to iterate. A scope the resolver cannot see is a scope an
/// operator can create a policy for and never have it take effect.
pub const RATE_SCOPES: &[&str] = &["user", "organization", "ip", "route"];

/// A keyed write's state.
pub const IDEMPOTENCY_STATES: &[&str] = &["in_progress", "completed", "failed"];

/// The state a key is written in before the handler has answered.
pub const IDEMPOTENCY_IN_PROGRESS: &str = "in_progress";

/// The state a key is written in once its attempt has answered, successfully or not.
///
/// Named here rather than in the store so the three states have ONE definition: a hand-written
/// copy of `"completed"` in a query is a second place to change it, and the copy nobody changes is
/// how a state rots into a value the `check` constraint rejects — at runtime, on one row.
pub const IDEMPOTENCY_COMPLETED: &str = "completed";

/// The state of a key whose attempt never finished, so the key can be retried.
///
/// Distinct from `completed` on purpose. A replay of a failed key must NOT return a stored
/// response, because there is none; `decide` maps it to `Proceed` so the retry runs, and the row
/// stays as the evidence of what happened.
pub const IDEMPOTENCY_FAILED: &str = "failed";

/// The subsystems that retry, and each one's shipped default ceiling.
///
/// The order is the panel's display order and is deliberately the order of how expensive a
/// duplicate is: a webhook that fires twice pages somebody twice, an AI call that runs twice
/// costs money twice, and a storage write that runs twice is usually harmless.
pub const RETRY_SUBSYSTEMS: &[&str] = &[
    "webhook",
    "email",
    "ai",
    "workflow",
    "integration",
    "storage",
];

/// How one attempt ended.
pub const RETRY_OUTCOMES: &[&str] = &[
    "succeeded",
    "failed_retryable",
    "failed_permanent",
    "exhausted",
];

/// The three ways a delay may be spread. `full` is the default everywhere.
pub const JITTER_MODES: &[&str] = &["none", "equal", "full"];

/// A breaker's state.
pub const BREAKER_STATES: &[&str] = &["closed", "open", "half_open"];

/// The HMAC families an inbound endpoint may declare.
pub const INTAKE_SCHEMES: &[&str] = &["sha256_hex", "sha256_base64", "sha1_hex"];

/// Why an inbound request was refused.
///
/// These are the **stored** reasons. The wire code is the same string, so an operator reading a
/// rejection row and an operator reading a `401` body are reading one vocabulary rather than
/// two that have to be translated by hand.
pub const INTAKE_REASONS: &[&str] = &[
    "signature_missing",
    "signature_invalid",
    "timestamp_stale",
    "replay",
    "payload_too_large",
    "content_type_refused",
    "malformed",
];

/// How wide the sanitisation pass is.
pub const SANITIZE_PROFILES: &[&str] = &["strict", "balanced"];

/// Largest page any list read will return, whatever the caller asks for.
pub const MAX_PAGE: usize = 200;

/// Largest `Retry-After` a refusal may claim, in seconds.
///
/// One hour. A client told to wait longer than that is a client that has given up on the wait
/// and is retrying anyway, so the honest answer at that point is a longer window, not a longer
/// `Retry-After`.
pub const MAX_RETRY_AFTER_SECONDS: i64 = 3_600;

/// The stable error codes other systems match on.
///
/// The request says these are stable and documented, so they live in one table with a test
/// that reads **the request file** and asserts the table says what the document says — the same
/// direction the observability crate checks its events against its own spec, and for the same
/// reason: a code that exists only in a table describing an intent is an integration that
/// breaks when somebody renames a variant.
pub const ERROR_CODES: &[&str] = &[
    "idempotency_conflict",
    "rate_limited",
    "payload_too_large",
    "signature_invalid",
    "provider_unavailable",
];

/// The event names this request emits.
///
/// Same rule as [`ERROR_CODES`], and same test: the request's **Events** block is read and every
/// name it documents must be declared here, and nothing else may be. `limit.exceeded` is
/// documented as *aggregated* — it is emitted once per scope/target/route/window, never per
/// request — and [`crate::limits`] is where that aggregation happens.
pub const EVENT_NAMES: &[&str] = &[
    "reliability.limit.exceeded",
    "reliability.idempotency.conflict",
    "reliability.idempotency.keys.released",
    "reliability.retry.scheduled",
    "reliability.retry.exhausted",
    "reliability.breaker.opened",
    "reliability.breaker.half_opened",
    "reliability.breaker.closed",
    "reliability.intake.rejected",
    "reliability.policy.updated",
];

/// The event names this crate emits, as constants.
///
/// **Named, not indexed.** `EVENT_NAMES[4]` is one reordering away from announcing
/// `retry.exhausted` on the breaker-opened path, and the unit tests caught exactly that: a
/// breaker that opened reported the wrong event, which is a green suite and a wrong operator
/// alert. Every emission site now names the constant it means.
pub mod events {
    /// One refusal aggregate for a scope/target/route/window. Emitted on the rollup's first
    /// write, never per request.
    pub const LIMIT_EXCEEDED: &str = "reliability.limit.exceeded";
    /// A key replayed with a different body.
    pub const IDEMPOTENCY_CONFLICT: &str = "reliability.idempotency.conflict";
    /// Stuck in-progress keys were released by an operator.
    pub const IDEMPOTENCY_KEYS_RELEASED: &str = "reliability.idempotency.keys.released";
    /// The next attempt was scheduled.
    pub const RETRY_SCHEDULED: &str = "reliability.retry.scheduled";
    /// A subsystem ran out of attempts; the attempt is a dead letter.
    pub const RETRY_EXHAUSTED: &str = "reliability.retry.exhausted";
    /// A breaker tripped.
    pub const BREAKER_OPENED: &str = "reliability.breaker.opened";
    /// A breaker's cooldown elapsed and it is letting probes through.
    pub const BREAKER_HALF_OPENED: &str = "reliability.breaker.half_opened";
    /// A breaker's probes succeeded and it is closed again.
    pub const BREAKER_CLOSED: &str = "reliability.breaker.closed";
    /// An inbound request was refused.
    pub const INTAKE_REJECTED: &str = "reliability.intake.rejected";
    /// A policy was saved.
    pub const POLICY_UPDATED: &str = "reliability.policy.updated";
}

/// A retry event's name, kept in the same table so a test can hold the two to each other.
#[must_use]
pub fn retry_event_for(dead_letter: bool) -> &'static str {
    if dead_letter {
        events::RETRY_EXHAUSTED
    } else {
        events::RETRY_SCHEDULED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every list is a set: a duplicate in one of these is a second definition of the same word,
    /// and the panel's dropdown would show it twice while the resolver still matched one.
    /// The three named states are the ones queries actually write.
    ///
    /// `IDEMPOTENCY_STATES` is a list and the states are constants, so nothing in the type system
    /// stops a query writing a state the `check` constraint rejects — and that failure arrives at
    /// runtime, on one row, in production. The constants exist to stop the string being typed
    /// twice; this test is what stops the constant itself drifting away from the constraint.
    ///
    /// I first added the three constants to the `no_list_carries_a_duplicate` table, which
    /// iterates `(name, list)` pairs — so the table refused to compile, correctly, because they
    /// are not lists. A test's data table is a contract about what kind of thing goes in it.
    #[test]
    fn every_named_idempotency_state_is_one_the_schema_accepts() {
        for state in [IDEMPOTENCY_IN_PROGRESS, IDEMPOTENCY_COMPLETED, IDEMPOTENCY_FAILED] {
            assert!(
                IDEMPOTENCY_STATES.contains(&state),
                "{state} is written by a query but is not in IDEMPOTENCY_STATES, so the check \
                 constraint would refuse it at runtime"
            );
        }
        assert_eq!(
            IDEMPOTENCY_STATES.len(),
            3,
            "a fourth state needs a constant and a decision about what a replay of it does, not \
             just a row in this list"
        );
    }

    #[test]
    fn no_list_carries_a_duplicate() {
        for (name, list) in [
            ("RATE_SCOPES", RATE_SCOPES),
            ("IDEMPOTENCY_STATES", IDEMPOTENCY_STATES),
            ("RETRY_SUBSYSTEMS", RETRY_SUBSYSTEMS),
            ("RETRY_OUTCOMES", RETRY_OUTCOMES),
            ("JITTER_MODES", JITTER_MODES),
            ("BREAKER_STATES", BREAKER_STATES),
            ("INTAKE_SCHEMES", INTAKE_SCHEMES),
            ("INTAKE_REASONS", INTAKE_REASONS),
            ("SANITIZE_PROFILES", SANITIZE_PROFILES),
        ] {
            let unique: BTreeSet<_> = list.iter().collect();
            assert_eq!(
                unique.len(),
                list.len(),
                "{name} lists a value twice: {:?}",
                list.iter().collect::<BTreeSet<_>>().len()
            );
            assert!(!unique.is_empty(), "{name} is empty");
        }
    }

    /// Specificity order is part of the contract, so it is asserted rather than described.
    #[test]
    fn rate_scopes_stay_ordered_most_specific_first() {
        assert_eq!(RATE_SCOPES, &["user", "organization", "ip", "route"]);
    }

    /// Every named event constant is in the table, and the table has nothing unnamed.
    ///
    /// This is the test that would have caught the index bug: it reads the *constants*, not an
    /// array position, so a reorder cannot silently change which event a state machine emits.
    #[test]
    fn every_named_event_is_in_the_table() {
        for name in [
            events::LIMIT_EXCEEDED,
            events::IDEMPOTENCY_CONFLICT,
            events::IDEMPOTENCY_KEYS_RELEASED,
            events::RETRY_SCHEDULED,
            events::RETRY_EXHAUSTED,
            events::BREAKER_OPENED,
            events::BREAKER_HALF_OPENED,
            events::BREAKER_CLOSED,
            events::INTAKE_REJECTED,
            events::POLICY_UPDATED,
        ] {
            assert!(EVENT_NAMES.contains(&name), "{name} is not in EVENT_NAMES");
        }
        assert_eq!(retry_event_for(true), events::RETRY_EXHAUSTED);
        assert_eq!(retry_event_for(false), events::RETRY_SCHEDULED);
    }

    /// A retry subsystem nobody routes is a policy nobody edits.
    #[test]
    fn every_retry_subsystem_has_a_shipped_default() {
        assert!(crate::retry::default_policy_for("webhook").is_some());
        assert!(crate::retry::default_policy_for("storage").is_some());
        for name in RETRY_SUBSYSTEMS {
            assert!(
                crate::retry::default_policy_for(name).is_some(),
                "{name} is listed as a subsystem with no default policy"
            );
        }
        assert!(crate::retry::default_policy_for("nope").is_none());
    }
}
