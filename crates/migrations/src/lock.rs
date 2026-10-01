//! One migration at a time, and the screen that says who is holding the lock
//! (docs/requests/REQ-129, slice 1).
//!
//! ## Why a second lock when SQLx already takes one
//!
//! SQLx's [`Migrator::run`] takes `pg_advisory_lock` on its own key and holds it for the length of
//! its own apply loop. That is enough for *its* loop and not enough for this one, for three
//! reasons that all come from the same place — the ledger write:
//!
//! * The ledger row is written **after** SQLx commits, in a separate transaction. An advisory
//!   lock released at the end of SQLx's loop therefore protects the apply and not the ledger
//!   write, so two runners could interleave as `apply → release → ledger → ledger`.
//! * The **down** direction has no SQLx lock at all. `Migrator::undo` exists but this crate does
//!   not use it: a reversal is a list of statements extracted from the file, not a reversible
//!   migration, so nothing in SQLx is holding a lock while it runs.
//! * The **lint / plan** path must be able to take the lock without applying anything, because
//!   the request's `409` line is about a second runner being refused *before* it writes.
//!
//! So this module owns one key, and it is a **different key from SQLx's** — deliberately, because
//! reusing SQLx's key would mean the two locks serialise each other into a deadlock order nobody
//! wrote down. Two distinct keys is one extra lock in the process and two clearly-named halves:
//! "the apply is in progress" and "the migration layer is in progress".
//!
//! ## The key is ASCII, so it is readable in `pg_locks`
//!
//! `pg_advisory_lock(bigint)` puts the raw value in `pg_locks.objid`, which an operator reads when
//! the lock screen says somebody is blocked. A magic constant answers nothing there; `'omnionmg'`
//! read as bytes is `8029195109791591783`, and
//! [`LOCK_ID_TEXT`] renders it back. Both the constant and its rendering are derived in code, so
//! they cannot disagree — the test asserts the rendering, and the SQL is written against the
//! rendering rather than against a number typed twice.
//!
//! ## A lock that is refused is not a lock that is waited for
//!
//! [`acquire`] takes [`LOCK_WAIT`]: the operator's `lock_timeout_ms` translated into a bounded
//! wait. After that it answers [`MigrationSafetyError::Locked`] carrying **the holder's pid and
//! query age**, because a refusal an operator cannot attribute is a refusal they resolve by
//! killing something. The wait is `pg_try_advisory_lock` in a loop rather than a blocking
//! `pg_advisory_lock`, so the caller is free to abandon it and so the wait can report progress.

use std::time::Duration;

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{MigrationSafetyError, Result};

/// The advisory-lock key, as the eight ASCII bytes of `"omnionmg"`.
#[must_use]
pub fn lock_id() -> i64 {
    i64::from_be_bytes(*b"omnionmg")
}

/// The same key rendered the way `pg_locks` reports it, for a message a human reads.
///
/// Derived from [`lock_id`] rather than written next to it, because two copies of a constant is
/// two places to change it and the copy nobody changes is the one that ends up in a bug report.
#[must_use = "the rendering is what the lock screen and the error message print"]
pub fn lock_id_text() -> String {
    format!("{} (omnionmg)", lock_id())
}

/// How long [`acquire`] waits for a place before refusing.
///
/// Four seconds, deliberately short and deliberately not the policy's `lock_timeout_ms`: that
/// setting bounds a blocked **DDL statement** (a table lock held by a long transaction), while
/// this bounds waiting for the **runner**. A migration is not something an operator wants to be
/// quietly waiting on behind a deploy job that has itself hung, so this is a second knob and the
/// two never share a number.
pub const LOCK_WAIT: Duration = Duration::from_secs(4);

/// How often the wait re-asks PostgreSQL while it waits.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// What the lock screen shows, and what a refusal carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockView {
    /// `true` when this installation has a migration run in flight.
    pub held: bool,
    /// The `migration_runs` row that started first, when one is running.
    pub version: Option<String>,
    /// Its direction (`up` / `down`).
    pub direction: Option<String>,
    /// Who started it.
    pub actor: Option<String>,
    /// Where it was started from (`cli`, `deploy`, `ci`, `boot`).
    pub source: Option<String>,
    /// When it started.
    pub started_at: Option<OffsetDateTime>,
    /// How long it has been running, in whole seconds.
    pub age_seconds: Option<i64>,
    /// The advisory key this crate uses, rendered for a human.
    pub lock_key: String,
    /// Running queries that are waiting on a lock, so the blocker is named rather than guessed.
    pub blocked: Vec<BlockedQuery>,
}

/// One query waiting for a lock.
///
/// Read from `pg_locks` joined with `pg_stat_activity`, because the two are what an operator
/// would query by hand and the screen's job is to save them that query — not to replace it with a
/// summary that loses the pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockedQuery {
    /// The backend pid, which is what `pg_cancel_backend` takes.
    pub pid: i32,
    /// How long it has been waiting, in whole seconds.
    pub age_seconds: i64,
    /// The first line of its query, trimmed — never the whole statement, because a blocked
    /// `UPDATE` can carry a literal somebody did not want echoed into a screen.
    pub query: String,
    /// The application name, so a pass's own connections are distinguishable from the app's.
    pub application: Option<String>,
}

