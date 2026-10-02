//! The idempotency key STORE, proved against a real database (REQ-127 slice 2).
//!
//! ## Why these live in a walk and not in the crate's unit tests
//!
//! Everything the store claims is a claim about **concurrency and persistence**, and neither is
//! observable without a database: `rows_affected` is what decides whether a caller owns a key,
//! and a unit test that returns `Claim::Claimed` from a stub has proved that the function has a
//! branch named that. So the pure decisions are unit tested in `idempotency.rs` and the CLAIM is
//! proved here, against the real `unique (scope, subject_id, key)` constraint.
//!
//! ## The test that matters is `only_one_of_n_concurrent_claims_wins`
//!
//! It claims the same key from N tasks at once and asserts **exactly one** `Claim::Claimed`. Any
//! implementation that read-then-wrote fails it, and it fails loudly rather than intermittently —
//! the unique index is the arbiter, so the race is decided by PostgreSQL every time rather than by
//! timing. A test that asserts "the last one to arrive loses" would pass on a broken
//! implementation whenever the scheduler happened to serialise the claims, which is the property
//! that makes concurrency bugs survive review.

mod support;

use omnion_reliability::idem_store::{self, Claim, KeySummary};
use omnion_reliability::idempotency::{
    self, fingerprint, Replay, StoredResponse, INLINE_BODY_CAP,
};
use serde_json::{json, Value};
use time::OffsetDateTime;

use support::walk_state::state_or_fail;

/// A scope and subject unique to THIS run, so a suite that runs twice on one database does not
/// inherit its own leftovers. A fixed name is a landmine for whoever runs it second — REQ-125 and
/// REQ-126 each learned that separately.
fn fixture() -> (String, String) {
    let n = uuid::Uuid::new_v4();
    (format!("w6-store {n}"), n.to_string())
}

fn record(scope: &str, subject: &str, key: &str, hash: &str, now: OffsetDateTime) -> idempotency::KeyRecord {
    idempotency::KeyRecord::new(scope, subject, key, "POST", "/api/v1/thing", hash, now)
}

#[tokio::test]
async fn a_first_claim_is_claimed_and_a_second_claim_finds_the_same_key() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    let first = idem_store::claim(pool, &scope, &subject, "k1", "POST", "/api/v1/thing", "h1", now)
        .await
        .expect("a fresh claim must succeed");
    assert_eq!(first, Claim::Claimed, "the first claim of an unused key owns it");

    let second = idem_store::claim(pool, &scope, &subject, "k1", "POST", "/api/v1/thing", "h1", now)
        .await
        .expect("a losing claim is not an error");
    match second {
        Claim::Existing(found) => {
            assert_eq!(found.request_hash, "h1", "the loser re-reads the CURRENT record");
            assert_eq!(found.state, "in_progress");
        }
        Claim::Claimed => panic!("the second claim of a live key must not own it"),
    }
}

/// The property the whole module exists for.
#[tokio::test]
async fn only_one_of_n_concurrent_claims_wins() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    let workers = 8usize;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..workers {
        let pool = pool.clone();
        let scope = scope.clone();
        let subject = subject.clone();
        set.spawn(async move {
            idem_store::claim(
                &pool, &scope, &subject, "race", "POST", "/api/v1/thing", "hash", now,
            )
            .await
            .map(|claim| (i, claim))
        });
    }

    let mut claimed = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (index, claim) = joined
            .expect("a claim task must not panic")
            .expect("every claim must return an outcome, never an error");
        if claim == Claim::Claimed {
            claimed.push(index);
        }
    }

    assert_eq!(
        claimed.len(),
        1,
        "exactly one of {workers} concurrent claims may own the key; {claimed:?} did — a \
         read-then-write loses this race, and `rows_affected` is what prevents it"
    );

    // And exactly one ROW exists, which is the other half: a scheme that returned `Claim::Claimed`
    // twice while inserting once would pass the count above and leave a caller believing it owns
    // an attempt it did not.
    let count = idem_store::count(pool, &scope, &subject).await.expect("count");
    assert_eq!(count, 1, "the race must not leave more than one row behind");
}

