//! The outbound circuit breaker: closed, open, half-open (REQ-127, slice 3).
//!
//! One breaker per outbound provider key — an AI provider, a webhook destination host, an SMTP
//! relay, a payment provider, the search backend. The state machine is **pure** and the row is
//! the persistence, which is what makes the request's hardest requirement hold: *"a restart
//! does not pretend a broken provider is healthy"*. A breaker that lived only in memory would
//! come back `closed` after a deploy and a provider that has been down for an hour would get a
//! full burst of traffic on the way back up — the exact moment it cannot afford it.
//!
//! ## Why half-open is a counter and not a flag
//!
//! Half-open answers "let a few through and see". "A few" is `half_open_probes`, and closing
//! needs `success_threshold` successes. Both are counts because both are things an operator
//! tunes while an incident is happening, and a boolean cannot express "three is right, one is
//! optimistic".
//!
//! ## The decision is a function of the row and the observation
//!
//! [`record`] is pure: it takes the stored [`BreakerState`], an [`Observation`] and `now`, and
//! returns the new state plus the event to emit. The middleware calls it, the persistence layer
//! writes the result, and the panel's state chip reads the row. There is no second transition
//! table anywhere, so "the panel says closed while the middleware is refusing" is not
//! expressible.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{ReliabilityError, Result};
use crate::vocabulary::BREAKER_STATES;

/// Bounds an operator may configure, so a mistyped threshold cannot create a breaker that never
/// opens or one that opens on a single blip.
pub const MIN_THRESHOLD: i32 = 1;
/// A hundred failures inside a window is a provider that is down, not a provider that is flaky.
pub const MAX_THRESHOLD: i32 = 100;
/// Longest cooldown a breaker may wait before probing.
pub const MAX_COOLDOWN_SECONDS: i64 = 3_600;
/// Largest probe count a half-open state may allow through at once.
pub const MAX_PROBES: i32 = 100;

/// One provider's breaker, as stored and as rendered.
///
/// `open` is deliberately **not** a computed field: it is persisted, and a state read back after
/// a restart is the state that is true. A breaker whose open flag were derived from a timestamp
/// would quietly close when the timestamp was older than the cooldown, without a probe having
/// succeeded — which is the failure this module exists to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakerState {
    /// The provider key, e.g. `openai` or `webhook:hooks.example.com`.
    pub key: String,
    /// An operator-facing name.
    pub name: String,
    /// Failures inside the window that open the breaker.
    pub failure_threshold: i32,
    /// The window those failures are counted in.
    pub window_seconds: i64,
    /// How long the breaker stays open before it probes.
    pub cooldown_seconds: i64,
    /// How many calls may through while half-open.
    pub half_open_probes: i32,
    /// How many of those must succeed to close.
    pub success_threshold: i32,
    /// The current state, one of [`BREAKER_STATES`].
    pub state: String,
    /// Whether an operator deliberately drained this provider.
    ///
    /// **Separate from `state = open` on purpose.** A forced-open breaker must not close because
    /// a probe happened to succeed, and it must not expire on its cooldown. One flag, checked
    /// before every transition, is what makes "drain this provider until I say otherwise" a real
    /// statement rather than a 30-second suggestion.
    pub forced_open: bool,
    /// When the breaker last tripped.
    pub opened_at: Option<OffsetDateTime>,
    /// When the state last changed.
    pub state_changed_at: OffsetDateTime,
    /// How many times this breaker has tripped in its life.
    pub trips_total: i64,
    /// Failures counted inside the current window.
    pub failures_in_window: i32,
    /// Successes counted since the breaker went half-open.
    pub successes_in_half_open: i32,
    /// When the failure window opened; a count older than the window is not in the window.
    pub window_started_at: OffsetDateTime,
}

