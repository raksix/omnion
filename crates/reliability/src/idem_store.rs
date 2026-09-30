//! The idempotency KEY STORE — the persistence half of slice 2 (REQ-127).
//!
//! [`crate::idempotency`] holds the pure decision function ([`crate::idempotency::decide`]) and
//! the types; this module holds the writes that make the decision safe under concurrency. Both
//! halves are needed and neither is sufficient:
//!
//! - `decide` answers "what should this request do", given a record. Pure, exhaustive, unit
//!   tested, no database.
//! - this module answers "may I claim this key", which is a **race**, not a decision.
//!
//! ## The race is the whole reason this module exists
//!
//! Two clients send the same key in the same millisecond. Both read, both see nothing, both run
//! the handler — and the idempotency guarantee is void. A read-then-write cannot prevent it,
//! because the read is not part of the decision the database makes. So the claim is a single
//! `INSERT ... ON CONFLICT DO UPDATE ... WHERE <expired>` and the answer is `rows_affected`:
//!
//! ```text
//! claim → 1 row  → nobody has this key        → Proceed, and this caller owns the attempt
//! claim → 0 rows → somebody has it           → decide(record, hash, now) decides what you do
//! ```
//!
//! Reading first and then inserting is the shape that loses the race. **`rows_affected` is the
//! answer**, and it is the only part of this module that needs the database to be correct.
//!
//! ## Why the loser re-reads
//!
//! The insert that loses may lose *after* the winner already committed `completed`, or against a
//! row still `in_progress`. `decide` needs the CURRENT state, so the loser re-reads and then
//! decides. Two callers cannot both proceed because only one can insert; they cannot both replay
//! because there is only one row.
//!
//! ## Why an expired key is overwritten rather than deleted first
//!
//! Deleting then inserting opens a window: between the two statements another caller sees no row,
//! inserts, and now two callers believe they own the key. The single upsert has no such window —
//! the unique index and the expiry predicate are evaluated in ONE statement, so the key is freed
//! and taken atomically.
//!
//! ## Why a refused request never consumes a key (contract point 5)
//!
//! The API layer claims a key **after** authentication and permission, and never rolls the claim
//! back on a refusal. A caller who may not make the write must not be able to burn the key the
//! caller who can is using, and rolling back would let a refused request delete the winner's
//! `in_progress` row. [`release_stale`] exists for the opposite case — an attempt that CRASHED —
//! and not as an undo.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{ReliabilityError, Result};
use crate::idempotency::{KeyRecord, StoredResponse, INLINE_BODY_CAP};
use crate::vocabulary::{
    IDEMPOTENCY_COMPLETED, IDEMPOTENCY_FAILED, IDEMPOTENCY_IN_PROGRESS,
};

/// Columns of `idempotency_keys`, in query order.
///
/// Spelled out rather than `select *` on purpose: `*` lets a migration add a column and have the
/// struct either stop compiling (loud, fine) or silently keep a position (a query returning the
/// wrong value into a named field, which is the case that matters). Every `select` here uses this
/// list in this order.
const COLUMNS: &str = "scope, subject_id, key, method, path, request_hash, state, \
                       response_status, response_headers, response_body, response_body_ref, \
                       replay_count, expires_at, completed_at";

/// One row as it comes back from PostgreSQL.
///
/// `response_headers` is read and dropped: `KeyRecord` deliberately does not carry headers,
/// because the pure `decide` never reads them and widening the record would make the decision
/// function depend on data it has no use for. The stored response's headers travel through
/// [`StoredResponse`] on the API layer instead. The column is still listed so the row shape
/// matches the table and a future reader sees the omission was deliberate.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    scope: String,
    subject_id: String,
    key: String,
    method: String,
    path: String,
    request_hash: String,
    state: String,
    response_status: Option<i16>,
    #[allow(dead_code, reason = "read and discarded on purpose — see the type's doc comment")]
    response_headers: Value,
    /// `response_body` is `jsonb` in the migration and a `String` in `KeyRecord`, so it is read
    /// as a `Value` and rendered back to text. sqlx will NOT narrow a `jsonb` column into an
    /// `Option<String>`: it answers `mismatched types … not compatible with SQL type JSONB` on the
    /// first read of a row that has a body, which is every row a replay depends on. The cast has
    /// to happen on both sides of the store, not only the write side.
    response_body: Option<Value>,
    response_body_ref: Option<String>,
    replay_count: i32,
    expires_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
}

impl From<Row> for KeyRecord {
    fn from(row: Row) -> Self {
        Self {
            scope: row.scope,
            subject_id: row.subject_id,
            key: row.key,
            method: row.method,
            path: row.path,
            request_hash: row.request_hash,
            state: row.state,
            response_status: row.response_status,
            // A body that was stored as a JSON string comes back as a JSON string, and one that
            // was stored as a document comes back as a document. Rendering the VALUE rather than
            // unwrapping a string is what keeps a `201` that returned an object replaying as an
            // object rather than as a quoted string.
            response_body: row.response_body.as_ref().map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }),
            response_body_ref: row.response_body_ref,
            replay_count: row.replay_count,
            expires_at: row.expires_at,
            completed_at: row.completed_at,
        }
    }
}

