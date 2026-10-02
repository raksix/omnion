//! The retry ledger and breaker store, proved against a real database (REQ-127 slice 3).
//!
//! The state machines in `retry.rs` and `breaker.rs` are unit tested without I/O. What is left is
//! the part that can only be true of a DATABASE, and each walk below names which claim it is the
//! evidence for:
//!
//! * `a_restart_resumes_from_the_persisted_next_attempt_time` — the acceptance criterion "retry
//!   attempts survive a worker restart (next-attempt time is persisted, no double execution)". The
//!   proof is deliberately blunt: the walk writes the ledger, **drops every piece of in-process
//!   state it has**, and then asks a fresh connection what the next attempt time is. A
//!   scheduler that kept the countdown in a field it owns would answer `None` here, and the test
//!   would be describing a scheduler that cannot exist.
//! * `one_exhausted_sequence_writes_exactly_one_dead_letter` — "an exhausted delivery produces one
//!   dead letter". Asserted by COUNTING the rows flagged `dead_letter`, not by checking the
//!   returned row is flagged: a store that wrote the flag twice still returns a flagged row.
//! * `a_forced_open_breaker_survives_a_reload_and_only_a_reset_closes_it` — "a restart does not
//!   reset an open breaker", and the part that is easy to get wrong: `forced_open` is a SEPARATE
//!   column from `state`, so a `reset` that only wrote `state = 'closed'` would leave the flag
//!   set and every later observation would refuse.
//! * `an_observation_that_changes_nothing_writes_no_event` — the "one opened event" criterion. A
//!   healthy provider emits nothing, and this is the walk that would fail if the event write were
//!   unconditional.

mod support;

use omnion_reliability::breaker::{self, BreakerState, Observation, Transition};
use omnion_reliability::breaker_store as bstore;
use omnion_reliability::retry::{self, Failure, Policy};
use omnion_reliability::retry_store as rstore;
use time::{Duration, OffsetDateTime};

use support::walk_state::state_or_fail;

/// A provider key unique to this run, so a suite that runs twice on one database does not
/// inherit its own leftovers — the lesson REQ-125 and REQ-126 each learned separately.
fn fixture() -> (String, String, String) {
    let n = uuid::Uuid::new_v4();
    (format!("w6-ai-{n}"), format!("w6-job-{n}"), n.to_string())
}

fn webhook_policy() -> Policy {
    retry::default_policy_for("webhook").expect("webhook ships a default policy")
}

#[tokio::test]
async fn a_policy_is_edited_without_a_deploy_and_an_override_wins_outright() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    // A provider override unique to this run. The override key is
    // `(subsystem, provider_override)` and the table is GLOBAL — a fixed literal here would be
    // the same row a sibling walk writes, and two tests editing one row is a test that passes or
    // fails depending on which one the scheduler ran first.
    let (_, _, id) = fixture();
    let provider = format!("hooks-{id}.example.com");

    let mut policy = webhook_policy();
    policy.base_delay_ms = 250;
    let written = rstore::upsert_policy(pool, &policy).await.expect("upsert");
    assert_eq!(written.base_delay_ms, 250);

    // Writing the same (subsystem, NULL override) again REPLACES rather than duplicating. The
    // claim under test is "the newest value is the one a reader gets", so it is proved by RESOLVING
    // the key — a COUNT of rows for the subsystem would be a different claim, and one that is not
    // isolation-safe: the table is global and a sibling walk's policy would be counted as a
    // duplicate of this one's.
    policy.base_delay_ms = 900;
    rstore::upsert_policy(pool, &policy).await.expect("second upsert");
    let read_back = rstore::resolve_policy(pool, "webhook", None)
        .await
        .expect("resolve")
        .expect("the base policy is there");
    assert_eq!(read_back.base_delay_ms, 900, "the newest save is the one a reader gets");

    // A provider override is a DIFFERENT row, and it wins outright rather than merging.
    let mut over = policy.clone();
    over.provider_override = Some(provider.clone());
    over.max_attempts = 9;
    rstore::upsert_policy(pool, &over).await.expect("override");

    let resolved = rstore::resolve_policy(pool, "webhook", Some(&provider))
        .await
        .expect("resolve")
        .expect("the override is there");
    assert_eq!(resolved.max_attempts, 9);
    let base = rstore::resolve_policy(pool, "webhook", None)
        .await
        .expect("resolve base")
        .expect("the base is there");
    assert_eq!(
        base.max_attempts, policy.max_attempts,
        "an override does not mutate the subsystem it overrides"
    );
}