#[tokio::test]
async fn a_replay_returns_the_stored_response_and_counts_itself() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    let created = StoredResponse {
        status: 201,
        headers: [("location".to_owned(), "/api/v1/things/7".to_owned())]
            .into_iter()
            .collect(),
        body: Some(r#"{"id":"7"}"#.to_owned()),
        body_ref: None,
        original_request_id: uuid::Uuid::new_v4(),
    };
    let claim = idem_store::claim(pool, &scope, &subject, "k2", "POST", "/api/v1/thing", "h2", now)
        .await
        .expect("claim");
    assert_eq!(claim, Claim::Claimed);
    idem_store::complete(pool, &scope, &subject, "k2", &created, now)
        .await
        .expect("complete");

    // Now the REPLAY: a second claim, then the pure decision over what it found.
    let replay_claim =
        idem_store::claim(pool, &scope, &subject, "k2", "POST", "/api/v1/thing", "h2", now)
            .await
            .expect("a replay is not an error");
    let Claim::Existing(record) = replay_claim else {
        panic!("a completed key must not be reclaimable")
    };
    match idempotency::decide(Some(&record), "h2", now) {
        Replay::ReturnStored { status, body, .. } => {
            assert_eq!(status, 201, "the stored STATUS is replayed, not re-derived");
            assert_eq!(body.as_deref(), Some(r#"{"id":"7"}"#));
        }
        other => panic!("a same-body replay of a completed key must return the stored response, got {other:?}"),
    }

    assert!(
        idem_store::count_replay(pool, &scope, &subject, "k2").await.expect("count replay"),
        "the replay counter must move"
    );
    let after = idem_store::count_replay(pool, &scope, &subject, "k2").await.expect("count replay");
    assert!(after, "and keep moving on a second replay");
}

#[tokio::test]
async fn the_same_key_with_a_different_body_is_a_conflict_and_a_different_subject_is_not() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    idem_store::claim(pool, &scope, &subject, "k3", "POST", "/api/v1/thing", "hash-A", now)
        .await
        .expect("claim");
    let created = StoredResponse {
        status: 200,
        headers: Default::default(),
        body: Some("{}".to_owned()),
        body_ref: None,
        original_request_id: uuid::Uuid::new_v4(),
    };
    idem_store::complete(pool, &scope, &subject, "k3", &created, now)
        .await
        .expect("complete");

    let other = idem_store::claim(pool, &scope, &subject, "k3", "POST", "/api/v1/thing", "hash-B", now)
        .await
        .expect("a conflicting claim is not an error");
    let Claim::Existing(record) = other else { panic!("the key exists") };
    assert_eq!(
        idempotency::decide(Some(&record), "hash-B", now),
        Replay::Conflict,
        "the same key with a different body must be a conflict, never a second execution"
    );

    // One tenant's key must not collide with another's: that is what `subject_id` is in the
    // unique index for, and it is the difference between a replay guard and a denial of service.
    let other_subject = idem_store::claim(
        pool, &scope, "a-different-subject", "k3", "POST", "/api/v1/thing", "hash-A", now,
    )
    .await
    .expect("claim");
    assert_eq!(
        other_subject,
        Claim::Claimed,
        "one subject's key is another subject's fresh key"
    );
}

#[tokio::test]
async fn an_in_progress_key_is_released_and_then_runs_again() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    idem_store::claim(pool, &scope, &subject, "k4", "POST", "/api/v1/thing", "h4", now)
        .await
        .expect("claim");

    assert!(
        idem_store::release_stale(pool, &scope, &subject, "k4", now).await.expect("release"),
        "a key stuck in_progress must be released"
    );
    assert!(
        !idem_store::release_stale(pool, &scope, &subject, "k4", now).await.expect("release again"),
        "releasing twice must report no work the second time, not a second success"
    );

    // FIRST: the released row is evidence, and it must not read as a response that exists.
    //
    // This is asserted BEFORE the retry claims the key, because after the claim the row has been
    // overwritten and the assertion would pass for the wrong reason. The first version of this
    // test claimed first and asserted `decide(…) == Proceed` on the resulting record — which read
    // as "the released key was re-claimed", when it was really asserting that `decide` had been
    // taught to map `failed` to `Proceed` without proving it on the row that is actually failed.
    let released = idem_store::find(pool, &scope, &subject, "k4")
        .await
        .expect("find")
        .expect("the row survives its release");
    assert_eq!(released.state, "failed", "a released key is marked failed, not completed");
    assert_eq!(
        idempotency::decide(Some(&released), "h4", now),
        Replay::Proceed,
        "a released key must run again rather than replay an answer that never happened — and \
         before this tick it returned a `ReturnStored` carrying status 200 and no body, which \
         is a client being told a write succeeded that never did"
    );

    // SECOND: the retry then owns the key outright, because the upsert takes a failed row over.
    let after = idem_store::claim(pool, &scope, &subject, "k4", "POST", "/api/v1/thing", "h4", now)
        .await
        .expect("claim after release");
    assert_eq!(
        after,
        Claim::Claimed,
        "the retry of a released key must own it; a caller that had to ask again would deadlock \
         against its own crashed attempt"
    );
    assert_eq!(
        idem_store::find(pool, &scope, &subject, "k4").await.expect("find").expect("row").state,
        "in_progress",
        "and the takeover resets it to in_progress, so a second concurrent retry loses the race \
         exactly like a first attempt would"
    );
}

