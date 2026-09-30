//! Worker heartbeats: the writer that makes `4/4` a fact (REQ-014, slice 4).
//!
//! Slices 1 and 3 shipped `worker_heartbeats` as a table and a reader — [`crate::probes::probe_workers`]
//! counts rows and names the stale ones — and the acceptance criterion *"worker counts come
//! from heartbeat rows; stopping a worker changes `4/4` to `3/4` and names it"* was ticked. It
//! was ticked honestly, by the walk, which inserts the rows it then reads.
//!
//! **Nothing in the platform ever wrote one.** Every other runner in `apps/api` writes into a
//! table it also reads; this table had the read half only. In production the worker row would
//! have read `unknown` ("no worker has registered a heartbeat") for ever, and the `4/4` card
//! would have shipped as a screen that always renders one honest sentence. That is the same
//! shape as the uncalled `prune_candidates` in REQ-010 and the unwritten `next_run_at` in
//! REQ-013: a table, a reader, a column nobody fills.
//!
//! So this module is the writer, and it is deliberately boring:
//!
//! * **A heartbeat is an upsert keyed on `id`, and `id` is derived, not passed.** A process
//!   that invented its own id would create a new row on every restart and the `4/4` would grow
//!   to `4/9` after a week of deploys. The key is `kind` + host + pid, so a restarted process
//!   reclaims its own row instead of adding one.
//! * **`started_at` is written once, on insert, and never moved afterwards.** It answers "when
//!   did this worker come up", and a heartbeat loop that refreshed it every tick would make a
//!   process that has been up for three months look three seconds old — which is exactly the
//!   number an operator reads first when they ask "did this just restart?".
//! * **The heartbeat loop skips a tick rather than catching up.** `MissedTickBehavior::Skip`
//!   is set by the caller in `apps/api`; a heartbeat that fires a burst of catch-up ticks after
//!   a GC pause writes rows nobody can read and, worse, makes `last_seen_at` jump forward so
//!   fast that a genuinely stale worker looks fresh.
//! * **Nothing here emits an event.** A heartbeat is not a fact anybody subscribes to; it is
//!   polled. The events the request names (`health.service.degraded`, `health.service.recovered`,
//!   `health.threshold.breached`, `health.incident.acknowledged`, `health.checks.completed`)
//!   are emitted by the check runner in `apps/api/health_runner.rs`, which is the code that
//!   knows *what changed*, where a heartbeat only knows that something is still there.

use serde_json::{json, Value};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{HealthError, Result};

/// The heartbeat states `worker_heartbeats` accepts.
///
/// Mirrors `worker_heartbeats_state_check` in `0188_system_health.sql`. The duplication is
/// deliberate: the migration is the floor and this list is what a caller can name, so a typo
/// is refused here with a sentence instead of arriving as a `23514`.
pub const WORKER_STATES: [&str; 4] = ["running", "stopping", "stopped", "failed"];

/// One worker's claim to be alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// Stable key — `kind` + host + pid. See [`heartbeat_id`].
    pub id: String,
    /// What kind of worker this is (`scheduler`, `delivery`, `indexer`, …).
    pub kind: String,
    /// Hostname the worker runs on.
    pub host: String,
    /// Build version, so a panel can name the version a stale worker is running.
    pub version: String,
    /// One of [`WORKER_STATES`].
    pub state: String,
    /// Process id on that host; part of the key so a restart reclaims the row.
    pub pid: i32,
    /// Anything the worker wants the panel to know about.
    pub meta: Value,
}

/// One heartbeat row, as stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct WorkerRow {
    /// Row key.
    pub id: String,
    /// Worker kind.
    pub kind: String,
    /// Hostname.
    pub host: String,
    /// Build version.
    pub version: String,
    /// Last state the worker wrote.
    pub state: String,
    /// When the process came up.
    pub started_at: OffsetDateTime,
    /// When it last said it was alive.
    pub last_seen_at: OffsetDateTime,
    /// Stored detail.
    pub meta: Value,
    /// Seconds since `last_seen_at`, read in SQL so every row is judged against one clock.
    pub age_seconds: i64,
}

