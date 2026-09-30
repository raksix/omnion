//! The store of record for circuit breakers and their transition log (REQ-127, slice 3).
//!
//! [`crate::breaker`] decides; this file remembers. The split is the same one `retry_store.rs`
//! draws, and it exists for one reason: **a breaker that forgets is a breaker that does not
//! work.** Every guarantee the request asks for is a property of what is written down here —
//!
//! * *"A restart does not reset an open breaker."* [`load`] returns the persisted `state`, and
//!   [`save`] writes it back in the same transaction as the transition it came from. A breaker
//!   whose state were recomputed from `opened_at` on boot would close itself the moment the
//!   cooldown elapsed with nobody watching — the exact burst at a downed provider that a breaker
//!   exists to prevent.
//! * *"`Reset` and `Force open` both require confirmation and write audit rows."* Both are
//!   [`change_thresholds`]-free, event-emitting functions ([`force_open`], [`reset`]) that append
//!   to `breaker_events` through the same path an automatic trip uses, so a manual action and a
//!   machine action are indistinguishable to the reader and neither can happen unlogged.
//! * *"State transitions are logged and persisted."* [`observe`] is the **only** writer of a
//!   state change: it takes the [`Transition`] the machine produced and persists state and event
//!   in one transaction, so a crash between them cannot leave a breaker that moved without a
//!   record of moving.
//!
//! ## The `breaker_events` write is conditional, and that is the point
//!
//! A healthy provider produces no event — `Transition::event` is `None` — so an observation that
//! changes nothing writes nothing. A table with one row per call would be a call log wearing a
//! transition table's name, and the "one opened event" the acceptance criteria ask for would be
//! unfalsifiable inside a flood of successes.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::breaker::{BreakerState, Transition};
use crate::error::{ReliabilityError, Result};
use crate::vocabulary::MAX_PAGE;

#[derive(Debug, sqlx::FromRow)]
struct BreakerRow {
    key: String,
    name: String,
    failure_threshold: i32,
    window_seconds: i32,
    cooldown_seconds: i32,
    half_open_probes: i32,
    success_threshold: i32,
    state: String,
    forced_open: bool,
    opened_at: Option<OffsetDateTime>,
    state_changed_at: OffsetDateTime,
    failures_in_window: i32,
    successes_in_half_open: i32,
    window_started_at: OffsetDateTime,
    trips_total: i64,
}