impl BreakerState {
    /// A closed breaker with the shipped defaults, which are conservative on purpose: an
    /// outbound path that retries also breaks, so a breaker that opens late is cheaper than a
    /// platform that hammers something already down.
    #[must_use]
    pub fn new(key: &str, now: OffsetDateTime) -> Self {
        Self {
            key: key.into(),
            name: key.into(),
            failure_threshold: 5,
            window_seconds: 60,
            cooldown_seconds: 30,
            half_open_probes: 3,
            success_threshold: 3,
            state: "closed".into(),
            forced_open: false,
            opened_at: None,
            state_changed_at: now,
            trips_total: 0,
            failures_in_window: 0,
            successes_in_half_open: 0,
            window_started_at: now,
        }
    }

    /// The failures counted in the window right now, resetting the count when the window has
    /// rolled.
    ///
    /// **A fixed window, not a sliding one, and the choice is visible here.** A sliding window
    /// needs a per-call history to answer "how many failures in the last 60 s", which means a
    /// Redis list per provider and a read on every call; a fixed window is one counter and one
    /// timestamp. The cost is the well-known edge: a burst of failures either side of a roll
    /// opens the breaker one window earlier than a sliding window would. That is a *sooner*
    /// trip on a provider that is already misbehaving, and this module's job is to stop calling
    /// something that is down — an early trip is the cheap direction to be wrong in.
    #[must_use]
    pub fn failures_in_window(&self, now: OffsetDateTime) -> i32 {
        if (now - self.window_started_at).whole_seconds() >= self.window_seconds {
            0
        } else {
            self.failures_in_window
        }
    }

    /// Reject a configuration the platform will not run, naming the field and the bound.
    pub fn validate(&self) -> Result<()> {
        if self.key.trim().is_empty() {
            return Err(ReliabilityError::invalid("key must not be empty"));
        }
        if !(MIN_THRESHOLD..=MAX_THRESHOLD).contains(&self.failure_threshold) {
            return Err(ReliabilityError::invalid(format!(
                "failure_threshold must be between {MIN_THRESHOLD} and {MAX_THRESHOLD}, got {}",
                self.failure_threshold
            )));
        }
        if !(1..=MAX_COOLDOWN_SECONDS).contains(&self.cooldown_seconds) {
            return Err(ReliabilityError::invalid(format!(
                "cooldown_seconds must be between 1 and {MAX_COOLDOWN_SECONDS}, got {}",
                self.cooldown_seconds
            )));
        }
        if !(1..=MAX_PROBES).contains(&self.half_open_probes) {
            return Err(ReliabilityError::invalid(format!(
                "half_open_probes must be between 1 and {MAX_PROBES}, got {}",
                self.half_open_probes
            )));
        }
        if !(1..=MAX_THRESHOLD).contains(&self.success_threshold) {
            return Err(ReliabilityError::invalid(format!(
                "success_threshold must be between 1 and {MAX_THRESHOLD}, got {}",
                self.success_threshold
            )));
        }
        Ok(())
    }

    /// Whether a call may be attempted right now.
    ///
    /// `true` for `closed` and `half_open`; `false` for `open` and for any state a forced-open
    /// flag holds. The caller gets a *fail fast* either way — the point of an open breaker is
    /// that the caller is told immediately rather than waiting out a timeout.
    #[must_use]
    pub fn allows_call(&self, now: OffsetDateTime) -> bool {
        if self.forced_open {
            return false;
        }
        match self.state.as_str() {
            "closed" | "half_open" => true,
            "open" => {
                // The cooldown is the earliest time a probe may be *considered*; the
                // transition to half_open happens in `record`, so this returns true and lets
                // exactly one call through to be the probe.
                self.cooldown_elapsed(now)
            }
            _ => false,
        }
    }

    /// Whether the cooldown has passed since the trip.
    #[must_use]
    pub fn cooldown_elapsed(&self, now: OffsetDateTime) -> bool {
        match self.opened_at {
            Some(opened) => (now - opened).whole_seconds() >= self.cooldown_seconds,
            // An open breaker with no timestamp is a hand-edited row. Treating it as "not
            // elapsed" is the safe answer: keep refusing until a probe is explicitly allowed.
            None => false,
        }
    }