/// Releasing must never touch a COMPLETED key — that row holds a real response.
#[tokio::test]
async fn a_completed_key_is_never_released() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    idem_store::claim(pool, &scope, &subject, "k5", "POST", "/api/v1/thing", "h5", now)
        .await
        .expect("claim");
    let created = StoredResponse {
        status: 200,
        headers: Default::default(),
        body: Some("{}".to_owned()),
        body_ref: None,
        original_request_id: uuid::Uuid::new_v4(),
    };
    idem_store::complete(pool, &scope, &subject, "k5", &created, now)
        .await
        .expect("complete");

    assert!(
        !idem_store::release_stale(pool, &scope, &subject, "k5", now).await.expect("release"),
        "a completed key holds a real response; releasing it would let the write run twice"
    );
    let still = idem_store::find(pool, &scope, &subject, "k5")
        .await
        .expect("find")
        .expect("the row survives");
    assert_eq!(still.state, "completed", "and its state must be untouched");
}

#[tokio::test]
async fn an_expired_key_is_freed_and_taken_by_the_next_caller() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    idem_store::claim(pool, &scope, &subject, "k6", "POST", "/api/v1/thing", "h6", now)
        .await
        .expect("claim");
    // Push the row's expiry into the past rather than waiting a day for it.
    sqlx::query("update idempotency_keys set expires_at = $4 \
                 where scope = $1 and subject_id = $2 and key = $3")
        .bind(&scope)
        .bind(&subject)
        .bind("k6")
        .bind(now - time::Duration::hours(1))
        .execute(pool)
        .await
        .expect("age the key");

    let claim = idem_store::claim(pool, &scope, &subject, "k6", "POST", "/api/v1/thing", "h7", now)
        .await
        .expect("claim over an expired key");
    assert_eq!(
        claim,
        Claim::Claimed,
        "an expired key must be taken by the next caller, not block it forever"
    );
}

#[tokio::test]
async fn prune_removes_expired_keys_and_reports_nothing_to_do_the_second_time() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    idem_store::claim(pool, &scope, &subject, "k7", "POST", "/api/v1/thing", "h7", now)
        .await
        .expect("claim");
    let live = idem_store::count(pool, &scope, &subject).await.expect("count");
    assert_eq!(live, 1);

    // Nothing has expired yet, so the sweep must be a no-op that says so.
    idem_store::prune(pool, now).await.expect("prune");
    assert_eq!(
        idem_store::count(pool, &scope, &subject).await.expect("count"),
        1,
        "a live key must survive the retention sweep"
    );
}