#[tokio::test]
async fn an_invalid_policy_is_refused_and_names_the_field() {
    let state = state_or_fail().await;
    let pool = state.db().pool();

    let mut policy = webhook_policy();
    policy.max_attempts = 999; // above MAX_ATTEMPTS
    let err = rstore::upsert_policy(pool, &policy).await.expect_err("must refuse");
    assert!(err.to_string().contains("max_attempts"), "got: {err}");
}

#[tokio::test]
async fn a_restart_resumes_from_the_persisted_next_attempt_time() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (subsystem, subject, _) = fixture();

    let policy = webhook_policy();
    let attempt = 1;
    let outcome = retry::next_attempt(
        &policy,
        attempt,
        Some(&Failure::Status(503)),
        0,
        0.5,
    );
    assert_eq!(outcome.outcome, "failed_retryable");
    let now = OffsetDateTime::now_utc();
    let next_at = now + Duration::seconds(3);

    rstore::record_outcome(
        pool,
        &subsystem,
        "webhook_delivery",
        Some(&subject),
        attempt,
        &outcome.outcome,
        outcome.error_class.as_deref(),
        outcome.next_delay_ms,
        Some(next_at),
        false,
    )
    .await
    .expect("record");

    // Everything above was in-process. A restarted worker has NONE of it and only a connection.
    let resumed = rstore::next_attempt_time(pool, "webhook_delivery", &subject)
        .await
        .expect("next attempt time")
        .expect("a retry is owed");
    // Compared at POSTGRESQL's precision, not the caller's. `timestamptz` stores microseconds, so
    // a value written with nanosecond precision comes back truncated and an exact `==` would fail
    // on a round trip that in fact preserved the instant to the resolution the column can hold.
    // Anything coarser than a microsecond and the test would stop proving the time came back at
    // all — a store that returned `now()` would still pass at minute resolution.
    let precision = Duration::microseconds(1);
    assert!(
        (resumed - next_at).abs() < precision,
        "the schedule came back, not the count: wrote {next_at}, read {resumed}"
    );

    // The ATTEMPT COUNT is equally a property of the database, for the same reason: a scheduler
    // counting attempts in a field it owns loses the count with the process.
    assert_eq!(
        rstore::attempt_count(pool, "webhook_delivery", &subject).await.expect("count"),
        1
    );
}

#[tokio::test]
async fn one_exhausted_sequence_writes_exactly_one_dead_letter() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (subsystem, subject, _) = fixture();

    let mut policy = webhook_policy();
    policy.max_attempts = 3;

    // Three failures against a policy that permits three attempts: the last one exhausts.
    for attempt in 1..=3 {
        let outcome = retry::next_attempt(&policy, attempt, Some(&Failure::Status(503)), 0, 0.5);
        rstore::record_outcome(
            pool,
            &subsystem,
            "webhook_delivery",
            Some(&subject),
            attempt,
            &outcome.outcome,
            outcome.error_class.as_deref(),
            outcome.next_delay_ms,
            None,
            outcome.dead_letter,
        )
        .await
        .expect("record");
    }

    let letters: Vec<_> = rstore::load_dead_letters(pool, 200)
        .await
        .expect("dead letters")
        .into_iter()
        .filter(|r| r.subject_id.as_deref() == Some(subject.as_str()))
        .collect();
    assert_eq!(
        letters.len(),
        1,
        "one exhausted sequence is one dead letter — the count is the assertion, not the flag"
    );
    assert_eq!(letters[0].attempt, 3);
    assert_eq!(letters[0].outcome, "exhausted");

    // The full timeline is the evidence the dead letter points at, and it is ordered by ATTEMPT.
    let timeline = rstore::load_timeline(pool, "webhook_delivery", &subject)
        .await
        .expect("timeline");
    let attempts: Vec<i32> = timeline.iter().map(|r| r.attempt).collect();
    assert_eq!(attempts, vec![1, 2, 3], "a timeline the reader cannot order is not a timeline");
}