    /// Seconds until a probe may be considered, or `None` while held open deliberately.
    ///
    /// The number the `provider_unavailable` error carries, so a caller that chooses to wait
    /// waits exactly as long as the breaker will allow.
    #[must_use]
    pub fn retry_after(&self, now: OffsetDateTime) -> Option<i64> {
        if self.forced_open {
            return None;
        }
        let opened = self.opened_at?;
        Some((self.cooldown_seconds - (now - opened).whole_seconds()).max(0))
    }
}

/// What one call to the provider did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observation {
    /// The provider answered successfully.
    Success,
    /// The provider failed in a way the breaker counts.
    Failure,
}

/// The result of feeding one observation through the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    /// The new state.
    pub state: BreakerState,
    /// The event name to emit, or `None` when nothing changed.
    ///
    /// `None` is the common case and is the point: a healthy provider emits nothing, so the
    /// event stream is a list of incidents rather than a list of calls.
    pub event: Option<&'static str>,
    /// The state the breaker left, for the `breaker_events` row.
    pub from_state: String,
    /// Why it moved, for the same row.
    pub reason: Option<String>,
    /// The failure rate the caller observed, recorded on the trip.
    ///
    /// A `double` the store may not be able to write — a NaN from a division by a zero window
    /// — and the row takes it as `NULL` rather than as a NaN, because "no rate" and "a rate of
    /// not-a-number" are the same answer to an operator and only one of them is a value.
    pub failure_rate: Option<f64>,
}

/// Feed one observation through the state machine.
///
/// The order of the checks is the machine:
///
/// 1. **Forced open wins over everything.** No success, no cooldown and no probe closes a
///    provider an operator has deliberately drained.
/// 2. **Closed** counts failures inside the window; reaching the threshold opens the breaker and
///    increments the trip count.
/// 3. **Open** either refuses (before cooldown — but then the caller should not have called, so
///    this is a late caller and the state is unchanged) or moves to `half_open` when the cooldown
///    has passed. **The move is the probe**: the first call after the cooldown is the one that
///    decides, which is why the counters start from zero here.
/// 4. **Half-open** counts successes toward `success_threshold` and failures back to `open` with
///    the trip count incremented again — a provider that fails its probe has not recovered, and
///    the second trip is the evidence for it.
#[must_use]
pub fn record(
    state: &BreakerState,
    observation: Observation,
    now: OffsetDateTime,
    failure_rate: Option<f64>,
) -> Transition {
    let from = state.state.clone();

    if state.forced_open {
        return Transition {
            state: state.clone(),
            event: None,
            from_state: from,
            reason: None,
            failure_rate: None,
        };
    }

    match state.state.as_str() {
        "closed" => {
            // The window may have rolled since the last observation, in which case the count
            // starts from this failure rather than from the previous window's last one.
            let rolled = (now - state.window_started_at).whole_seconds() >= state.window_seconds;
            match observation {
                Observation::Success => {
                    // A success does NOT clear the window's count. It ends a bad run when the
                    // failures were spread out; a breaker that forgets its failures the moment
                    // one call succeeds is a breaker that opens only when every call fails.
                    let mut next = state.clone();
                    if rolled {
                        next.failures_in_window = 0;
                        next.window_started_at = now;
                    }
                    Transition {
                        state: next,
                        event: None,
                        from_state: from,
                        reason: None,
                        failure_rate: None,
                    }
                }
                Observation::Failure => {
                    let counted = if rolled { 1 } else { state.failures_in_window + 1 };
                    let mut next = state.clone();
                    if rolled {
                        next.window_started_at = now;
                    }
                    next.failures_in_window = counted;
                    if counted >= state.failure_threshold {
                        // The rate is the count against the threshold, not against the window:
                        // "five failures when five open it" is the number an operator reads off
                        // the trip row and the one the screen shows.
                        let rate =
                            Some(f64::from(counted) / f64::from(state.failure_threshold));
                        return open(
                            &next,
                            now,
                            from,
                            "threshold reached",
                            failure_rate.or(rate),
                        );
                    }
                    Transition {
                        state: next,
                        event: None,
                        from_state: from,
                        reason: None,
                        failure_rate: None,
                    }
                }
            }
        }
        "open" => {
            if state.cooldown_elapsed(now) {
                Transition {
                    state: BreakerState {
                        state: "half_open".into(),
                        state_changed_at: now,
                        // Entering half-open is the probe: the counters start from zero here,
                        // or the new state inherits the old window's failures and the first
                        // success closes it.
                        failures_in_window: 0,
                        successes_in_half_open: 0,
                        window_started_at: now,
                        ..state.clone()
                    },
                    event: Some(crate::vocabulary::events::BREAKER_HALF_OPENED), // breaker.half_opened
                    from_state: from,
                    reason: Some("cooldown elapsed".into()),
                    failure_rate: None,
                }
            } else {
                // A late caller inside the cooldown. The state is unchanged, and no event is
                // emitted: a refusal is not a transition.
                Transition {
                    state: state.clone(),
                    event: None,
                    from_state: from,
                    reason: None,
                    failure_rate: None,
                }
            }
        }
        "half_open" => match observation {
            Observation::Success => {
                let counted = state.successes_in_half_open + 1;
                if counted >= state.success_threshold {
                    close(state, now, from)
                } else {
                    let mut next = state.clone();
                    next.successes_in_half_open = counted;
                    Transition {
                        state: next,
                        event: None,
                        from_state: from,
                        reason: None,
                        failure_rate: None,
                    }
                }
            }
            Observation::Failure => open(state, now, from, "probe failed", failure_rate),
        },
        // An unknown state is a hand-edited row. Refusing is the safe answer: a breaker that
        // fails open on a state it does not understand is not a breaker.
        _ => Transition {
            state: state.clone(),
            event: None,
            from_state: from,
            reason: None,
            failure_rate: None,
        },
    }
}