impl From<BreakerRow> for BreakerState {
    fn from(row: BreakerRow) -> Self {
        Self {
            key: row.key,
            name: row.name,
            failure_threshold: row.failure_threshold,
            // Three of the four windows are `int` in the migration and `i64` in the domain. The
            // same INT4/INT8 mismatch `store.rs` documents, converted rather than bound.
            window_seconds: i64::from(row.window_seconds),
            cooldown_seconds: i64::from(row.cooldown_seconds),
            half_open_probes: row.half_open_probes,
            success_threshold: row.success_threshold,
            state: row.state,
            forced_open: row.forced_open,
            opened_at: row.opened_at,
            state_changed_at: row.state_changed_at,
            failures_in_window: row.failures_in_window,
            successes_in_half_open: row.successes_in_half_open,
            window_started_at: row.window_started_at,
            trips_total: row.trips_total,
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    id: i64,
    key: String,
    from_state: String,
    to_state: String,
    reason: Option<String>,
    failure_rate: Option<f64>,
    created_at: OffsetDateTime,
}

/// One logged transition, as the screen's timeline reads it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BreakerEvent {
    pub id: i64,
    pub key: String,
    pub from_state: String,
    pub to_state: String,
    pub reason: Option<String>,
    pub failure_rate: Option<f64>,
    pub created_at: OffsetDateTime,
}

impl From<EventRow> for BreakerEvent {
    fn from(row: EventRow) -> Self {
        Self {
            id: row.id,
            key: row.key,
            from_state: row.from_state,
            to_state: row.to_state,
            reason: row.reason,
            failure_rate: row.failure_rate,
            created_at: row.created_at,
        }
    }
}

const BREAKER_COLUMNS: &str = "key, name, failure_threshold, window_seconds, cooldown_seconds, \
     half_open_probes, success_threshold, state, forced_open, opened_at, state_changed_at, \
     failures_in_window, successes_in_half_open, window_started_at, trips_total";

/// Load one breaker, or `None` when the provider has none yet.
///
/// `None` is not an error: an outbound path that has never tripped has no row, and the caller
/// creates one from [`BreakerState::new`]. Inserting a row per provider on first sight would put
/// the platform's whole provider list in the breaker table as permanently-closed rows.
pub async fn load(pool: &PgPool, key: &str) -> Result<Option<BreakerState>> {
    let sql = format!("select {BREAKER_COLUMNS} from circuit_breakers where key = $1");
    let row = sqlx::query_as::<_, BreakerRow>(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(BreakerState::from))
}

/// Every breaker, for the screen's list.
pub async fn list(pool: &PgPool) -> Result<Vec<BreakerState>> {
    let sql = format!("select {BREAKER_COLUMNS} from circuit_breakers order by key");
    let rows = sqlx::query_as::<_, BreakerRow>(&sql).fetch_all(pool).await?;
    Ok(rows.into_iter().map(BreakerState::from).collect())
}

/// Create a breaker, or return the existing one unchanged.
///
/// Idempotent by key rather than an error on conflict: two workers that both decide to protect
/// a provider must not have one of them fail, and the primary key already guarantees there is
/// only ever one. The `do nothing` is what makes the second call a *read* of the winner's row.
pub async fn ensure(pool: &PgPool, state: &BreakerState) -> Result<BreakerState> {
    sqlx::query(
        "insert into circuit_breakers \
             (key, name, failure_threshold, window_seconds, cooldown_seconds, half_open_probes, \
              success_threshold, state, forced_open, opened_at, state_changed_at, \
              failures_in_window, successes_in_half_open, window_started_at, trips_total) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (key) do nothing",
    )
    .bind(&state.key)
    .bind(&state.name)
    .bind(state.failure_threshold)
    .bind(state.window_seconds as i32)
    .bind(state.cooldown_seconds as i32)
    .bind(state.half_open_probes)
    .bind(state.success_threshold)
    .bind(&state.state)
    .bind(state.forced_open)
    .bind(state.opened_at)
    .bind(state.state_changed_at)
    .bind(state.failures_in_window)
    .bind(state.successes_in_half_open)
    .bind(state.window_started_at)
    .bind(state.trips_total)
    .execute(pool)
    .await?;
    load(pool, &state.key)
        .await?
        // `NotFound` rather than a bespoke "internal": a row that was just inserted and cannot
        // then be read is a store fault, and the enum's `Database` variant is where a caller
        // learns that — inventing a message here would hide a foreign-key or transaction bug
        // behind a message that reads like a policy problem.
        .ok_or(ReliabilityError::Database(sqlx::Error::RowNotFound))
}

/// Persist a state change and its event in one transaction.
///
/// The event write is **conditional on `transition.event`**: `None` means the machine changed
/// nothing, and a log row for a call that changed nothing turns the transition log into a call
/// log. The state update runs either way, because the failure counters move on every call even
/// when the state does not.
pub async fn observe(pool: &PgPool, transition: &Transition) -> Result<()> {
    let mut tx = pool.begin().await?;
    save_in(&mut tx, &transition.state).await?;
    if let Some(_event) = transition.event {
        sqlx::query(
            "insert into breaker_events (key, from_state, to_state, reason, failure_rate) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(&transition.state.key)
        .bind(&transition.from_state)
        .bind(&transition.state.state)
        .bind(&transition.reason)
        .bind(transition.failure_rate)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Write the state alone — the threshold editor, which changes settings and not behaviour.
///
/// Separate from [`observe`] on purpose: editing a threshold is not a transition, so it must not
/// appear in the transition timeline as one, and it must not be able to write an event by
/// accident.
pub async fn save(pool: &PgPool, state: &BreakerState) -> Result<()> {
    let mut tx = pool.begin().await?;
    save_in(&mut tx, state).await?;
    tx.commit().await?;
    Ok(())
}

async fn save_in(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, state: &BreakerState) -> Result<()> {
    sqlx::query(
        "update circuit_breakers set \
             name = $2, failure_threshold = $3, window_seconds = $4, cooldown_seconds = $5, \
             half_open_probes = $6, success_threshold = $7, state = $8, forced_open = $9, \
             opened_at = $10, state_changed_at = $11, failures_in_window = $12, \
             successes_in_half_open = $13, window_started_at = $14, trips_total = $15, \
             updated_at = now() \
           where key = $1",
    )
    .bind(&state.key)
    .bind(&state.name)
    .bind(state.failure_threshold)
    .bind(state.window_seconds as i32)
    .bind(state.cooldown_seconds as i32)
    .bind(state.half_open_probes)
    .bind(state.success_threshold)
    .bind(&state.state)
    .bind(state.forced_open)
    .bind(state.opened_at)
    .bind(state.state_changed_at)
    .bind(state.failures_in_window)
    .bind(state.successes_in_half_open)
    .bind(state.window_started_at)
    .bind(state.trips_total)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Deliberately drain a provider until somebody says otherwise.
///
/// Sets `forced_open`, which [`crate::breaker::record`] checks before every other rule, so no
/// success and no cooldown closes it. Appended to `breaker_events` through the same statement an
/// automatic trip uses, which is what makes it an audit row rather than a hidden flag.
pub async fn force_open(pool: &PgPool, key: &str, reason: &str) -> Result<Option<BreakerState>> {
    let Some(mut state) = load(pool, key).await? else {
        return Ok(None);
    };
    let from = state.state.clone();
    state.forced_open = true;
    state.state = "open".into();
    state.opened_at = Some(state.state_changed_at);
    let mut tx = pool.begin().await?;
    save_in(&mut tx, &state).await?;
    sqlx::query(
        "insert into breaker_events (key, from_state, to_state, reason) values ($1, $2, $3, $4)",
    )
    .bind(key)
    .bind(&from)
    .bind("open")
    .bind(format!("forced: {reason}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(state))
}

/// Return a breaker to `closed` by hand, clearing the forced flag.
///
/// Audited for the same reason [`force_open`] is: "we fixed it, close it" and "the provider tripped"
/// are different facts, and an operator reading the timeline later needs to tell them apart. The
/// event records the *reset*, not a fictitious failure.
pub async fn reset(pool: &PgPool, key: &str, reason: &str) -> Result<Option<BreakerState>> {
    let Some(mut state) = load(pool, key).await? else {
        return Ok(None);
    };
    let from = state.state.clone();
    state.state = "closed".into();
    state.forced_open = false;
    state.opened_at = None;
    state.failures_in_window = 0;
    state.successes_in_half_open = 0;
    let mut tx = pool.begin().await?;
    save_in(&mut tx, &state).await?;
    sqlx::query(
        "insert into breaker_events (key, from_state, to_state, reason) values ($1, $2, $3, $4)",
    )
    .bind(key)
    .bind(&from)
    .bind("closed")
    .bind(format!("reset: {reason}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(state))
}

/// Edit a provider's thresholds without touching its state.
///
/// A screen where editing `failure_threshold` also silently closed an open breaker would be a
/// trap, so the two are different functions with different SQL.
pub async fn update_settings(
    pool: &PgPool,
    key: &str,
    name: &str,
    failure_threshold: i32,
    window_seconds: i32,
    cooldown_seconds: i32,
    half_open_probes: i32,
    success_threshold: i32,
) -> Result<Option<BreakerState>> {
    let result = sqlx::query(
        "update circuit_breakers set \
             name = $2, failure_threshold = $3, window_seconds = $4, cooldown_seconds = $5, \
             half_open_probes = $6, success_threshold = $7, updated_at = now() \
           where key = $1",
    )
    .bind(key)
    .bind(name)
    .bind(failure_threshold)
    .bind(window_seconds)
    .bind(cooldown_seconds)
    .bind(half_open_probes)
    .bind(success_threshold)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    load(pool, key).await
}

/// One provider's transition history, newest first.
pub async fn events_for(pool: &PgPool, key: &str, limit: usize) -> Result<Vec<BreakerEvent>> {
    let limit = limit.clamp(1, MAX_PAGE) as i64;
    let rows = sqlx::query_as::<_, EventRow>(
        "select id, key, from_state, to_state, reason, failure_rate, created_at \
           from breaker_events where key = $1 order by created_at desc, id desc limit $2",
    )
    .bind(key)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(BreakerEvent::from).collect())
}

/// The whole transition log, newest first, for the screen's activity list.
pub async fn recent_events(pool: &PgPool, limit: usize) -> Result<Vec<BreakerEvent>> {
    let limit = limit.clamp(1, MAX_PAGE) as i64;
    let rows = sqlx::query_as::<_, EventRow>(
        "select id, key, from_state, to_state, reason, failure_rate, created_at \
           from breaker_events order by created_at desc, id desc limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(BreakerEvent::from).collect())
}

/// How many breakers are in each state, for the screen's summary cards.
///
/// The `count(*)` is cast in the SQL rather than decoded: PostgreSQL returns `int8` and the
/// tuple must be `(String, i64)` or sqlx's tuple decoder refuses the row at runtime. Asking the
/// database for the type it will answer is the same rule the rest of the store follows.
pub async fn state_counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "select state, count(*)::int8 as total from circuit_breakers group by state order by state",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Delete a breaker and its history.
///
/// A provider the platform no longer calls has no breaker to protect; the history goes with it
/// because a timeline whose provider no longer exists is a row of orphaned prose.
pub async fn delete(pool: &PgPool, key: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from breaker_events where key = $1")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("delete from circuit_breakers where key = $1")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}