#[tokio::test]
async fn a_non_retryable_failure_never_becomes_a_dead_letter() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (subsystem, subject, _) = fixture();

    let policy = webhook_policy();
    // A 422 with a policy that does not list it: permanent on the first attempt, so the sequence
    // ends immediately and leaves nothing to dead-letter.
    let outcome = retry::next_attempt(&policy, 1, Some(&Failure::Status(422)), 0, 0.5);
    assert_eq!(outcome.outcome, "failed_permanent");
    assert!(!outcome.dead_letter, "a permanent failure is not an exhausted one");
    assert!(outcome.next_delay_ms.is_none());

    rstore::record_outcome(
        pool,
        &subsystem,
        "webhook_delivery",
        Some(&subject),
        1,
        &outcome.outcome,
        outcome.error_class.as_deref(),
        None,
        None,
        outcome.dead_letter,
    )
    .await
    .expect("record");

    let letters = rstore::load_dead_letters(pool, 200).await.expect("letters");
    assert!(!letters
        .iter()
        .any(|r| r.subject_id.as_deref() == Some(subject.as_str())));
}

#[tokio::test]
async fn retry_now_adds_an_attempt_and_leaves_the_failure_in_the_timeline() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (subsystem, subject, _) = fixture();

    let policy = webhook_policy();
    let outcome = retry::next_attempt(&policy, 1, Some(&Failure::Status(503)), 0, 0.5);
    let letter = rstore::record_outcome(
        pool,
        &subsystem,
        "webhook_delivery",
        Some(&subject),
        1,
        "exhausted",
        outcome.error_class.as_deref(),
        None,
        None,
        true,
    )
    .await
    .expect("record");

    let retried = rstore::retry_now(pool, letter.id)
        .await
        .expect("retry now")
        .expect("a row was there");
    assert_eq!(retried.attempt, 2, "the retry continues the sequence, it does not reset it");

    let timeline = rstore::load_timeline(pool, "webhook_delivery", &subject)
        .await
        .expect("timeline");
    assert_eq!(timeline.len(), 2);
    assert!(
        timeline.iter().any(|r| r.outcome == "exhausted"),
        "the failure the operator acted on is still in the history"
    );
}

#[tokio::test]
async fn a_forced_open_breaker_survives_a_reload_and_only_a_reset_closes_it() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (key, _, _) = fixture();

    let created = bstore::ensure(pool, &BreakerState::new(&key, OffsetDateTime::now_utc()))
        .await
        .expect("ensure");
    assert_eq!(created.state, "closed");

    let forced = bstore::force_open(pool, &key, "provider is being migrated")
        .await
        .expect("force open")
        .expect("row");
    assert_eq!(forced.state, "open");
    assert!(forced.forced_open, "draining is a flag, not a state that can expire");

    // THE restart case: a fresh read of the persisted row. A breaker whose state were recomputed
    // from a timestamp would come back `closed` here and send the burst this exists to prevent.
    let reloaded = bstore::load(pool, &key).await.expect("load").expect("row");
    assert_eq!(reloaded.state, "open", "a restart must not close an open breaker");
    assert!(reloaded.forced_open);

    // And a SUCCESS must not close a forced-open breaker, which is the whole point of the flag.
    let transition = breaker::record(&reloaded, Observation::Success, OffsetDateTime::now_utc(), None);
    assert_eq!(transition.state.state, "open", "a drained provider stays drained");
    assert!(transition.event.is_none());

    let events = bstore::events_for(pool, &key, 50).await.expect("events");
    assert!(
        events.iter().any(|e| e.reason.as_deref().unwrap_or("").contains("forced")),
        "a manual drain is an audit row, not a hidden flag"
    );

    let reset = bstore::reset(pool, &key, "migration finished")
        .await
        .expect("reset")
        .expect("row");
    assert_eq!(reset.state, "closed");
    assert!(!reset.forced_open, "a reset clears the flag, or the next success is ignored");

    // The reset is auditable AND distinguishable from a machine trip.
    let events = bstore::events_for(pool, &key, 50).await.expect("events");
    assert!(events
        .iter()
        .any(|e| e.reason.as_deref().unwrap_or("").contains("reset")));
}