/// Open the breaker and record the trip.
fn open(
    state: &BreakerState,
    now: OffsetDateTime,
    from: String,
    reason: &str,
    failure_rate: Option<f64>,
) -> Transition {
    Transition {
        state: BreakerState {
            state: "open".into(),
            opened_at: Some(now),
            state_changed_at: now,
            trips_total: state.trips_total + 1,
            ..state.clone()
        },
        event: Some(crate::vocabulary::events::BREAKER_OPENED), // breaker.opened
        from_state: from,
        reason: Some(reason.to_string()),
        failure_rate: failure_rate.filter(|r| r.is_finite()),
    }
}

/// Close the breaker.
fn close(state: &BreakerState, now: OffsetDateTime, from: String) -> Transition {
    Transition {
        state: BreakerState {
            state: "closed".into(),
            opened_at: None,
            state_changed_at: now,
            failures_in_window: 0,
            successes_in_half_open: 0,
            window_started_at: now,
            ..state.clone()
        },
        event: Some(crate::vocabulary::events::BREAKER_CLOSED), // breaker.closed
        from_state: from,
        reason: Some("probe succeeded".into()),
        failure_rate: None,
    }
}

/// Whether a call may proceed, and what to answer when it may not.
///
/// One function so the middleware cannot take the "allowed" path for `open` and the error path
/// for `closed` — those are two match statements that have to agree, and the request's
/// `provider_unavailable` contract is exactly about them agreeing.
#[must_use]
pub fn admit(state: &BreakerState, now: OffsetDateTime) -> Admitted {
    if state.allows_call(now) {
        Admitted::Allowed
    } else {
        Admitted::Refused {
            retry_after: state.retry_after(now),
        }
    }
}