/// What happened when a caller asked to claim a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// This call inserted the row: nobody had the key, and this caller owns the attempt.
    Claimed,
    /// The key already existed. The record is the CURRENT one, re-read after the lost race.
    Existing(Box<KeyRecord>),
}

/// Load one key, or `None` when this scope and subject have never used it.
pub async fn find(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
) -> Result<Option<KeyRecord>> {
    let sql = format!("select {COLUMNS} from idempotency_keys \
                       where scope = $1 and subject_id = $2 and key = $3");
    let row: Option<Row> = sqlx::query_as(&sql)
        .bind(scope)
        .bind(subject_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(KeyRecord::from))
}

/// Claim a key for an attempt.
///
/// Returns [`Claim::Claimed`] when this call inserted the row and [`Claim::Existing`] — carrying
/// the freshly re-read record — when it did not. **An existing record is not an error**: it is
/// the other two thirds of the contract, and the caller decides with
/// [`crate::idempotency::decide`].
///
/// The insert is deliberately not wrapped in a transaction. The attempt runs for as long as the
/// handler runs, and holding a transaction open for the length of a handler is how a pool dies.
/// The durability this needs is the row itself: `in_progress` already means "claimed, not
/// finished", and a crashed attempt leaves exactly that row for [`release_stale`] to release.
pub async fn claim(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
    method: &str,
    path: &str,
    request_hash: &str,
    now: OffsetDateTime,
) -> Result<Claim> {
    // The takeover predicate is the whole correctness of this statement, and it is written
    // against `now` ($9) rather than against the expiry this statement is about to write ($8).
    //
    // The first version compared `idempotency_keys.expires_at <= $8`, where $8 is
    // `now + DEFAULT_TTL_HOURS`. Every live row satisfies that — a row's expiry is always in the
    // past relative to a *later* stamp — so the `do update` fired on every claim and **eight of
    // eight concurrent claims all returned `Claim::Claimed`**. The unique index never got a
    // chance to arbitrate, because the upsert had already turned the conflict into an update.
    // The concurrency walk caught it on its first run; no reading of the code finds it, because
    // "expires_at <= the value I am writing" reads as obviously true and is obviously wrong.
    //
    // A `failed` row is taken over as well: its attempt finished without an answer, so the
    // retry is entitled to the key and the old row is exactly what must not block it.
    let result = sqlx::query(
        "insert into idempotency_keys \
           (scope, subject_id, key, method, path, request_hash, state, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (scope, subject_id, key) do update \
           set method      = excluded.method, \
               path        = excluded.path, \
               request_hash = excluded.request_hash, \
               state        = excluded.state, \
               response_status   = null, \
               response_headers  = '{}'::jsonb, \
               response_body     = null, \
               response_body_ref = null, \
               replay_count      = 0, \
               completed_at      = null, \
               expires_at        = excluded.expires_at \
         where idempotency_keys.expires_at <= $9 \
            or idempotency_keys.state = $10",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .bind(method)
    .bind(path)
    .bind(request_hash)
    .bind(IDEMPOTENCY_IN_PROGRESS)
    .bind(now + time::Duration::hours(crate::idempotency::DEFAULT_TTL_HOURS))
    .bind(now)
    .bind(IDEMPOTENCY_FAILED)
    .execute(pool)
    .await?;

    if result.rows_affected() == 1 {
        return Ok(Claim::Claimed);
    }

    // The insert did not take, so a row exists. It is re-read rather than assumed: the loser of
    // a race may have lost it after the winner committed, and `decide` needs the current state.
    let existing = find(pool, scope, subject_id, key)
        .await?
        .expect("a row that blocked the insert exists and cannot have vanished between two \
                 statements in the same request");
    Ok(Claim::Existing(Box::new(existing)))
}

/// Record the outcome of an attempt this caller owned.
///
/// `headers` are stored and replayed verbatim: a caller that stored a `201` body but not its
/// `Location` gets a replay that is a different response, which defeats the point of storing one.
pub async fn complete(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
    response: &StoredResponse,
    now: OffsetDateTime,
) -> Result<()> {
    // There is no truncation branch, and that is the point. A truncated stored body would make a
    // replay a lie; the caller knows whether it wrote the body to the object store, so the
    // inline-versus-reference decision belongs to it. The cap is enforced at seal time by
    // `StoredResponse::seal` and by `oversized_without_reference`.
    debug_assert!(
        !response.oversized_without_reference(),
        "an inline body past {} bytes must carry a reference instead",
        INLINE_BODY_CAP
    );

    // `response_body` is `jsonb` in the migration and the stored body is a STRING here, so the
    // bind needs the cast. Without it PostgreSQL refuses with 42804 naming the column — the row
    // is never written, the caller believes its response is replayable, and a replay finds
    // nothing. sqlx binds `Option<&str>` as text and does not narrow it, so the cast belongs in
    // the SQL where the column's type is visible.
    let body: Option<Value> = response
        .body
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok().or(Some(Value::String(raw.to_owned()))));
    let headers =
        serde_json::to_value(&response.headers).unwrap_or_else(|_| Value::Object(Default::default()));
    let result = sqlx::query(
        "update idempotency_keys \
            set state = $4, \
                response_status = $5, \
                response_headers = $6, \
                response_body = $7, \
                response_body_ref = $8, \
                completed_at = $9 \
          where scope = $1 and subject_id = $2 and key = $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .bind(IDEMPOTENCY_COMPLETED)
    .bind(response.status)
    .bind(headers)
    .bind(body)
    .bind(response.body_ref.as_deref())
    .bind(now)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // Silently succeeding would be a lie: the caller believes its response is now replayable
        // while a replay would find nothing to return. Naming it is the only way the owner of the
        // key can find out before a client discovers it for them.
        // `NotFound` is a UNIT variant on purpose: "no such policy" and "a policy you may not
        // see" must be one answer, or the detail route becomes a probe for what exists. The
        // detail that makes this occurrence diagnosable goes in the log line beside it, not in
        // the error the caller sees.
        tracing::warn!(
            scope,
            subject_id,
            key,
            "an idempotency key vanished while its own attempt was being completed"
        );
        return Err(ReliabilityError::NotFound);
    }
    Ok(())
}