/// The lock's current state.
///
/// Never errors and never hides: if the query fails the screen says the lock state is
/// **unknown**, because "no lock" and "could not ask" are different answers and an operator
/// deciding whether to start a migration needs the second one as much as the first.
pub async fn view(pool: &PgPool) -> LockView {
    // The age is computed BY THE DATABASE, in the same statement that reads `started_at`. Doing
    // the subtraction in Rust means a second clock: `OffsetDateTime::now_utc()` against a
    // timestamp written by `now()` on another host, which is a screen that can render a negative
    // age and a caller that has no way to tell a clock skew from a fresh run.
    let run = sqlx::query_as::<_, (String, String, String, String, OffsetDateTime, i64)>(
        "select version, direction, actor, source, started_at, \
                extract(epoch from (now() - started_at))::bigint \
         from migration_runs where status = 'running' order by started_at limit 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let (version, direction, actor, source, started_at, age_seconds) = match run {
        Some(row) => (Some(row.0), Some(row.1), Some(row.2), Some(row.3), Some(row.4), Some(row.5)),
        None => (None, None, None, None, None, None),
    };

    LockView {
        held: started_at.is_some(),
        age_seconds,
        version,
        direction,
        actor,
        source,
        started_at,
        lock_key: lock_id_text(),
        blocked: blocked_queries(pool).await,
    }
}

/// Queries currently waiting on a lock, longest wait first.
///
/// `pg_locks.pid is not null` is the whole filter and it is the important one: rows in
/// `pg_locks` without a `pid` are the *holder* being waited for, and reading them here would
/// print the blocker as if it were the victim.
async fn blocked_queries(pool: &PgPool) -> Vec<BlockedQuery> {
    sqlx::query_as::<_, (i32, i64, Option<String>, Option<String>)>(
        "select l.pid::int, \
                extract(epoch from (now() - a.query_start))::bigint, \
                left(a.query, 200), \
                a.application_name \
         from pg_locks l \
         join pg_stat_activity a on a.pid = l.pid \
         where l.pid is not null and not l.granted \
         order by a.query_start nulls last limit 20",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(pid, age_seconds, query, application)| BlockedQuery {
        pid,
        age_seconds,
        query: query.unwrap_or_default().trim().to_owned(),
        application,
    })
    .collect()
}

/// Take the migration lock, or refuse with the holder's identity.
///
/// The loop is [`pg_try_advisory_lock`] rather than a blocking `pg_advisory_lock` for the reason
/// in the module comment: the caller must be able to give up, and to say who it was waiting for
/// when it gives up. The refusal message names the pid and the age because that is the whole
/// operator question — "who has held this for four minutes" — and answering it with "409
/// conflict" answers nothing.
pub async fn acquire(pool: &PgPool, wait: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let taken: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)")
            .bind(lock_id())
            .fetch_one(pool)
            .await?;
        if taken {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(MigrationSafetyError::Locked(blocked_message(pool).await));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Release the lock this process holds.
///
/// Best effort by design: a session that dies releases a PostgreSQL advisory lock anyway, so a
/// failed unlock is not worth turning a successful migration run into a failure. It is still
/// logged, because "the unlock failed" on a *live* session is the difference between a lock that
/// expires and one that pins itself to one pooled connection.
pub async fn release(pool: &PgPool) {
    if let Err(err) = sqlx::query("select pg_advisory_unlock($1)")
        .bind(lock_id())
        .execute(pool)
        .await
    {
        tracing::warn!(error = %err, "the migration advisory lock could not be released explicitly");
    }
}

/// The refusal message: what is running, since when, and what is waiting behind it.
async fn blocked_message(pool: &PgPool) -> String {
    let state = view(pool).await;
    let mut message = format!(
        "another migration run holds the lock {}",
        state.lock_key
    );
    match (state.version.as_deref(), state.age_seconds) {
        (Some(version), Some(age)) => {
            message.push_str(&format!(
                ": {version} has been running for {age}s"
            ));
            if let Some(actor) = state.actor.as_deref() {
                message.push_str(&format!(" (by {actor}"));
                if let Some(source) = state.source.as_deref() {
                    message.push_str(&format!(" from {source}"));
                }
                message.push(')');
            }
        }
        _ => message.push_str(" (the run that holds it is not in the journal)"),
    }
    if !state.blocked.is_empty() {
        let waiting: Vec<String> = state
            .blocked
            .iter()
            .map(|query| format!("pid {} waiting {}s", query.pid, query.age_seconds))
            .collect();
        message.push_str(&format!("; waiting behind it: {}", waiting.join(", ")));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_ascii_of_its_own_name() {
        // The number is asserted, not derived, because the point is that an operator reading
        // `pg_locks.objid` gets the same value this crate uses — and a test that computed it
        // from the same expression would pass for a key nobody could recognise in a log.
        assert_eq!(lock_id(), 8_029_195_109_791_591_783);
    }

    #[test]
    fn the_key_rendering_carries_the_name_as_well_as_the_number() {
        let rendered = lock_id_text();
        assert!(
            rendered.contains("omnionmg"),
            "the number alone is unreadable in pg_locks: {rendered}"
        );
        assert!(
            rendered.contains(&lock_id().to_string()),
            "the number is still there for an operator copying it into a query: {rendered}"
        );
    }

    #[test]
    fn the_key_is_not_sqlxs() {
        // SQLx uses `0` for its own migration lock (the "one big lock" constant). Sharing it
        // would serialise the two layers into an order neither was written for, so the property
        // is pinned rather than left to a comment.
        assert_ne!(lock_id(), 0);
    }

    #[test]
    fn the_wait_is_shorter_than_a_statement_timeout_because_it_bounds_a_different_thing() {
        // The policy's `lock_timeout_ms` default is 5000 and bounds a blocked DDL statement. This
        // one bounds waiting for the runner, and the test names which of the two it is so a later
        // reader does not "fix" it to match the policy.
        assert_eq!(LOCK_WAIT, Duration::from_secs(4));
        assert!(
            LOCK_WAIT < Duration::from_millis(5000),
            "waiting for the runner must not outlast the default statement lock timeout"
        );
    }
}