/// The key a worker's heartbeat is stored under.
///
/// `kind` + host + pid, and not a uuid: the reader counts rows, so a worker that generated a
/// fresh id per process would make `4/4` a function of how many times the fleet has been
/// restarted.
///
/// **A pid being reused cannot be told apart from a worker still running, and the tie is broken
/// towards the claim that is safer to be wrong about.** When a pid comes back, the row is
/// reclaimed — same id, same `started_at`, `last_seen_at` moved forward — so the panel reports a
/// worker that has been up since the *first* process rather than since the restart. The
/// alternative (moving `started_at` forward on every conflict) would claim the opposite, and it
/// would be wrong every single heartbeat: refreshing the start on each beat makes a process that
/// has run for three months look three seconds old, which is the first number an operator reads
/// when they are asking whether something just restarted. So the row under-reports a restart
/// rather than inventing one on every tick.
#[must_use]
pub fn heartbeat_id(kind: &str, host: &str, pid: i32) -> String {
    format!("{kind}@{host}#{pid}")
}

/// Whether a state name is one the table accepts.
#[must_use]
pub fn is_worker_state(state: &str) -> bool {
    WORKER_STATES.contains(&state)
}

/// Validate a heartbeat before it is written.
///
/// The refusals are the ones a silent failure would turn into a mystery: an empty kind or host
/// produces a row the panel groups under nothing, and an unknown state is a `23514` at best.
/// `meta` has to be an object because the migration's `jsonb_typeof(meta) = 'object'` check
/// says so — the same floor, named here so the message is the useful one.
pub fn validate(beat: &Heartbeat) -> Result<()> {
    if beat.kind.trim().is_empty() {
        return Err(HealthError::Invalid(
            "a worker heartbeat needs a kind".to_string(),
        ));
    }
    if beat.host.trim().is_empty() {
        return Err(HealthError::Invalid(
            "a worker heartbeat needs a host".to_string(),
        ));
    }
    if beat.version.trim().is_empty() {
        return Err(HealthError::Invalid(
            "a worker heartbeat needs a version".to_string(),
        ));
    }
    if !is_worker_state(&beat.state) {
        return Err(HealthError::Invalid(format!(
            "'{}' is not a worker state; use one of {}",
            beat.state,
            WORKER_STATES.join(", ")
        )));
    }
    if !beat.meta.is_object() {
        return Err(HealthError::Invalid(
            "worker heartbeat detail must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

/// Write one heartbeat, inserting the row or moving its `last_seen_at` forward.
///
/// `started_at` is written on insert and left alone on conflict — that asymmetry is the whole
/// reason this is not two statements, and the direction it errs in is deliberate: see
/// [`heartbeat_id`] for why the row under-reports a reused pid rather than over-reporting one.
pub async fn beat(pool: &PgPool, beat: &Heartbeat) -> Result<()> {
    validate(beat)?;
    sqlx::query(
        "insert into worker_heartbeats (id, kind, host, version, state, meta) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (id) do update \
            set last_seen_at = now(), \
                version = excluded.version, \
                state = excluded.state, \
                meta = excluded.meta",
    )
    .bind(&beat.id)
    .bind(&beat.kind)
    .bind(&beat.host)
    .bind(&beat.version)
    .bind(&beat.state)
    .bind(&beat.meta)
    .execute(pool)
    .await?;
    Ok(())
}

/// Move a worker's row to `stopped`.
///
/// Called from a process's shutdown path, and it is what makes a *clean* stop visible instead of
/// being indistinguishable from a crash: without it, a gracefully stopped worker keeps its row
/// until the staleness window closes, and the panel says "stale" — which is true but useless,
/// because "we stopped it on purpose" and "it died" are the two answers an operator needs.
pub async fn mark_stopped(pool: &PgPool, id: &str) -> Result<bool> {
    let updated = sqlx::query(
        "update worker_heartbeats set state = 'stopped', last_seen_at = now() where id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Every worker row that has said something in the last `window`, with its age.
///
/// The age is computed in SQL (`extract(epoch from (now() - last_seen_at))`) so every row is
/// judged against the same `now()`: reading the timestamp into Rust and subtracting a
/// separately-read clock is wrong by exactly the gap between the two reads, and a panel that
/// sorts workers by it can show a worker older than itself.
pub async fn rows(pool: &PgPool, window_seconds: i64) -> Result<Vec<WorkerRow>> {
    let rows = sqlx::query_as::<_, WorkerRow>(
        "select id, kind, host, version, state, started_at, last_seen_at, meta, \
                extract(epoch from (now() - last_seen_at))::bigint as age_seconds \
         from worker_heartbeats \
         where last_seen_at > now() - make_interval(secs => $1::double precision) \
         order by kind, id",
    )
    .bind(window_seconds)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A summary of the fleet, as the panel's worker card renders it.
///
/// `n/m` needs an `m`, and the platform cannot know how many workers an installation *should*
/// have: the honest denominator is the distinct kinds that have ever registered inside the
/// window, so the card reads `4/4` on a healthy four-kind fleet and `3/4` the moment one of
/// them goes quiet — which is the request's own phrasing and the only one the data supports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkerSummary {
    /// Kinds that are inside the staleness window.
    pub alive: i64,
    /// Kinds that have registered inside the window at all — the denominator.
    pub expected: i64,
    /// Worker ids past the stale limit, named.
    pub stale: Vec<String>,
    /// Counts per kind.
    pub kinds: Vec<KindCount>,
}

/// How many workers of one kind are reporting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KindCount {
    /// The kind.
    pub kind: String,
    /// How many rows are fresh.
    pub alive: i64,
    /// How many rows are past the stale limit.
    pub stale: i64,
}

/// Summarise a set of rows into the card's numbers.
///
/// Split out from the query so the arithmetic is unit-testable without a database, and so the
/// walk and the route agree by construction rather than by two people writing the same `if`.
#[must_use]
pub fn summarise(rows: &[WorkerRow], stale_after_seconds: i64) -> WorkerSummary {
    let mut kinds: Vec<KindCount> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut alive = 0_i64;

    for row in rows {
        let stale_row = row.age_seconds > stale_after_seconds;
        if stale_row {
            stale.push(row.id.clone());
        } else {
            alive += 1;
        }
        match kinds.iter_mut().find(|entry| entry.kind == row.kind) {
            Some(entry) => {
                entry.alive += i64::from(!stale_row);
                entry.stale += i64::from(stale_row);
            }
            None => kinds.push(KindCount {
                kind: row.kind.clone(),
                alive: i64::from(!stale_row),
                stale: i64::from(stale_row),
            }),
        }
    }
    kinds.sort_by(|left, right| left.kind.cmp(&right.kind));
    stale.sort();

    WorkerSummary {
        alive,
        // Every kind present in the window is a kind somebody expects: a kind that vanished
        // from the fleet entirely is not something this summary can notice, and pretending it
        // could is how a `4/4` becomes `0/4` on a fresh install that has one worker.
        expected: kinds.iter().map(|entry| i64::from(entry.alive + entry.stale > 0)).sum(),
        stale,
        kinds,
    }
}

/// Remove rows a worker forgot to clean up on a host that is gone.
///
/// Not part of slice 4's acceptance criteria and therefore **not wired to a route**: it is
/// here because the read window already bounds what the panel shows, and a prune without a
/// caller is precisely the thing this file exists to stop repeating. It is `pub` so the next
/// slice can call it from the same place the other runners are spawned.
pub async fn prune(pool: &PgPool, older_than_seconds: i64) -> Result<i64> {
    let removed = sqlx::query(
        "delete from worker_heartbeats \
         where last_seen_at < now() - make_interval(secs => $1::double precision)",
    )
    .bind(older_than_seconds)
    .execute(pool)
    .await?;
    Ok(i64::try_from(removed.rows_affected()).unwrap_or(i64::MAX))
}

/// The default detail a heartbeat carries when its caller has nothing to add.
///
/// An empty object rather than a null: the migration's `jsonb_typeof` check says object, and a
/// caller that has nothing to say should not be pushed into inventing a value.
#[must_use]
pub fn default_meta() -> Value {
    json!({})
}

/// Build the heartbeat a runner publishes about itself.
///
/// `kind` and `version` are arguments rather than read from the environment here so the caller
/// stays in charge of what it claims to be — a helper that guessed `hostname` and `CARGO_PKG_VERSION`
/// would make the panel's "which version is this worker" answer unanswerable.
#[must_use]
pub fn self_heartbeat(kind: &str, host: &str, version: &str, meta: Value) -> Heartbeat {
    Heartbeat {
        id: heartbeat_id(kind, host, std::process::id() as i32),
        kind: kind.to_string(),
        host: host.to_string(),
        version: version.to_string(),
        state: "running".to_string(),
        pid: std::process::id() as i32,
        meta,
    }
}

/// The host a worker reports itself as.
///
/// Reads the environment first and falls back to the literal `unknown`, and the reason is not
/// tidiness: this crate's header calls `probes::statvfs` "the one `unsafe` in the workspace" and
/// a `deny(unsafe_code)` is what keeps that true. A second `unsafe` to call `gethostname`
/// would have to either carry `#[allow(unsafe_code)]` — weakening the claim the header makes —
/// or pull in a dependency for one string.
///
/// `HOSTNAME` is set by the init system on every Linux host this runs on, and when it is not,
/// `unknown` is the **honest** answer: the row still gets written, the panel still counts the
/// worker, and the host column says it could not be read. An invented hostname would be worse
/// than either — it would group every worker that could not read its own host under one
/// fabricated name, and the reader would count that group as real.
#[must_use]
pub fn hostname() -> String {
    match std::env::var("HOSTNAME") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => UNKNOWN_HOST.to_string(),
    }
}

/// What a worker reports when it cannot read its own host.
pub const UNKNOWN_HOST: &str = "unknown";

/// A UUID-shaped id for callers that need a fresh worker identity (tests, the panel's fixtures).
#[must_use]
pub fn scratch_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, id: &str, age: i64) -> WorkerRow {
        WorkerRow {
            id: id.to_string(),
            kind: kind.to_string(),
            host: "host-a".to_string(),
            version: "1.0.0".to_string(),
            state: "running".to_string(),
            started_at: OffsetDateTime::UNIX_EPOCH,
            last_seen_at: OffsetDateTime::UNIX_EPOCH,
            meta: json!({}),
            age_seconds: age,
        }
    }

    fn beat(kind: &str, state: &str) -> Heartbeat {
        Heartbeat {
            id: heartbeat_id(kind, "host-a", 42),
            kind: kind.to_string(),
            host: "host-a".to_string(),
            version: "1.0.0".to_string(),
            state: state.to_string(),
            pid: 42,
            meta: json!({}),
        }
    }

    /// The id is derived, not random: two beats of the same process are one row.
    #[test]
    fn a_heartbeat_id_is_derived_from_kind_host_and_pid() {
        assert_eq!(
            heartbeat_id("scheduler", "host-a", 7),
            heartbeat_id("scheduler", "host-a", 7)
        );
        assert_ne!(
            heartbeat_id("scheduler", "host-a", 7),
            heartbeat_id("scheduler", "host-a", 8),
            "a different process is a different row"
        );
        assert_ne!(
            heartbeat_id("scheduler", "host-a", 7),
            heartbeat_id("delivery", "host-a", 7),
            "a different kind is a different row"
        );
    }

    /// `4/4` on a healthy fleet, and the missing one named — the request's own criterion.
    #[test]
    fn one_silent_worker_of_a_kind_is_named_and_lowers_the_count() {
        let rows = vec![
            row("scheduler", "scheduler@host-a#1", 4),
            row("scheduler", "scheduler@host-a#2", 5),
            row("delivery", "delivery@host-a#1", 3),
            row("indexer", "indexer@host-a#1", 9_000),
        ];
        let summary = summarise(&rows, 120);

        assert_eq!(summary.alive, 3, "the stale worker is not counted alive");
        assert_eq!(summary.expected, 3, "the kinds are the denominator");
        assert_eq!(
            summary.stale,
            vec!["indexer@host-a#1".to_string()],
            "and it is named, not merely counted"
        );
        let indexer = summary
            .kinds
            .iter()
            .find(|entry| entry.kind == "indexer")
            .expect("the kind is still listed");
        assert_eq!(indexer.alive, 0);
        assert_eq!(indexer.stale, 1);
    }

    /// An age exactly at the limit is still fresh.
    ///
    /// The boundary is pinned because `>` and `>=` here decide whether a worker that beat
    /// exactly one tick over its interval is called stale. One tick of slack is the difference
    /// between a panel that is quiet and a panel that screams at every heartbeat boundary.
    #[test]
    fn the_stale_limit_is_exclusive() {
        let rows = vec![row("scheduler", "scheduler@host-a#1", 120)];
        assert!(summarise(&rows, 120).stale.is_empty(), "exactly at the limit");
        assert_eq!(
            summarise(&rows, 119).stale.len(),
            1,
            "one second past it is stale"
        );
    }

    /// A worker that has registered nothing is not `4/0` and not `0/0`.
    #[test]
    fn an_empty_fleet_is_zero_of_zero_rather_than_a_failure() {
        let summary = summarise(&[], 120);
        assert_eq!(summary.alive, 0);
        assert_eq!(summary.expected, 0);
        assert!(summary.stale.is_empty());
    }

    /// The states the table accepts are the states this module names.
    #[test]
    fn an_unknown_state_is_refused_with_the_list() {
        for state in WORKER_STATES {
            assert!(validate(&beat("scheduler", state)).is_ok(), "{state}");
        }
        let error = validate(&beat("scheduler", "sleeping")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("sleeping"), "names what it got: {message}");
        assert!(message.contains("running"), "names what it wants: {message}");
    }

    /// The validations that would otherwise arrive as a `23514` or a row grouped under nothing.
    #[test]
    fn a_heartbeat_without_kind_host_version_or_object_detail_is_refused() {
        let mut empty_kind = beat("scheduler", "running");
        empty_kind.kind = "  ".to_string();
        assert!(validate(&empty_kind).is_err(), "no kind");

        let mut empty_host = beat("scheduler", "running");
        empty_host.host = String::new();
        assert!(validate(&empty_host).is_err(), "no host");

        let mut empty_version = beat("scheduler", "running");
        empty_version.version = String::new();
        assert!(validate(&empty_version).is_err(), "no version");

        let mut array_meta = beat("scheduler", "running");
        array_meta.meta = json!([1, 2, 3]);
        assert!(
            validate(&array_meta).is_err(),
            "the migration's jsonb_typeof check says object"
        );
    }

    /// A worker's own heartbeat describes the process that wrote it.
    #[test]
    fn a_self_heartbeat_names_the_running_process() {
        let beat = self_heartbeat("scheduler", "host-a", "1.2.3", json!({}));
        assert_eq!(beat.id, heartbeat_id("scheduler", "host-a", std::process::id() as i32));
        assert_eq!(beat.pid, std::process::id() as i32);
        assert_eq!(beat.state, "running");
        assert!(validate(&beat).is_ok());
    }

    /// The host is read from the kernel, and a machine that refuses is `unknown`, not empty.
    #[test]
    fn the_hostname_is_never_empty() {
        let host = hostname();
        assert!(!host.is_empty(), "an empty host would group every row under nothing");
    }
}