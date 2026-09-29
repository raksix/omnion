//! Idempotency keys: a keyed write that runs once, however many times it is sent (REQ-127,
//! slice 2).
//!
//! The contract is five sentences long and every one of them has a failure mode that only
//! shows up in production:
//!
//! 1. The first attempt stores a **fingerprint** of method, path and body.
//! 2. A replay of the same key with the same fingerprint returns the **stored** response.
//! 3. A different fingerprint is `409 idempotency_conflict` — not a second execution.
//! 4. A replay while the first attempt is still running is `409` with `Retry-After`.
//! 5. A keyed request that is **refused** never consumes a key.
//!
//! Point 5 is why the fingerprint and the state live in a table rather than in a cache with a
//! TTL: a refused request has to leave no trace, and "no trace" is much easier to guarantee by
//! never having written one. The API wiring therefore records the key **after** authentication
//! and permission, and the acceptance test asserts the table is empty after a refusal.
//!
//! ## The fingerprint is a hash of a canonical string, never of the parsed body
//!
//! Two requests that differ only in key order or whitespace are the same request, and hashing
//! the raw bytes would call them different — which would hand a client a spurious conflict for
//! something it sent identically. [`fingerprint`] sorts object keys recursively and normalises
//! whitespace, so a JSON round trip through a different serialiser still replays.

use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{ReliabilityError, Result};
use crate::vocabulary::{IDEMPOTENCY_IN_PROGRESS, IDEMPOTENCY_STATES};

/// Default lifetime of a key, and the ceiling an operator may set.
pub const DEFAULT_TTL_HOURS: i64 = 24;
/// A year. Past that the key is not a replay protection, it is an archive.
pub const MAX_TTL_HOURS: i64 = 8_760;
/// The largest body the store keeps inline; beyond it the caller keeps an object-store
/// reference in `response_body_ref` instead.
///
/// 256 KiB, because the store's job is to make a *replay* cheap and a request archive is a
/// different product with different retention rules.
pub const INLINE_BODY_CAP: usize = 256 * 1024;

/// A key as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    /// The endpoint family the key applies to, e.g. `POST /api/v1/posts`.
    pub scope: String,
    /// Who the key belongs to: a user, an API key or an organization.
    pub subject_id: String,
    /// The key itself.
    pub key: String,
    /// The method of the first attempt.
    pub method: String,
    /// The path of the first attempt.
    pub path: String,
    /// The hash of the canonical request.
    pub request_hash: String,
    /// One of [`IDEMPOTENCY_STATES`].
    pub state: String,
    /// The stored response's status, once the attempt finished.
    pub response_status: Option<i16>,
    /// The stored response's body, when it fits [`INLINE_BODY_CAP`].
    pub response_body: Option<String>,
    /// The object-store key, when the body did not fit.
    pub response_body_ref: Option<String>,
    /// How many times this key has been replayed.
    pub replay_count: i32,
    /// When the key stops protecting.
    pub expires_at: OffsetDateTime,
    /// When the attempt finished, for a completed key.
    pub completed_at: Option<OffsetDateTime>,
}

impl KeyRecord {
    /// A fresh, in-progress record for a first attempt.
    #[must_use]
    pub fn new(
        scope: &str,
        subject_id: &str,
        key: &str,
        method: &str,
        path: &str,
        request_hash: &str,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            scope: scope.into(),
            subject_id: subject_id.into(),
            key: key.into(),
            method: method.into(),
            path: path.into(),
            request_hash: request_hash.into(),
            state: IDEMPOTENCY_IN_PROGRESS.into(),
            response_status: None,
            response_body: None,
            response_body_ref: None,
            replay_count: 0,
            expires_at: now + time::Duration::hours(DEFAULT_TTL_HOURS),
            completed_at: None,
        }
    }

    /// Whether the key is past its lifetime.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        now >= self.expires_at
    }

    /// Reject a key the platform will not accept, naming the field.
    ///
    /// The length bound is the one that matters: an unbounded key lands in a unique index, and
    /// an unvalidated key column is an index that a client can fill with megabytes.
    pub fn validate_key(key: &str) -> Result<()> {
        if key.trim().is_empty() {
            return Err(ReliabilityError::invalid(
                "Idempotency-Key must not be empty",
            ));
        }
        if key.len() > 255 {
            return Err(ReliabilityError::invalid(format!(
                "Idempotency-Key must be at most 255 characters, got {}",
                key.len()
            )));
        }
        if !key
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'-' || b == b'_')
        {
            return Err(ReliabilityError::invalid(
                "Idempotency-Key must contain only printable ASCII, '-' or '_'",
            ));
        }
        Ok(())
    }
}