#[tokio::test]
async fn a_breaker_trips_on_failures_and_probes_its_way_back() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (key, _, _) = fixture();

    let mut breaker = BreakerState::new(&key, OffsetDateTime::now_utc());
    breaker.failure_threshold = 2;
    breaker.cooldown_seconds = 10;
    breaker.success_threshold = 2;
    bstore::ensure(pool, &breaker).await.expect("ensure");

    let mut current = breaker;
    let mut opened: Option<Transition> = None;
    for _ in 0..2 {
        let t = breaker::record(&current, Observation::Failure, OffsetDateTime::now_utc(), Some(1.0));
        if t.state.state == "open" {
            opened = Some(t.clone());
        }
        current = t.state.clone();
        bstore::observe(pool, &t).await.expect("observe");
    }
    let opened = opened.expect("two failures with a threshold of two must open the breaker");
    assert_eq!(opened.state.state, "open");
    assert!(opened.event.is_some(), "a trip emits");

    // A restart reads `open` back and STAYS open — not even the cooldown has passed yet.
    let reloaded = bstore::load(pool, &key).await.expect("load").expect("row");
    assert_eq!(reloaded.state, "open");

    // Past the cooldown the next observation IS the probe. The move to half-open RESETS the
    // success counter (the machine's own comment says so), so the probe that opened the window
    // is not counted toward `success_threshold` — otherwise a single success would satisfy a
    // threshold of two and close a provider that has recovered exactly once.
    let later = OffsetDateTime::now_utc() + Duration::seconds(30);
    let probing = breaker::record(&reloaded, Observation::Success, later, None);
    assert_eq!(probing.state.state, "half_open", "the cooldown's end is the probe");
    assert_eq!(probing.state.successes_in_half_open, 0, "the counter starts from zero here");
    bstore::observe(pool, &probing).await.expect("observe");

    // One success is one: `success_threshold` is 2, so this is still probing.
    let one = breaker::record(&probing.state, Observation::Success, later, None);
    assert_eq!(one.state.state, "half_open", "one success of two is not a recovery");
    bstore::observe(pool, &one).await.expect("observe");

    // The second closes it.
    let closing = breaker::record(&one.state, Observation::Success, later, None);
    assert_eq!(closing.state.state, "closed");
    assert!(closing.event.is_some());
    bstore::observe(pool, &closing).await.expect("observe");

    let events = bstore::events_for(pool, &key, 50).await.expect("events");
    let opened_events = events.iter().filter(|e| e.to_state == "open").count();
    assert_eq!(opened_events, 1, "one trip is one opened event");
}

#[tokio::test]
async fn an_observation_that_changes_nothing_writes_no_event() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (key, _, _) = fixture();

    let mut breaker = BreakerState::new(&key, OffsetDateTime::now_utc());
    breaker.failure_threshold = 5;
    bstore::ensure(pool, &breaker).await.expect("ensure");

    // Ten successful calls on a healthy provider.
    for _ in 0..10 {
        let t = breaker::record(&breaker, Observation::Success, OffsetDateTime::now_utc(), None);
        assert!(t.event.is_none(), "a healthy provider emits nothing");
        bstore::observe(pool, &t).await.expect("observe");
    }

    let events = bstore::events_for(pool, &key, 50).await.expect("events");
    assert!(
        events.is_empty(),
        "an unconditional event write turns the transition log into a call log"
    );

    // The COUNTERS still moved, which is the other half: `observe` saves state even with no event.
    let reloaded = bstore::load(pool, &key).await.expect("load").expect("row");
    assert_eq!(reloaded.failures_in_window, 0);
    assert_eq!(reloaded.state, "closed");
}

#[tokio::test]
async fn editing_a_threshold_does_not_close_an_open_breaker() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (key, _, _) = fixture();

    let mut breaker = BreakerState::new(&key, OffsetDateTime::now_utc());
    breaker.failure_threshold = 1;
    bstore::ensure(pool, &breaker).await.expect("ensure");

    let t = breaker::record(&breaker, Observation::Failure, OffsetDateTime::now_utc(), Some(1.0));
    assert_eq!(t.state.state, "open");
    bstore::observe(pool, &t).await.expect("observe");

    // Raising the threshold is a SETTINGS edit. A screen where saving a number also silently
    // closed a tripped breaker would be a trap, so the two are different SQL.
    let edited = bstore::update_settings(pool, &key, "OpenAI", 9, 60, 30, 3, 3)
        .await
        .expect("update")
        .expect("row");
    assert_eq!(edited.failure_threshold, 9);
    assert_eq!(edited.state, "open", "editing a threshold is not a transition");

    // And a deleted provider takes its history with it, rather than leaving orphaned prose.
    assert!(bstore::delete(pool, &key).await.expect("delete"));
    assert!(bstore::load(pool, &key).await.expect("load").is_none());
    assert!(bstore::events_for(pool, &key, 50).await.expect("events").is_empty());
}