/// Count a replay against a key.
///
/// Best-effort by design: a counter that fails to move must never turn an already-correct replay
/// into an error, and the replay has ALREADY been answered by the time this runs.
pub async fn count_replay(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "update idempotency_keys set replay_count = replay_count + 1 \
          where scope = $1 and subject_id = $2 and key = $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Release an `in_progress` key whose attempt never finished, so the key can be retried.
///
/// Returns whether a row was actually released.
///
/// The predicate repeats the constant rather than a literal, and the state it guards is
/// `in_progress` specifically: releasing a COMPLETED key would destroy a real stored response,
/// and releasing an already-`failed` row would report `true` for work that was not done.
pub async fn release_stale(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let _ = now;
    let result = sqlx::query(
        "update idempotency_keys set state = $4, completed_at = $5 \
          where scope = $1 and subject_id = $2 and key = $3 and state = $6",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .bind(IDEMPOTENCY_FAILED)
    .bind(now)
    .bind(IDEMPOTENCY_IN_PROGRESS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Mark a key `failed` because its attempt finished without a storable answer.
///
/// **This is not an undo and it is not a success.** It is the honest record of a write whose
/// response could not be stored — a body past [`INLINE_BODY_CAP`] with no object-store reference
/// to find it by. The alternatives were both worse: storing a truncated body makes a replay a
/// lie, and leaving the row `in_progress` makes the key permanently unusable because every
/// retry is a `409` with nobody left to release it. `failed` is the only state that says "run it
/// again", and the double execution that can follow is a client-visible retry rather than a
/// silent `200` with half a body.
///
/// The predicate is `state = in_progress` for the same reason [`release_stale`] uses it: a key
/// that already has a stored response must never be overwritten by a second, later attempt.
pub async fn abandon(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    key: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let result = sqlx::query(
        "update idempotency_keys set state = $4, completed_at = $5 \
          where scope = $1 and subject_id = $2 and key = $3 and state = $6",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(key)
    .bind(IDEMPOTENCY_FAILED)
    .bind(now)
    .bind(IDEMPOTENCY_IN_PROGRESS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Delete every key past its lifetime.
///
/// Returns how many rows it removed, so the retention job can log a real number rather than the
/// same line it logs when there was nothing to do.
pub async fn prune(pool: &PgPool, now: OffsetDateTime) -> Result<()> {
    sqlx::query("delete from idempotency_keys where expires_at <= $1")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// The keys a screen lists, newest first.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KeySummary {
    /// The key, as the caller sent it.
    pub key: String,
    /// The endpoint family it protects.
    pub scope: String,
    /// One of `in_progress` / `completed` / `failed`.
    pub state: String,
    /// How many times this key has been replayed.
    pub replay_count: i32,
    /// When the key stops protecting.
    pub expires_at: OffsetDateTime,
    /// When the attempt finished.
    pub completed_at: Option<OffsetDateTime>,
}

/// List a scope's keys, newest first.
pub async fn list(
    pool: &PgPool,
    scope: &str,
    subject_id: &str,
    limit: i64,
) -> Result<Vec<KeySummary>> {
    Ok(sqlx::query_as(
        "select key, scope, state, replay_count, expires_at, completed_at \
           from idempotency_keys \
          where scope = $1 and subject_id = $2 \
          order by created_at desc \
          limit $3",
    )
    .bind(scope)
    .bind(subject_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?)
}

/// How many keys this scope and subject hold. `count(*)` is `int8`, and sqlx will decode it into
/// an `i64` — the `::int` cast a sibling uses for an `i32` decoder is wrong here and fails at
/// runtime rather than at compile time.
pub async fn count(pool: &PgPool, scope: &str, subject_id: &str) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*) from idempotency_keys \
                                         where scope = $1 and subject_id = $2")
        .bind(scope)
        .bind(subject_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}