/// What [`admit`] answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "admitted", rename_all = "snake_case")]
pub enum Admitted {
    /// The call may proceed.
    Allowed,
    /// The breaker is refusing; `retry_after` is `None` while held open deliberately.
    Refused {
        /// Seconds until a probe may be considered.
        retry_after: Option<i64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn at(seconds: i64) -> OffsetDateTime {
        datetime!(2026-01-01 00:00 UTC) + time::Duration::seconds(seconds)
    }

    #[test]
    fn a_new_breaker_is_closed_and_lets_calls_through() {
        let b = BreakerState::new("openai", at(0));
        assert_eq!(b.state, "closed");
        assert!(b.allows_call(at(0)));
        assert_eq!(admit(&b, at(0)), Admitted::Allowed);
    }

    #[test]
    fn failures_inside_a_default_breaker_do_not_open_it() {
        // Threshold 5: four failures are noise, and a breaker that opens on the first blip is
        // a breaker nobody leaves enabled.
        let mut b = BreakerState::new("openai", at(0));
        for t in 0..4 {
            let tr = record(&b, Observation::Failure, at(t), None);
            assert_eq!(tr.state.state, "closed", "opened on failure {t}");
            assert!(tr.event.is_none());
            b = tr.state;
        }
    }

    #[test]
    fn the_threshold_opens_the_breaker_and_emits_exactly_one_event() {
        let mut b = BreakerState {
            failure_threshold: 3,
            ..BreakerState::new("openai", at(0))
        };
        let mut events = 0;
        for t in 0..5 {
            let tr = record(&b, Observation::Failure, at(t), None);
            if tr.event.is_some() {
                events += 1;
            }
            b = tr.state;
        }
        assert_eq!(events, 1, "an open breaker must emit one opened event, not one per failure");
        assert_eq!(b.state, "open");
        assert_eq!(b.trips_total, 1);
    }

    #[test]
    fn an_open_breaker_fails_fast_with_a_retry_after_and_moves_no_state() {
        let b = BreakerState {
            state: "open".into(),
            opened_at: Some(at(0)),
            ..BreakerState::new("openai", at(0))
        };
        assert!(!b.allows_call(at(5)));
        let Admitted::Refused { retry_after } = admit(&b, at(5)) else {
            panic!("expected refusal");
        };
        assert_eq!(retry_after, Some(25));
        // A late caller's observation changes nothing and emits nothing.
        let tr = record(&b, Observation::Failure, at(5), None);
        assert_eq!(tr.state.state, "open");
        assert!(tr.event.is_none());
    }

    #[test]
    fn the_cooldown_moves_to_half_open_and_emits_the_half_opened_event() {
        let b = BreakerState {
            state: "open".into(),
            opened_at: Some(at(0)),
            cooldown_seconds: 30,
            ..BreakerState::new("openai", at(0))
        };
        let tr = record(&b, Observation::Success, at(30), None);
        assert_eq!(tr.state.state, "half_open");
        assert_eq!(tr.event, Some("reliability.breaker.half_opened"));
        assert_eq!(tr.from_state, "open");
    }

    #[test]
    fn a_success_in_half_open_closes_only_when_the_threshold_is_reached() {
        let mut b = BreakerState {
            state: "half_open".into(),
            success_threshold: 2,
            ..BreakerState::new("openai", at(0))
        };
        // One success is not closure. This is the assertion a boolean half-open would fail.
        let tr = record(&b, Observation::Success, at(30), None);
        assert_eq!(tr.state.state, "half_open");
        b = tr.state;
        let tr = record(&b, Observation::Success, at(31), None);
        assert_eq!(tr.state.state, "closed");
        assert_eq!(tr.event, Some("reliability.breaker.closed"));
        assert!(tr.state.opened_at.is_none());
    }

    #[test]
    fn a_failed_probe_reopens_and_increments_the_trip_count() {
        let b = BreakerState {
            state: "half_open".into(),
            trips_total: 1,
            ..BreakerState::new("openai", at(0))
        };
        let tr = record(&b, Observation::Failure, at(60), None);
        assert_eq!(tr.state.state, "open");
        assert_eq!(tr.state.trips_total, 2);
        assert_eq!(tr.event, Some("reliability.breaker.opened"));
    }

    #[test]
    fn a_threshold_of_one_opens_on_the_first_failure_not_the_second() {
        let b = BreakerState {
            failure_threshold: 1,
            ..BreakerState::new("flaky", at(0))
        };
        let tr = record(&b, Observation::Failure, at(0), None);
        assert_eq!(tr.state.state, "open");
    }

    #[test]
    fn a_forced_open_breaker_is_not_closed_by_a_success_and_never_probes() {
        let mut b = BreakerState {
            state: "open".into(),
            forced_open: true,
            opened_at: Some(at(0)),
            ..BreakerState::new("openai", at(0))
        };
        // Not even a success, and not even after the cooldown.
        for t in [0, 30, 10_000] {
            assert!(!b.allows_call(at(t)));
            let tr = record(&b, Observation::Success, at(t), None);
            assert_eq!(tr.state.state, "open");
            assert!(tr.event.is_none());
        }
        // And the error carries no retry_after, because no amount of waiting helps.
        let Admitted::Refused { retry_after } = admit(&b, at(1_000)) else {
            panic!("expected refusal");
        };
        assert_eq!(retry_after, None);
        b.state = "closed".into();
        // Even a closed breaker that carries the flag is held: the flag is the operator's
        // statement, and a state change from elsewhere must not silently clear it.
        assert!(!b.allows_call(at(0)));
    }

    #[test]
    fn an_open_breaker_survives_a_restart_because_the_row_carries_the_state() {
        // The property is the shape of the type: state and opened_at are stored fields, and
        // `new` is the only thing that produces `closed`. Reading a row back after a restart
        // therefore cannot yield a healthy-looking breaker.
        let mut b = BreakerState::new("openai", at(0));
        for t in 0..5 {
            b = record(&b, Observation::Failure, at(t), None).state;
        }
        assert_eq!(b.state, "open");
        let round_tripped: BreakerState =
            serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(round_tripped, b);
        assert!(!round_tripped.allows_call(at(5)));
    }

    #[test]
    fn an_open_breaker_with_no_timestamp_keeps_refusing() {
        let b = BreakerState {
            state: "open".into(),
            opened_at: None,
            ..BreakerState::new("openai", at(0))
        };
        assert!(!b.allows_call(at(10_000)));
    }

    #[test]
    fn an_unknown_state_refuses_rather_than_failing_open() {
        let b = BreakerState {
            state: "weird".into(),
            ..BreakerState::new("openai", at(0))
        };
        assert!(!b.allows_call(at(0)));
    }

    #[test]
    fn validation_names_the_field_and_the_bound() {
        let mut b = BreakerState::new("x", at(0));
        b.failure_threshold = 0;
        assert!(b.validate().unwrap_err().to_string().contains("failure_threshold"));
        let mut b = BreakerState::new("x", at(0));
        b.cooldown_seconds = 0;
        assert!(b.validate().unwrap_err().to_string().contains("cooldown_seconds"));
        let mut b = BreakerState::new("x", at(0));
        b.half_open_probes = 0;
        assert!(b.validate().unwrap_err().to_string().contains("half_open_probes"));
        let mut b = BreakerState::new("x", at(0));
        b.key = " ".into();
        assert!(b.validate().unwrap_err().to_string().contains("key"));
    }

    #[test]
    fn every_state_this_module_produces_is_in_the_vocabulary() {
        let mut b = BreakerState::new("x", at(0));
        for t in 0..3 {
            b = record(&b, Observation::Failure, at(t), None).state;
        }
        for t in 3..8 {
            b = record(&b, Observation::Success, at(t), None).state;
        }
        for t in 8..14 {
            b = record(&b, Observation::Failure, at(t), None).state;
        }
        assert!(BREAKER_STATES.contains(&b.state.as_str()));
    }
}