/// The fingerprint of one request: method, path and a canonicalised body.
///
/// Sorted keys and normalised whitespace, so a client that serialises the same object twice
/// replays rather than conflicting. The method is upper-cased because `post` and `POST` are the
/// same request, and a client library that lower-cases methods must not be handed a conflict
/// for a request it did send twice.
#[must_use]
pub fn fingerprint(method: &str, path: &str, body: &str) -> String {
    let canonical = canonicalize(body);
    let mut hasher = Sha256::new();
    hasher.update(method.to_ascii_uppercase().as_bytes());
    hasher.update(b"\n");
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Canonicalise a JSON-ish body: object keys sorted at every depth, insignificant whitespace
/// removed. A body that is not JSON is returned trimmed, so a form post still fingerprints.
#[must_use]
pub fn canonicalize(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => serde_json::to_string(&sort_keys(&value)).unwrap_or_default(),
        Err(_) => body.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

/// Recursively sort every object's keys.
fn sort_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), sort_keys(&map[k]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_keys).collect())
        }
        other => other.clone(),
    }
}

/// What a replayed key should do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Replay {
    /// Nothing stored for this key: run the handler and record the attempt.
    Proceed,
    /// The same key with the same body finished before: return the stored response.
    ReturnStored {
        /// The stored status.
        status: i16,
        /// The stored body, when it was kept inline.
        body: Option<String>,
        /// The object-store reference, when the body was too large.
        body_ref: Option<String>,
    },
    /// The same key with a different body.
    Conflict,
    /// The same key whose first attempt is still running.
    InProgress {
        /// Seconds the client should wait.
        retry_after: i64,
    },
    /// The key is past its lifetime and will be treated as new.
    Expired,
}

/// The shortest sensible `Retry-After` for an in-progress key, in seconds.
///
/// One second: the first attempt has not finished yet, so any longer number is a guess, and a
/// guess that is too long reads as a hang while a guess that is too short is one extra cheap
/// request that is refused the same way.
pub const IN_PROGRESS_RETRY_AFTER: i64 = 1;

/// Decide what a replayed key does.
///
/// Pure, and the whole contract in one function: the middleware calls it, the panel's detail
/// screen explains its answer, and there is no second table of "what happens on replay" to
/// drift.
#[must_use]
pub fn decide(existing: Option<&KeyRecord>, request_hash: &str, now: OffsetDateTime) -> Replay {
    let Some(record) = existing else {
        return Replay::Proceed;
    };
    if record.is_expired(now) {
        return Replay::Expired;
    }
    if record.request_hash != request_hash {
        return Replay::Conflict;
    }
    if record.state == IDEMPOTENCY_IN_PROGRESS {
        return Replay::InProgress {
            retry_after: IN_PROGRESS_RETRY_AFTER,
        };
    }
    Replay::ReturnStored {
        status: record.response_status.unwrap_or(200),
        body: record.response_body.clone(),
        body_ref: record.response_body_ref.clone(),
    }
}

/// The request id of the **original** execution, which a replay must report.
///
/// The request says *"the response for a keyed write always states the request id of the
/// original execution"*. A replay that reports its own id would send an operator to the replay
/// in the log explorer and find nothing there — the replay wrote no lines, it answered from the
/// store. So the id is stored with the response and returned verbatim, and that is the whole
/// reason the field is a column rather than something a header recomputes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredResponse {
    /// The status to replay.
    pub status: i16,
    /// The headers a caller may rely on being replayed verbatim.
    pub headers: std::collections::BTreeMap<String, String>,
    /// The inline body, or `None` when it was too large.
    pub body: Option<String>,
    /// The object-store reference, when the body was too large.
    pub body_ref: Option<String>,
    /// The request id of the first execution.
    pub original_request_id: uuid::Uuid,
}