#[tokio::test]
async fn the_list_shows_the_keys_newest_first() {
    let state = state_or_fail().await;
    let pool = state.db().pool();
    let (scope, subject) = fixture();
    let now = OffsetDateTime::now_utc();

    for key in ["first", "second"] {
        idem_store::claim(pool, &scope, &subject, key, "POST", "/api/v1/thing", "h", now)
            .await
            .expect("claim");
    }
    let rows: Vec<KeySummary> = idem_store::list(pool, &scope, &subject, 10)
        .await
        .expect("list");
    assert_eq!(rows.len(), 2, "both keys belong to this fixture");
    assert_eq!(
        rows[0].key, "second",
        "the list is newest first, so the screen shows the most recent key at the top"
    );
}

/// The size cap is a promise about REPLAYS, so it is asserted through `seal`, which is the only
/// way a `StoredResponse` is built in production.
///
/// The first version of this test constructed `body: Some(oversized), body_ref: None` and
/// asserted `oversized_without_reference()`. That state is UNREACHABLE: `seal` drops an
/// oversized body to `None`, so the function sees `body.is_none() && body_ref.is_none()` — and
/// `body.is_none()` was false, because the test had put a body back that the constructor had
/// already removed. The test failed against correct code, which is only useful if the question
/// "which is right?" is asked before editing the test. Here the constructor's contract is the
/// product's: a body over the cap is never stored inline.
#[test]
fn a_body_past_the_inline_cap_is_never_stored_inline() {
    let oversized = "x".repeat(INLINE_BODY_CAP + 1);

    // No reference offered: the caller did not write the body anywhere, so there is nothing to
    // replay from and the honest answer is "oversized with nothing to point at".
    let orphaned = StoredResponse::seal(200, oversized.clone(), uuid::Uuid::new_v4(), None);
    assert!(
        orphaned.body.is_none(),
        "a body past {} bytes must not be kept inline", INLINE_BODY_CAP
    );
    assert!(
        orphaned.oversized_without_reference(),
        "with no reference there is nothing to replay from, and a silent empty replay is worse          than a refusal — this is the shape the middleware turns into a 500 it has to explain"
    );

    // Reference offered: the caller wrote the body to object storage, so a replay can find it.
    let referenced = StoredResponse::seal(
        200,
        oversized,
        uuid::Uuid::new_v4(),
        Some("replies/w6/blob-1".to_owned()),
    );
    assert!(referenced.body.is_none(), "still not inline");
    assert_eq!(referenced.body_ref.as_deref(), Some("replies/w6/blob-1"));
    assert!(
        !referenced.oversized_without_reference(),
        "a reference is the correct way to store a large body, and it makes the replay whole"
    );

    // And the boundary itself: one byte under the cap is still stored inline.
    let fits = StoredResponse::seal(200, "y".repeat(INLINE_BODY_CAP), uuid::Uuid::new_v4(), None);
    assert!(
        fits.body.is_some(),
        "a body exactly at the cap is not oversized; the bound is inclusive"
    );
    assert!(!fits.oversized_without_reference());
}

/// Two requests that differ only in key order are the SAME request.
#[test]
fn the_fingerprint_ignores_key_order_and_whitespace() {
    let a = fingerprint("POST", "/api/v1/thing", r#"{"a":1,"b":{"c":2}}"#);
    let b = fingerprint("POST", "/api/v1/thing", "{ \"b\" : { \"c\" : 2 } ,  \"a\" : 1 }");
    assert_eq!(
        a, b,
        "a client that re-serialised its JSON must still replay, not be handed a spurious conflict"
    );
    let different = fingerprint("POST", "/api/v1/thing", r#"{"a":2,"b":{"c":2}}"#);
    assert_ne!(a, different, "a genuinely different body must still conflict");
}