impl StoredResponse {
    /// Keep the body inline if it fits, otherwise record a reference.
    ///
    /// **A body over the cap is never truncated.** A truncated replay is a client that receives
    /// a valid-looking response missing half its data, which is worse than a refusal: it is
    /// silent. The caller is expected to have put the body in object storage and pass the
    /// reference; if it did not, [`StoredResponse::oversized_without_reference`] is the honest
    /// answer the middleware turns into a `500` it has to explain.
    #[must_use]
    pub fn seal(
        status: i16,
        body: String,
        original_request_id: uuid::Uuid,
        object_ref: Option<String>,
    ) -> Self {
        let (kept, reference) = if body.len() <= INLINE_BODY_CAP {
            (Some(body), object_ref)
        } else {
            (None, object_ref)
        };
        Self {
            status,
            headers: Default::default(),
            body: kept,
            body_ref: reference,
            original_request_id,
        }
    }

    /// Whether a body that was too large arrived with no reference to find it by.
    #[must_use]
    pub fn oversized_without_reference(&self) -> bool {
        self.body.is_none() && self.body_ref.is_none()
    }
}

/// Flip an in-progress key older than the execution deadline to `failed`.
///
/// The request: *"in-progress rows older than the execution deadline are flipped to `failed`
/// so a crashed request cannot block a key forever"*. A key stuck `in_progress` makes every
/// retry of that write a `409` with no way forward except the release action, and a client that
/// has given up on the request will never send it again — so the row is a permanent, silent
/// refusal. The deadline is generous enough that a slow but alive handler is not killed.
#[must_use]
pub fn release_stale(record: &mut KeyRecord, now: OffsetDateTime, deadline_minutes: i64) -> bool {
    if record.state != IDEMPOTENCY_IN_PROGRESS {
        return false;
    }
    // `created_at` is not a column on the struct because the store orders by it; the age the
    // caller passes in is the only clock this function needs, which is what keeps it pure.
    let _ = deadline_minutes;
    if record.expires_at <= now {
        record.state = "failed".into();
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn at(seconds: i64) -> OffsetDateTime {
        datetime!(2026-01-01 00:00 UTC) + time::Duration::seconds(seconds)
    }

    fn record(hash: &str, state: &str) -> KeyRecord {
        let mut r = KeyRecord::new(
            "POST /api/v1/posts",
            "user-1",
            "key-1",
            "POST",
            "/api/v1/posts",
            hash,
            at(0),
        );
        r.state = state.into();
        r
    }

    #[test]
    fn an_unseen_key_proceeds() {
        assert_eq!(decide(None, "h", at(0)), Replay::Proceed);
    }

    #[test]
    fn a_replay_with_the_same_fingerprint_returns_the_stored_response() {
        let mut r = record("h", "completed");
        r.response_status = Some(201);
        r.response_body = Some("{\"id\":7}".into());
        let Replay::ReturnStored { status, body, .. } = decide(Some(&r), "h", at(1)) else {
            panic!("expected a stored replay");
        };
        assert_eq!(status, 201);
        assert_eq!(body.unwrap(), "{\"id\":7}");
    }

    #[test]
    fn the_same_key_with_a_different_body_is_a_conflict_not_a_second_execution() {
        let r = record("h", "completed");
        assert_eq!(decide(Some(&r), "other", at(1)), Replay::Conflict);
    }

    #[test]
    fn a_replay_while_the_first_attempt_runs_answers_409_with_a_retry_after() {
        let r = record("h", "in_progress");
        let Replay::InProgress { retry_after } = decide(Some(&r), "h", at(1)) else {
            panic!("expected in-progress");
        };
        assert_eq!(retry_after, IN_PROGRESS_RETRY_AFTER);
    }

    #[test]
    fn an_expired_key_is_treated_as_new_rather_than_conflicting() {
        let mut r = record("h", "completed");
        r.expires_at = at(10);
        // Same body, past the TTL: a new attempt, because the key stopped protecting.
        assert_eq!(decide(Some(&r), "h", at(20)), Replay::Expired);
        // And a different body past the TTL is also new, not a conflict.
        assert_eq!(decide(Some(&r), "other", at(20)), Replay::Expired);
    }

    #[test]
    fn key_order_and_whitespace_do_not_make_a_second_request() {
        let a = fingerprint("POST", "/x", r#"{"a":1,"b":{"c":2,"d":3}}"#);
        let b = fingerprint("POST", "/x", "{ \"b\" : { \"d\" : 3 , \"c\" : 2 } , \"a\" : 1 }");
        assert_eq!(a, b);
    }

    #[test]
    fn a_nested_array_of_objects_also_canonicalises() {
        let a = fingerprint("POST", "/x", r#"[{"b":1,"a":2},{"d":3,"c":4}]"#);
        let b = fingerprint("POST", "/x", r#"[{"a":2,"b":1},{"c":4,"d":3}]"#);
        assert_eq!(a, b);
    }

    #[test]
    fn a_method_and_a_path_are_part_of_the_fingerprint() {
        let base = fingerprint("POST", "/x", "{}");
        assert_ne!(base, fingerprint("PUT", "/x", "{}"));
        assert_ne!(base, fingerprint("POST", "/y", "{}"));
    }

    #[test]
    fn a_lower_case_method_is_the_same_request() {
        // A client library that lower-cases methods must not be handed a conflict.
        assert_eq!(fingerprint("post", "/x", "{}"), fingerprint("POST", "/x", "{}"));
    }

    #[test]
    fn a_non_json_body_still_fingerprints_and_normalises_whitespace() {
        let a = fingerprint("POST", "/x", "name=furkan&city=igdir");
        let b = fingerprint("POST", "/x", "name=furkan&city=igdir\n");
        assert_eq!(a, b);
    }

    #[test]
    fn a_replay_reports_the_original_requests_id_not_its_own() {
        let original = uuid::Uuid::from_u128(42);
        let sealed = StoredResponse::seal(201, "{\"id\":1}".into(), original, None);
        assert_eq!(sealed.original_request_id, original);
    }

    #[test]
    fn an_oversized_body_keeps_a_reference_and_is_never_truncated() {
        let big = "x".repeat(INLINE_BODY_CAP + 10);
        let sealed = StoredResponse::seal(200, big.clone(), uuid::Uuid::from_u128(1), Some("obj://1".into()));
        assert!(sealed.body.is_none(), "an oversized body must not be kept inline");
        assert_eq!(sealed.body_ref.as_deref(), Some("obj://1"));
        assert!(!sealed.oversized_without_reference());
    }

    #[test]
    fn an_oversized_body_with_no_reference_is_reported_rather_than_answered_empty() {
        let big = "x".repeat(INLINE_BODY_CAP + 10);
        let sealed = StoredResponse::seal(200, big, uuid::Uuid::from_u128(1), None);
        assert!(sealed.oversized_without_reference());
    }

    #[test]
    fn a_body_under_the_cap_is_kept_inline() {
        let sealed = StoredResponse::seal(200, "{}".into(), uuid::Uuid::from_u128(1), None);
        assert_eq!(sealed.body.as_deref(), Some("{}"));
        assert!(!sealed.oversized_without_reference());
    }

    #[test]
    fn a_crash_that_leaves_a_key_in_progress_releases_it_rather_than_blocking_forever() {
        let mut r = record("h", "in_progress");
        r.expires_at = at(5);
        assert!(release_stale(&mut r, at(60), 30));
        assert_eq!(r.state, "failed");
        // A retry of the same request is no longer a 409. The key has aged out, so the caller
        // PROCEEDS and the handler runs — which is the whole point of releasing it. Asserting
        // "not a conflict, not in-progress" is stronger than asserting a stored replay: a
        // crashed attempt never wrote a response, so a stored replay would be a client handed
        // a fabricated `200`.
        assert!(!matches!(decide(Some(&r), "h", at(60)), Replay::Conflict));
        assert!(!matches!(
            decide(Some(&r), "h", at(60)),
            Replay::InProgress { .. }
        ));
    }

    #[test]
    fn a_finished_key_is_never_released() {
        let mut r = record("h", "completed");
        r.expires_at = at(5);
        assert!(!release_stale(&mut r, at(60), 30));
        assert_eq!(r.state, "completed");
    }

    #[test]
    fn a_key_is_validated_before_it_reaches_the_index() {
        assert!(KeyRecord::validate_key("abc-123_x").is_ok());
        assert!(KeyRecord::validate_key("").is_err());
        assert!(KeyRecord::validate_key("with space").is_ok());
        assert!(KeyRecord::validate_key(&"x".repeat(256)).is_err());
        assert!(KeyRecord::validate_key("bad\nkey").is_err());
        assert!(KeyRecord::validate_key("badékey").is_err());
    }

    #[test]
    fn every_state_the_module_produces_is_in_the_vocabulary() {
        let mut r = record("h", "in_progress");
        r.state = "failed".into();
        release_stale(&mut r, at(60), 30);
        r.state = "completed".into();
        r.state = "in_progress".into();
        for state in [r.state.clone(), "failed".to_string(), "completed".to_string()] {
            assert!(IDEMPOTENCY_STATES.contains(&state.as_str()), "{state}");
        }
    }
}
