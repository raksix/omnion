//! The probe registry: one probe per dependency, and the rules that keep it honest.
//!
//! A probe is a named async function that answers **one** question about **one**
//! dependency. The registry is a table of them, and every rule in this module
//! exists because of a specific way a health screen lies:
//!
//! * **A probe that cannot reach is `down` with a message, not an absent row and
//!   not an error.** Stopping Redis has to turn a row red. A probe returning
//!   `Err` and being skipped would render a *missing* Redis, and a missing row on
//!   a status screen is the most dangerous way to be wrong. So [`Observation`]
//!   has no "unreachable" error path — unreachability is a *state*, and only a
//!   bug in a probe is a panic.
//! * **A probe never has the last word on thresholds.** It reports what it saw;
//!   the state is derived in [`finish`] from the latency budget in one place, so
//!   `degraded` means the same thing on every row.
//! * **A probe is cheap or it is not a probe.** Every deadline is applied by the
//!   probe itself from [`PROBE_TIMEOUT_MS`] and nowhere else, so a new probe
//!   cannot forget its own timeout — the first risk the request names.
//!
//! Nothing here opens a connection of its own. Every probe reads through the
//! handles in [`ProbeContext`], which are the same handles the request path
//! uses, so a status screen can never be the thing that exhausts the pool.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{Value, json};

use crate::model::CheckDescription;
use crate::vocabulary::{HOST_SERVICE, is_state};

/// How long a single probe may take before it gives up.
///
/// Three seconds, and short on purpose: the panel's default auto-refresh is 15 s,
/// so a probe that hits the ceiling is a fifth of the interval spent on one row.
/// A probe that needs longer is measuring something that belongs in a metrics
/// pipeline, not on a status screen.
pub const PROBE_TIMEOUT_MS: u64 = 3_000;

/// The latency above which an answer is *degraded* rather than healthy.
///
/// A probe that answers in 2.9 s is technically fine and operationally not, and
/// that difference is what a status screen is for. It is one constant because the
/// budget is the same everywhere in slice 1; the threshold policy in slice 3 is
/// what makes it configurable.
pub const DEGRADED_LATENCY_MS: i64 = 1_000;

/// The depth at which a merely busy queue starts counting as degraded.
pub const DEGRADED_QUEUE_DEPTH: i64 = 1_000;

/// What a probe learned.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// The dependency answered and is fine.
    Healthy(String),
    /// The dependency answered, but something about it is wrong.
    Degraded(String),
    /// The dependency did not answer. A *state*, not an error.
    Down(String),
    /// The probe could not run — no table, no file, an interface this kernel does
    /// not have. The platform's gap, not the dependency's failure.
    Unknown(String),
}

impl Observation {
    /// The state word this observation maps to.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Healthy(_) => "healthy",
            Self::Degraded(_) => "degraded",
            Self::Down(_) => "down",
            Self::Unknown(_) => "unknown",
        }
    }

    /// The sentence an operator reads.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Healthy(message)
            | Self::Degraded(message)
            | Self::Down(message)
            | Self::Unknown(message) => message,
        }
    }
}

/// One probe's answer, plus everything the panel needs to render the row.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    /// The service key this was for.
    pub service: String,
    /// What the probe found.
    pub outcome: Observation,
    /// How long the probe took, in milliseconds.
    pub latency_ms: i64,
    /// Named fields behind the sentence. The panel renders these by key.
    pub detail: Value,
    /// The individual checks, for the service detail screen's table.
    pub checks: Vec<CheckDescription>,
    /// The metric samples this probe produced.
    pub metrics: Vec<MetricReading>,
}

impl ProbeResult {
    /// A `down` result carrying the error's own message.
    ///
    /// The one constructor that says "the dependency did not answer", so the
    /// path that produces a red row cannot be forgotten by a probe that fails a
    /// different way.
    ///
    /// **It still reports a `latency_ms` metric, and that is the point.** A probe that produced
    /// no metrics produced no *samples* either — `report_of` derives `health_samples` rows from
    /// `metrics` — so an outage was recorded as the *absence* of a reading. Every green metric
    /// either side of it stayed at its last healthy value, and the 24 h trend drew one straight
    /// line straight through the outage. The panel's present tense was honest (the live read
    /// probes) and its history was a lie, which is the worse of the two: the chart is what an
    /// operator opens *after* the incident to see how long it lasted.
    ///
    /// The value is the time until the give-up (`PROBE_TIMEOUT_MS` or less, never a measured
    /// round trip), and the unit says so in the row the chart draws: a missing reading and a
    /// reading that timed out are different facts, and this is the one the platform can prove.
    #[must_use]
    pub fn down(service: &str, latency_ms: i64, reason: impl Into<String>) -> Self {
        Self {
            service: service.to_string(),
            outcome: Observation::Down(reason.into()),
            latency_ms,
            detail: json!({}),
            checks: Vec::new(),
            metrics: vec![MetricReading {
                metric: "latency_ms".to_string(),
                value: f64::from(i32::try_from(latency_ms).unwrap_or(i32::MAX)),
                unit: "ms (timeout)".to_string(),
            }],
        }
    }

    /// A `down` result plus the reason, attached both to the row and to its one
    /// check, so the detail screen's table is never empty for a red row.
    #[must_use]
    pub fn down_with_check(
        service: &str,
        check: &str,
        latency_ms: i64,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self::down(service, latency_ms, reason.clone())
            .with_detail(json!({ "error": reason }))
            .with_check(check, "down", reason)
    }

    /// Attach the structured detail. A chain rather than a field, so a probe's
    /// happy path reads as one expression.
    #[must_use]
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    /// Attach a named check, which becomes a row on the detail screen.
    #[must_use]
    pub fn with_check(mut self, check: &str, state: &str, message: impl Into<String>) -> Self {
        self.checks.push(CheckDescription {
            check: check.to_string(),
            state: state.to_string(),
            message: message.into(),
            latency_ms: self.latency_ms,
        });
        self
    }

    /// Attach a metric reading, which becomes a sample and a metric card.
    #[must_use]
    pub fn with_metric(mut self, metric: &str, value: f64, unit: &str) -> Self {
        self.metrics.push(MetricReading {
            metric: metric.to_string(),
            value,
            unit: unit.to_string(),
        });
        self
    }
}

/// One metric a probe measured.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricReading {
    /// The metric key.
    pub metric: String,
    /// The measured value.
    pub value: f64,
    /// The unit it is in.
    pub unit: String,
}

/// The sentence a slow-but-answering probe gets, and the millisecond stopwatch a
/// probe reports through.
///
/// The distinction [`finish`] draws is the whole reason `degraded` is a state of
/// its own rather than a shade of red: a four-second round trip to the database
/// is not the same event as a refused connection, and somebody woken at 03:00
/// needs to be told which one happened.
///
/// **A non-healthy outcome still records its latency.** `report_of` turns `metrics` into
/// `health_samples` rows, so a probe that ended `down` or `unknown` without a metric wrote *no
/// sample*, and the stored history showed a continuous healthy line through the outage. The live
/// read was right and the 24 h chart was not, and the chart is the one somebody opens afterwards
/// to see how long it lasted. One reading, marked with the state that produced it, is the honest
/// minimum: the platform cannot say what the metric *was* while nothing answered, and the unit
/// says which of the two this is.
fn finish(service: &str, outcome: Observation, elapsed: Instant) -> ProbeResult {
    let latency_ms = i64::try_from(elapsed.elapsed().as_millis()).unwrap_or(i64::MAX);
    let state = outcome.state();
    let mut result = ProbeResult {
        service: service.to_string(),
        outcome,
        latency_ms,
        detail: json!({}),
        checks: Vec::new(),
        // Only the healthy path gets this from the caller; a `down`/`unknown` result is
        // recorded here so the history carries the outage instead of a gap that reads as
        // "nothing happened". `PROBE_TIMEOUT_MS` is the ceiling, so the value can never
        // be mistaken for a measured round trip.
        metrics: if state == "healthy" {
            Vec::new()
        } else {
            vec![MetricReading {
                metric: "latency_ms".to_string(),
                value: f64::from(i32::try_from(latency_ms).unwrap_or(i32::MAX)),
                unit: "ms (timeout)".to_string(),
            }]
        },
    };
    if state == "healthy" && latency_ms > DEGRADED_LATENCY_MS {
        result.outcome = Observation::Degraded(format!(
            "answered in {latency_ms} ms, over the {DEGRADED_LATENCY_MS} ms budget"
        ));
    }
    result
}

/// What a probe is handed: the handles it may read through, and nothing else.
pub struct ProbeContext<'a> {
    /// The PostgreSQL pool the request path is already using.
    pub pool: &'a sqlx::PgPool,
    /// The platform's Redis handle.
    pub redis: &'a omnion_core::RedisClient,
    /// The object store the media library writes through.
    pub storage: &'a omnion_storage::Storage,
    /// Which storage driver is active, so the S3 row can name what it checked.
    pub storage_driver: String,
    /// The running build, for the API row's own version and service name.
    pub build: omnion_core::BuildInfo,
    /// The environment name, for the API row.
    pub environment: String,
    /// After how many seconds a worker with no heartbeat counts as stale.
    pub worker_stale_seconds: i64,
}

/// The row the PostgreSQL probe's fixed statement answers with.
///
/// A named `FromRow` rather than a tuple plus `row.get(...)`: the tuple version
/// binds a value to a *position*, so reordering the SELECT — or, worse, adding a
/// column to the statement and forgetting the reader — compiles and then reads
/// `connections` out of the `max_connections` column. The names are checked by
/// the derive, which is the only place in this crate where a column rename is an
/// error rather than a wrong number on a status screen.
#[derive(Debug, sqlx::FromRow)]
struct PostgresRow {
    /// The `select 1` answer, rendered as text so the row can show the round trip
    /// actually happened rather than only that it did not error.
    one: String,
    /// Connections currently open on the server, across every client.
    connections: i64,
    /// The server's own `max_connections`.
    max_connections: i64,
}

/// The row the queue probe's statement answers with.
#[derive(Debug, sqlx::FromRow)]
struct QueueRow {
    /// Everything waiting across the real queues.
    depth: i64,
    /// How long the oldest pending delivery has waited. `None` when nothing is
    /// waiting — the distinction matters, because "0 seconds old" and "nothing"
    /// are different answers to "is the queue stuck".
    oldest_seconds: Option<i64>,
    /// Deliveries that have exhausted their attempts.
    failed: i64,
}

/// The row the workers probe groups into.
#[derive(Debug, sqlx::FromRow)]
struct WorkerKindRow {
    /// Which kind of worker this row counts.
    kind: String,
    /// How many of them have registered.
    total: i64,
    /// Seconds since the *oldest* one in this kind was last seen. `None` when the
    /// aggregate cannot say, which the probe turns into "stale" rather than
    /// "fresh": an age it cannot read is not a fresh heartbeat.
    oldest_age: Option<i64>,
}

/// The API itself: the process answering, right now.
///
/// This is the control row. It cannot fail in the interesting way, which is
/// exactly why it is still probed rather than hard-coded green: a panel whose
/// `api` row is a constant teaches operators to stop reading that column, and
/// the column is the one they need most.
#[must_use]
pub fn probe_api(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let build = ctx.build;
    finish(
        "api",
        Observation::Healthy(format!(
            "{} {} is serving requests",
            build.service, build.version
        )),
        started,
    )
    .with_detail(json!({
        "service": build.service,
        "version": build.version,
        "environment": ctx.environment,
    }))
    .with_check("serving", "healthy", "the request handler is on this thread")
}

/// PostgreSQL: a round trip on a fixed statement, plus the pool's own counters.
///
/// The statement is `select 1` and two `pg_catalog` reads rather than a table
/// read, deliberately: the point is that the *server* answers. A query that
/// touched a platform table would also fail on a locked or missing table and be
/// reported as "PostgreSQL is down", which is a much worse lie — it points an
/// operator at the database when the fault is one migration behind.
pub async fn probe_postgres(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let round_trip: Result<PostgresRow, sqlx::Error> = tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        sqlx::query_as::<_, PostgresRow>(
            "select 1::text as one, \
                    (select count(*)::bigint from pg_stat_activity) as connections, \
                    (select setting::bigint from pg_settings where name = 'max_connections') \
                        as max_connections",
        )
        .fetch_one(ctx.pool),
    )
    .await
    .unwrap_or(Err(sqlx::Error::PoolTimedOut));

    match round_trip {
        Ok(row) => {
            let PostgresRow {
                one,
                connections,
                max_connections,
            } = row;
            let idle_in_pool = i64::try_from(ctx.pool.num_idle()).unwrap_or(i64::MAX);
            // A pool the platform has not yet borrowed from is worth saying out
            // loud: a status screen that reads `0 idle` on a process serving
            // traffic is a screen whose numbers are not the real ones.
            let pool_state = if idle_in_pool == 0 { "unknown" } else { "healthy" };
            let outcome = if max_connections > 0 && connections >= max_connections {
                Observation::Degraded(format!(
                    "{connections} of {max_connections} connections are in use"
                ))
            } else {
                Observation::Healthy(format!(
                    "answered as {one} with {connections} of {max_connections} connections in use"
                ))
            };
            finish("postgres", outcome, started)
                .with_detail(json!({
                    "round_trip_value": one,
                    "connections": connections,
                    "max_connections": max_connections,
                    "idle_in_pool": idle_in_pool,
                }))
                .with_check("round_trip", "healthy", format!("select 1 answered as {one}"))
                .with_check(
                    "pool",
                    pool_state,
                    if idle_in_pool == 0 {
                        "this process holds no pooled connection; the numbers above are the server's"
                            .to_string()
                    } else {
                        format!("{idle_in_pool} idle connections held")
                    },
                )
                .with_metric("db_connections", connections as f64, "connections")
        }
        Err(error) => {
            let message = describe_sqlx(&error);
            finish("postgres", Observation::Down(message.clone()), started)
                .with_detail(json!({ "error": message }))
                .with_check("round_trip", "down", message)
        }
    }
}

/// Redis: `PING`, plus used memory and connected clients when they can be read.
///
/// The extra two come from a second, *optional* command. `INFO` is not available
/// on every Redis build, and a Redis that answered `PING` correctly but declined
/// to volunteer its memory is a healthy Redis — failing the row over two
/// statistics would be the platform reporting a dependency as down for giving
/// less information.
pub async fn probe_redis(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    match ctx.redis.ping().await {
        Ok(()) => {
            let mut result = finish(
                "redis",
                Observation::Healthy("PING answered".to_string()),
                started,
            )
            .with_check("ping", "healthy", "PING answered");
            if let Some(stats) = redis_stats(ctx).await {
                result = result
                    .with_detail(json!({
                        "used_memory_bytes": stats.used_memory,
                        "connected_clients": stats.connected_clients,
                    }))
                    .with_metric("used_memory_bytes", stats.used_memory, "bytes");
                result = result.with_check(
                    "memory",
                    "healthy",
                    format!(
                        "{} bytes in use by {} clients",
                        stats.used_memory, stats.connected_clients
                    ),
                );
            } else {
                // Said out loud rather than left as a missing field: the panel
                // renders a probe's checks, and a check that silently vanished
                // is a check the operator cannot tell apart from one that passed.
                result = result.with_check(
                    "memory",
                    "unknown",
                    "Redis answered but did not report its memory statistics",
                );
            }
            result
        }
        Err(error) => {
            let message = error.to_string();
            finish("redis", Observation::Down(message.clone()), started)
                .with_detail(json!({ "error": message }))
                .with_check("ping", "down", message)
        }
    }
}

/// The optional `INFO` read. A Redis that refuses it costs the row two fields,
/// not its verdict.
async fn redis_stats(ctx: &ProbeContext<'_>) -> Option<RedisStats> {
    let mut connection = ctx.redis.connection().await.ok()?;
    // `INFO memory` and `INFO clients` are two sections of one document, and
    // asking for both in one round trip is the difference between one timeout
    // budget and two.
    let response: redis::Value = redis::cmd("INFO")
        .arg("memory")
        .arg("clients")
        .query_async(&mut connection)
        .await
        .ok()?;
    let text: String = redis::from_redis_value(response).ok()?;
    let field = |name: &str| -> Option<f64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.trim().parse::<f64>().ok())
    };
    Some(RedisStats {
        used_memory: field("used_memory:")?,
        connected_clients: field("connected_clients:")?,
    })
}

/// The two statistics the Redis row shows when it can get them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RedisStats {
    /// `used_memory` from `INFO memory`.
    used_memory: f64,
    /// `connected_clients` from `INFO clients`.
    connected_clients: f64,
}

/// The object store: the platform's own `probe`, through whichever driver the
/// deployment is configured with.
///
/// Deliberately not "HEAD a fixed object". A probe key nobody created would
/// report a healthy store as unhealthy (the key is absent) and, worse, would
/// report a store that is merely *writable* as one that can *read*. The storage
/// crate already answers "are you really there" for both drivers — and it
/// already distinguishes a missing bucket (first run) from an unreachable store,
/// which is exactly the distinction this row has to keep. So the row delegates
/// instead of re-implementing a third time.
pub async fn probe_storage(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let driver = ctx.storage_driver.clone();
    match tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        ctx.storage.probe(),
    )
    .await
    {
        Ok(Ok(())) => finish(
            "s3",
            Observation::Healthy(format!("the {driver} store answered")),
            started,
        )
        .with_detail(json!({ "driver": driver }))
        .with_check("round_trip", "healthy", format!("the {driver} store answered")),
        Ok(Err(error)) => {
            let message = error.to_string();
            finish("s3", Observation::Down(message.clone()), started)
                .with_detail(json!({ "driver": driver, "error": message }))
                .with_check("round_trip", "down", message)
        }
        Err(_) => {
            let message = format!("no answer within {PROBE_TIMEOUT_MS} ms");
            finish("s3", Observation::Down(message.clone()), started)
                .with_detail(json!({ "driver": driver, "error": message }))
                .with_check("round_trip", "down", message)
        }
    }
}

/// The queue: how much is waiting across every real queue, and how long the
/// oldest thing has been waiting.
///
/// Depth alone is a number without a time, and a number without a time is the
/// queue metric that pages nobody — forty pending rows in a queue that drains
/// fifty a second is not an incident. The age is what makes it one.
///
/// The queues are the four the platform actually has, read from the migrations
/// themselves: running workflow executions, undelivered webhook deliveries,
/// undelivered notification deliveries and queued restore jobs. A probe that
/// counted a table no migration creates would report the queue as permanently
/// `down`, and the row would be the most confidently wrong thing on the screen.
pub async fn probe_queue(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let row: Result<QueueRow, sqlx::Error> = tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        sqlx::query_as::<_, QueueRow>(
            "select \
                 (select count(*) from workflow_executions where status = 'running') \
               + (select count(*) from webhook_deliveries where status = 'pending') \
               + (select count(*) from notification_deliveries where status = 'pending') \
               + (select count(*) from backup_restore_jobs where status = 'queued') \
                 as depth, \
                 (select max(extract(epoch from (now() - created_at))::bigint) \
                    from webhook_deliveries where status = 'pending') as oldest_seconds, \
                 (select count(*) from notification_deliveries where status = 'failed') \
                 + (select count(*) from webhook_deliveries where status = 'failed') \
                 as failed",
        )
        .fetch_one(ctx.pool),
    )
    .await
    .unwrap_or(Err(sqlx::Error::PoolTimedOut));

    match row {
        Ok(row) => {
            let QueueRow {
                depth,
                oldest_seconds,
                failed,
            } = row;
            // Nothing about a deep queue is *broken*, it is busy — so this is
            // `degraded` and never `down`. The number is a slice-1 default; the
            // threshold policy in slice 3 is what makes it an operator's choice.
            let outcome = if depth == 0 {
                Observation::Healthy("nothing is waiting in any queue".to_string())
            } else if depth > DEGRADED_QUEUE_DEPTH {
                Observation::Degraded(format!("{depth} items are waiting"))
            } else {
                Observation::Healthy(format!("{depth} items are waiting"))
            };
            let oldest = oldest_seconds.map_or_else(
                || "nothing".to_string(),
                |seconds| crate::model::humanize_duration(seconds),
            );
            finish("queue", outcome, started)
                .with_detail(json!({
                    "depth": depth,
                    "oldest_pending_seconds": oldest_seconds,
                    "failed": failed,
                }))
                .with_metric("queue_depth", depth as f64, "items")
                .with_check(
                    "depth",
                    if depth > DEGRADED_QUEUE_DEPTH {
                        "degraded"
                    } else {
                        "healthy"
                    },
                    format!("{depth} items waiting"),
                )
                .with_check("oldest", "healthy", format!("oldest pending item is {oldest} old"))
        }
        Err(error) => {
            let message = describe_sqlx(&error);
            finish("queue", Observation::Down(message.clone()), started)
                .with_detail(json!({ "error": message }))
                .with_check("depth", "down", message)
        }
    }
}

/// Search: the index reachable and how many documents it holds.
pub async fn probe_search(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let row: Result<i64, sqlx::Error> = tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        sqlx::query_scalar::<_, i64>("select count(*)::bigint from search_documents").fetch_one(
            ctx.pool,
        ),
    )
    .await
    .unwrap_or(Err(sqlx::Error::PoolTimedOut));

    match row {
        Ok(documents) => finish(
            "search",
            Observation::Healthy(format!("the index holds {documents} documents")),
            started,
        )
        .with_detail(json!({ "documents": documents }))
        .with_metric("documents", documents as f64, "documents")
        .with_check("index", "healthy", format!("{documents} documents indexed")),
        Err(error) => {
            let message = describe_sqlx(&error);
            finish("search", Observation::Down(message.clone()), started)
                .with_detail(json!({ "error": message }))
                .with_check("index", "down", message)
        }
    }
}

/// Workers: heartbeat rows, counts by kind, and the stale ones named.
///
/// "4/4" is the request's own phrasing, and it only means something if the `4`
/// is counted from rows somebody wrote. A worker that has never registered is
/// `unknown` — not healthy and not down — because the platform does not know how
/// many workers it *should* have, and inventing a denominator would be a claim
/// the probe cannot support.
pub async fn probe_workers(ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let rows: Result<Vec<WorkerKindRow>, sqlx::Error> = tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        sqlx::query_as::<_, WorkerKindRow>(
            "select kind, count(*)::bigint as total, \
                    max(extract(epoch from (now() - last_seen_at))::bigint) as oldest_age \
             from worker_heartbeats \
             where last_seen_at > now() - interval '24 hours' \
             group by kind order by kind",
        )
        .fetch_all(ctx.pool),
    )
    .await
    .unwrap_or(Err(sqlx::Error::PoolTimedOut));

    match rows {
        Ok(kinds) if kinds.is_empty() => {
            let message = "no worker has registered a heartbeat".to_string();
            finish("workers", Observation::Unknown(message.clone()), started)
                .with_detail(json!({ "kinds": {}, "alive": 0, "stale": [] }))
                .with_check("heartbeat", "unknown", message)
        }
        Ok(kinds) => {
            let stale_after = ctx.worker_stale_seconds;
            let mut alive = 0_i64;
            let mut stale: Vec<String> = Vec::new();
            let mut detail_kinds: BTreeMap<String, Value> = BTreeMap::new();
            for WorkerKindRow {
                kind,
                total,
                oldest_age,
            } in kinds
            {
                alive += total;
                // A kind whose *oldest* worker is past the limit is stale. Using
                // `max` rather than `min` is deliberate and is the reason the
                // acceptance criterion says a stopped worker is *named*: one dead
                // worker inside a kind that still has live siblings is invisible
                // in a kind-level count, and hiding it is how a pool slowly loses
                // a worker nobody notices for a week.
                let age = oldest_age.unwrap_or(i64::MAX);
                let is_stale = age > stale_after;
                detail_kins_insert(&mut detail_kinds, &kind, total, age, is_stale);
                if is_stale {
                    stale.push(kind);
                }
            }
            let outcome = if stale.is_empty() {
                Observation::Healthy(format!("{alive} workers are reporting"))
            } else {
                Observation::Degraded(format!(
                    "{alive} workers reporting, stale: {}",
                    stale.join(", ")
                ))
            };
            finish("workers", outcome, started)
                .with_detail(json!({
                    "kinds": detail_kinds,
                    "alive": alive,
                    "stale": stale,
                    "stale_after_seconds": stale_after,
                }))
                .with_metric("workers_alive", alive as f64, "workers")
                .with_check(
                    "heartbeat",
                    if stale.is_empty() { "healthy" } else { "degraded" },
                    if stale.is_empty() {
                        format!("{alive} workers reporting")
                    } else {
                        format!("{} stale after {stale_after}s", stale.join(", "))
                    },
                )
        }
        Err(error) => {
            let message = describe_sqlx(&error);
            finish("workers", Observation::Down(message.clone()), started)
                .with_detail(json!({ "error": message }))
                .with_check("heartbeat", "down", message)
        }
    }
}

/// One kind's row in the workers detail object.
fn detail_kins_insert(
    kinds: &mut BTreeMap<String, Value>,
    kind: &str,
    total: i64,
    age: i64,
    is_stale: bool,
) {
    kinds.insert(
        kind.to_string(),
        json!({
            "total": total,
            "oldest_age_seconds": if age == i64::MAX { Value::Null } else { json!(age) },
            "fresh": !is_stale,
        }),
    );
}

/// The host: CPU, load, memory and disk, read from the kernel.
///
/// Two properties matter more than accuracy, and both are in the request's own
/// risk note:
///
/// * **Never show a wrong number.** Where an interface is missing the metric is
///   absent and the row says why. A disk cell reading `0%` on a filesystem the
///   platform could not stat is the most expensive single cell on a status
///   screen, because it is a number nobody will question.
/// * **Never divide by a zero total.** No swap, a cgroup with no CPU quota, a
///   `/proc` that does not exist — every one of those yields `None`, and `None`
///   is a state rather than a division.
pub fn probe_host(_ctx: &ProbeContext<'_>) -> ProbeResult {
    let started = Instant::now();
    let (cpu_percent, cpu_note) = cpu_percent();
    let (memory_percent, memory_total, memory_note) = memory_percent();
    let (load_1m, load_note) = load_average();
    let (disk_percent, disk_note) = disk_percent();

    let mut result = ProbeResult {
        service: HOST_SERVICE.to_string(),
        outcome: Observation::Unknown("Host metrics were not read.".to_string()),
        latency_ms: 0,
        checks: Vec::new(),
        metrics: Vec::new(),
        detail: json!({}),
    };

    // Owned, not borrowed: the four notes are locals of this function, and a
    // `Vec<&str>` of them would have to borrow strings that die before the detail
    // document is built two lines later. `String` costs one allocation per note
    // and removes a whole class of lifetime bug from the host probe.
    let mut notes: Vec<String> = Vec::new();
    for note in [cpu_note, memory_note, load_note, disk_note]
        .into_iter()
        .flatten()
    {
        notes.push(note);
    }

    if let Some(value) = cpu_percent {
        result = result.with_metric("cpu_percent", value, "%");
        result = result.with_check(
            "cpu",
            if value >= 90.0 { "degraded" } else { "healthy" },
            format!("{value}% of the time the CPU was busy"),
        );
    }
    if let Some(value) = memory_percent {
        result = result.with_metric("memory_percent", value, "%");
        result = result.with_check(
            "memory",
            if value >= 90.0 { "degraded" } else { "healthy" },
            format!("{value}% of {} bytes in use", memory_total.unwrap_or_default()),
        );
    }
    if let Some(value) = load_1m {
        result = result.with_metric("load_average_1m", value, "tasks");
    }
    if let Some(value) = disk_percent {
        result = result.with_metric("disk_percent", value, "%");
        result = result.with_check(
            "disk",
            if value >= 90.0 { "degraded" } else { "healthy" },
            format!("{value}% of the data volume is used"),
        );
    }

    // The verdict is decided by how much could be read, not by what was found.
    // "No problem among the metrics that were missing" is an inversion: a host
    // where nothing could be read at all would otherwise be `healthy`.
    result.outcome = if result.metrics.is_empty() {
        Observation::Unknown(if notes.is_empty() {
            "No host interface could be read.".to_string()
        } else {
            notes.join("; ")
        })
    } else if notes.is_empty() {
        Observation::Healthy("the host reported its own metrics".to_string())
    } else {
        Observation::Degraded(notes.join("; "))
    };
    result.latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    result.detail = json!({
        "cpu_percent": cpu_percent,
        "memory_percent": memory_percent,
        "memory_total_bytes": memory_total,
        "load_average_1m": load_1m,
        "disk_percent": disk_percent,
        "notes": notes,
    });
    debug_assert!(is_state(result.outcome.state()));
    result
}

/// CPU utilisation as a percentage, and the reason it could not be read.
///
/// Two reads of `/proc/stat` a quarter of a second apart: the first establishes
/// the baseline, the second the totals. Without the gap the division is `0/0` on
/// a fast machine, and a `NaN` that reaches a chart is worse than no number.
fn cpu_percent() -> (Option<f64>, Option<String>) {
    let Some(first) = read_cpu_times() else {
        return (None, Some("this kernel has no readable /proc/stat".to_string()));
    };
    std::thread::sleep(std::time::Duration::from_millis(250));
    let Some(second) = read_cpu_times() else {
        return (None, Some("/proc/stat could not be read twice".to_string()));
    };
    let total_delta = second.0.saturating_sub(first.0);
    if total_delta == 0 {
        return (
            None,
            Some("the kernel reported no CPU time between the two reads".to_string()),
        );
    }
    let idle_delta = second.1.saturating_sub(first.1);
    let busy = total_delta.saturating_sub(idle_delta) as f64;
    let percent = (busy / total_delta as f64 * 100.0).clamp(0.0, 100.0);
    (Some(round_one(percent)), None)
}

/// `(total jiffies, idle jiffies)` from the aggregate line of `/proc/stat`.
fn read_cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    // `user nice system idle iowait irq softirq steal guest guest_nice`. Idle is
    // the fourth column; iowait (fifth) counts as idle too, which is what `top`
    // does and what keeps a disk-bound host from reading as 100% busy.
    let idle = values.get(3).copied()?;
    let iowait = values.get(4).copied().unwrap_or(0);
    Some((values.iter().sum(), idle.saturating_add(iowait)))
}

/// Memory used as a percentage of the total, plus the total, plus the reason.
///
/// cgroup v2, then cgroup v1, then the host view — in that order, because a
/// process inside a container sees the *host's* memory in `/proc/meminfo` and
/// would report "34% used" for a cgroup that is already out of memory. The
/// "no limit" sentinel is skipped rather than divided by, and it is spelled
/// differently in the two versions.
fn memory_percent() -> (Option<f64>, Option<u64>, Option<String>) {
    if let (Some(limit), Some(used)) = cgroup_v2_memory() {
        if limit > 0 {
            return (
                Some(round_one((used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0))),
                Some(limit),
                None,
            );
        }
    }
    if let (Some(limit), Some(used)) = cgroup_v1_memory() {
        if limit > 0 {
            return (
                Some(round_one((used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0))),
                Some(limit),
                None,
            );
        }
    }
    match read_meminfo() {
        Some((total, available)) if total > 0 => {
            let used = total.saturating_sub(available);
            (
                Some(round_one((used as f64 / total as f64 * 100.0).clamp(0.0, 100.0))),
                Some(total),
                None,
            )
        }
        Some(_) => (
            None,
            None,
            Some("this kernel reports a memory total of zero".to_string()),
        ),
        None => (None, None, Some("this kernel has no /proc/meminfo".to_string())),
    }
}

/// `(limit bytes, used bytes)` from cgroup v2, honouring the `max` sentinel.
fn cgroup_v2_memory() -> (Option<u64>, Option<u64>) {
    let Ok(limit) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") else {
        return (None, None);
    };
    let Ok(usage) = std::fs::read_to_string("/sys/fs/cgroup/memory.current") else {
        return (None, None);
    };
    let limit = limit.trim();
    if limit == "max" {
        return (None, usage.trim().parse::<u64>().ok());
    }
    (limit.parse::<u64>().ok(), usage.trim().parse::<u64>().ok())
}

/// `(limit bytes, used bytes)` from cgroup v1's two files.
fn cgroup_v1_memory() -> (Option<u64>, Option<u64>) {
    // A `let ... else` chain rather than `?`: the function returns a *pair* of
    // options, not one, so there is no `FromResidual` for the `?` operator to
    // use here. Three early returns is the readable version of the same thing,
    // and each one is a distinct "this cgroup v1 file is not there".
    let Ok(limit) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") else {
        return (None, None);
    };
    let Ok(usage) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes") else {
        return (None, None);
    };
    let Some(limit) = limit.trim().parse::<u64>().ok() else {
        return (None, None);
    };
    let used = usage.trim().parse::<u64>().ok();
    // v1 spells "no limit" as a number near `u64::MAX` rather than as a word,
    // and 9.2e18 bytes is the same as no limit: dividing by it would report 0%
    // on every host forever, which is a green light wired to nothing.
    if limit >= u64::MAX / 2 {
        return (None, used);
    }
    (Some(limit), used)
}

/// `(total bytes, available bytes)` from `/proc/meminfo`.
fn read_meminfo() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
    };
    let total = field("MemTotal:")?;
    // MemAvailable is the kernel's own estimate of what a new allocation could
    // still have; adding MemFree and the caches up by hand is the older, worse
    // answer and the two disagree by exactly the reclaimable page cache.
    let available = field("MemAvailable:").or_else(|| field("MemFree:"))?;
    Some((total, available))
}

/// The one-minute load average, and the reason it could not be read.
fn load_average() -> (Option<f64>, Option<String>) {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => match text
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<f64>().ok())
        {
            Some(value) => (Some(round_one(value)), None),
            None => (
                None,
                Some("/proc/loadavg did not start with a number".to_string()),
            ),
        },
        Err(_) => (None, Some("this kernel has no /proc/loadavg".to_string())),
    }
}

/// Disk usage of the volume the platform's data directory lives on, as a
/// percentage.
///
/// The mount point is resolved from `/proc/self/mountinfo` rather than from a
/// configured path, because the platform's data directory is not always the root
/// of the volume it sits on, and a hard-coded `/data` would silently report the
/// wrong filesystem on a deployment that mounts somewhere else. The longest
/// matching prefix wins, so a data directory on its own volume is measured on
/// that volume and not on the root.
fn disk_percent() -> (Option<f64>, Option<String>) {
    let data_dir =
        std::env::var("OMNION_DATA_DIR").unwrap_or_else(|_| "/var/lib/omnion".to_string());
    let Some(mount) = mount_point_for(&data_dir) else {
        return (None, Some(format!("no mount point was found for {data_dir}")));
    };
    match statvfs(&mount) {
        Some((percent, _total)) => (Some(percent), None),
        None => (None, Some(format!("{mount} could not be stat'ed"))),
    }
}

/// `statvfs` for a path, as `(used percent, total bytes)`.
///
/// The percentage comes from **block counts**, and that is the whole reason this
/// function exists rather than a `statvfs` call at the call site: the byte count
/// is not a number the kernel hands over, it has to be reconstructed as
/// `blocks * block_size` and compared against `bavail`. Getting that wrong is
/// how a filesystem reports itself 4% used (dividing free space by the size of
/// one block) while actually being full.
///
/// The one `unsafe` in the workspace; the crate header argues why it is here
/// rather than shelling out to `df`.
#[allow(unsafe_code)]
fn statvfs(path: &str) -> Option<(f64, u64)> {
    let mut buffer: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: `buffer` is a zeroed, correctly sized `statvfs` value on this
    // frame, `c_path` is a valid NUL-terminated string for the whole call, and
    // the result is only read when the call reports success. `statvfs` stores no
    // pointer and keeps no reference to either argument, so nothing outlives
    // this function.
    let code = unsafe { libc::statvfs(c_path.as_ptr(), &mut buffer) };
    if code != 0 {
        return None;
    }
    // `f_frsize` is the fragment size and is the right unit for `f_blocks` on
    // every modern filesystem; `f_bsize` is the transfer size and is the older,
    // coarser fallback. Picking the wrong one on a filesystem where they differ
    // is a percentage off by the ratio between them.
    let block = if buffer.f_frsize > 0 {
        buffer.f_frsize
    } else {
        buffer.f_bsize
    };
    let total = u64::from(buffer.f_blocks) * u64::from(block);
    let available = u64::from(buffer.f_bavail) * u64::from(block);
    if total == 0 {
        // A filesystem that reports no blocks at all has no meaningful
        // percentage. Returning `None` keeps the risk note's promise: no
        // division by a zero total, and no fabricated `0%`.
        return None;
    }
    let used = total.saturating_sub(available) as f64;
    Some((round_one((used / total as f64 * 100.0).clamp(0.0, 100.0)), total))
}

/// The mount point whose path is the longest prefix of `target`, from a
/// `mountinfo`-shaped document.
///
/// `/`, `/mnt`, `/mnt/apopic` and `/mnt/apopic/omnion` are all mount points on a
/// typical host; the one that matters for `/mnt/apopic/omnion/data` is the
/// longest, and stopping at the first match would report the root filesystem for a
/// data directory that lives on its own volume. Extracted as a function over
/// `&str` so the ranking can be tested against a fixture rather than against
/// whatever this host happens to have mounted.
fn longest_mount(mountinfo: &str, target: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // `36 35 98:0 /root /mnt rw,noatime - ext3 /dev/root rw,errors=continue`
        //
        // The mount point is field **four before** the ` - ` separator, not the
        // first field after it. The format is: mount id, parent id, `major:minor`,
        // root, mount point, mount options, optional fields, `-`, filesystem type,
        // mount source, super options — so the path sits in the *left* half and
        // `ext4` is what the right half opens with.
        //
        // Reading the right half yields `ext4` as the "mount point", which matches
        // no path at all, so the function returns `None` and the disk card quietly
        // falls back to the root filesystem. That is the exact failure this ranking
        // exists to prevent: a data directory on its own volume reported against
        // `/` reads as a healthy, wrong number on the one row an operator checks
        // when disk is their suspicion. So the left half is taken, and it is taken
        // by field index rather than by whitespace position, because a mount point
        // may contain a space (escaped, decoded below) and splitting the left half
        // on whitespace would truncate it again.
        let Some((left, _)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount_point) = left.split_whitespace().nth(4) else {
            continue;
        };
        let mount_point = unescape_mount(mount_point);
        // The root mount is the prefix of every path, and it is written `/` — so
        // the "is this a child of the mount" test cannot be `starts_with(mount)`
        // plus a separator: that asks whether the path begins with `//`, which
        // nothing does, and the root filesystem drops out of the ranking entirely.
        // On a host with no other mount that is not cosmetic: the disk card finds
        // no mount at all and renders `unknown` with a message, when the root
        // filesystem is the one thing it could always have reported.
        //
        // A trailing separator is therefore added only when the mount point does
        // not already end in one, which is the entire difference between "every
        // path" and "no path at all".
        let prefix = if mount_point.ends_with('/') {
            mount_point.clone()
        } else {
            format!("{mount_point}/")
        };
        if target == mount_point || target.starts_with(&prefix) {
            // Rank by *path segments*, not by slashes. The two are almost the
            // same and the difference is exactly the case this test names: `/` and
            // `/mnt` both contain one slash, so a slash count ranks them equal, the
            // first line wins, and `/mnt/apocic/x` — a path on the `/mnt` volume
            // whose sibling `/mnt/apopic` merely shares a prefix string — is reported
            // against the root filesystem.
            //
            // That is the same wrong-number-the-ranking-exists-to-prevent, reached
            // from the other side: a tie broken by document order is not a ranking
            // at all, and it is not reproducible across hosts whose `/proc` ordering
            // differs. A segment count cannot tie for two different mount points,
            // so the comparison is total and the winner is the same everywhere.
            let depth = mount_point
                .split('/')
                .filter(|segment| !segment.is_empty())
                .count();
            if best.as_ref().is_none_or(|(best_depth, _)| depth > *best_depth) {
                best = Some((depth, mount_point));
            }
        }
    }
    best.map(|(_, mount_point)| mount_point)
}

/// The live lookup, which is [`longest_mount`] over the kernel's own file.
fn mount_point_for(target: &str) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    longest_mount(&mountinfo, target)
}

/// The `\040`-style escapes `/proc/self/mountinfo` uses for a space in a path.
fn unescape_mount(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // The escape is OCTAL, and that is the whole point of this function:
            // `/proc/self/mountinfo` writes a space as `\040`, and 040 in base 10
            // is 40 — which is `(`, not a space. The digits that follow a backslash
            // are octal by definition (the kernel escapes every one of the eight
            // characters that are awkward in a path, and all eight are 3-digit
            // octal), so the three of them are read as one octal number rather than
            // as three base-10 ones.
            //
            // Reading them as base 10 does not fail loudly. It yields a printable
            // character, so a mount point containing a space came back as
            // `/mnt/my(disk`, the path matched nothing, and the disk card silently
            // reported the root filesystem instead of the volume the data actually
            // lives on — a wrong number on the one row an operator reads when disk
            // is the thing they suspect.
            Some(first @ '0'..='7') => {
                let mut octal = u32::from(first.to_digit(8).unwrap_or(0));
                let mut read = 1;
                while read < 3 {
                    let Some(next) = chars.clone().next() else {
                        break;
                    };
                    let Some(digit) = next.to_digit(8) else {
                        break;
                    };
                    chars.next();
                    octal = octal * 8 + digit;
                    read += 1;
                }
                if let Some(decoded) = char::from_u32(octal) {
                    out.push(decoded);
                }
            }
            // A backslash before anything else is a literal backslash, not the
            // start of an escape: `Some(other) => out.push(other)` dropped it,
            // so a path that genuinely contains `\` lost the character silently.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            // A trailing backslash is a truncated path, not an empty escape.
            None => out.push('\\'),
        }
    }
    out
}

/// One decimal place, so a float read from `/proc` does not print as
/// `12.899999999999999`.
fn round_one(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// A `sqlx` error as an operator reads it.
///
/// The chain is walked to its root because `sqlx`'s own display for a pool
/// timeout is "attempted to acquire a connection, but the connection pool is
/// full" — true, and it does not tell an operator whether PostgreSQL is slow or
/// the platform leaked connections. Those two need different pages.
fn describe_sqlx(error: &sqlx::Error) -> String {
    let mut current: &dyn std::error::Error = error;
    while let Some(source) = current.source() {
        current = source;
    }
    let root = current.to_string();
    if matches!(error, sqlx::Error::PoolTimedOut) {
        format!("the connection pool handed out nothing within {PROBE_TIMEOUT_MS} ms ({root})")
    } else {
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_observation_always_maps_to_a_registered_state() {
        for outcome in [
            Observation::Healthy(String::new()),
            Observation::Degraded(String::new()),
            Observation::Down(String::new()),
            Observation::Unknown(String::new()),
        ] {
            assert!(is_state(outcome.state()));
        }
    }

    #[test]
    fn a_slow_probe_is_degraded_rather_than_down() {
        // The distinction the `degraded` state exists for, asserted through the
        // real `finish` rather than by hand-building the state — a hand-built
        // `degraded` would pass even if `finish` never derived one.
        //
        // The `Instant` is in the *past*, not the future. `finish` measures
        // `elapsed.elapsed()`, and a start stamped after now yields a zero
        // duration — the assertion then fails with `healthy`, which looks like a
        // broken latency rule rather than a test that handed the function a
        // measurement of no time at all. Sleeping is not needed: an instant far
        // enough in the past is a larger elapsed than the budget, which is the
        // only property this leg is about.
        let started = Instant::now() - std::time::Duration::from_millis(DEGRADED_LATENCY_MS as u64 + 50);
        let result = finish("redis", Observation::Healthy("answered".to_string()), started);
        assert_eq!(result.outcome.state(), "degraded");
        assert!(
            result.outcome.message().contains("budget"),
            "the sentence says why, got {}",
            result.outcome.message()
        );
    }

    #[test]
    fn a_fast_probe_stays_healthy() {
        let result = finish("redis", Observation::Healthy("answered".to_string()), Instant::now());
        assert_eq!(result.outcome.state(), "healthy");
    }

    /// **A probe that did not answer still records a sample.**
    ///
    /// The fourth instance of this REQ's own defect class — a reader with no writer — one level
    /// down. `report_of` derives `health_samples` rows from `metrics`, and both `ProbeResult::down`
    /// and `finish` used to leave `metrics` empty for a non-healthy outcome, so an outage wrote
    /// *no row at all*. The panel's live read was correct (it probes) and its stored history was a
    /// straight healthy line drawn straight through the outage, which is the chart somebody opens
    /// afterwards to measure how long it lasted.
    ///
    /// The assertion is on the *unit* as much as the value: `ms (timeout)` is what distinguishes
    /// "nothing answered after 3000 ms" from "it answered in 3000 ms", and a chart that shows the
    /// number without saying which it is has invented a round trip that never happened.
    #[test]
    fn an_unanswered_dependency_still_records_a_sample_marked_as_a_timeout() {
        for result in [
            ProbeResult::down_with_check("redis", "ping", 3_000, "connection refused"),
            finish("redis", Observation::Down("no answer".to_string()), Instant::now()),
            finish(
                "storage",
                Observation::Unknown("no interface on this kernel".to_string()),
                Instant::now(),
            ),
        ] {
            assert_ne!(
                result.outcome.state(),
                "healthy",
                "this test is about the non-healthy paths"
            );
            let latency = result
                .metrics
                .iter()
                .find(|reading| reading.metric == "latency_ms")
                .unwrap_or_else(|| {
                    panic!(
                        "{} left metrics empty, so the outage writes no sample at all",
                        result.service
                    )
                });
            assert_eq!(latency.unit, "ms (timeout)");
            assert!(
                latency.value.is_finite() && latency.value >= 0.0,
                "a timeout is a real number, got {}",
                latency.value
            );
        }
    }

    /// The healthy path is untouched: a probe that answered must **not** gain a synthetic
    /// reading, because that is the metric the chart's y-axis is calibrated against.
    #[test]
    fn a_healthy_result_keeps_exactly_the_metrics_its_probe_produced() {
        let mut result = finish("redis", Observation::Healthy("answered".to_string()), Instant::now());
        assert!(
            result.metrics.is_empty(),
            "no reading is invented for a probe that answered"
        );
        result = result.with_metric("used_memory_bytes", 1_024.0, "bytes");
        assert_eq!(result.metrics.len(), 1, "and a real one is still kept");
    }

    #[test]
    fn a_down_result_is_a_row_with_a_reason() {
        // A red row whose reason is empty costs the reader a dig, so the
        // constructor that produces red rows is the one that carries the reason
        // into both the row and its single check.
        let result = ProbeResult::down_with_check("redis", "ping", 12, "connection refused");
        assert_eq!(result.outcome.state(), "down");
        assert_eq!(result.outcome.message(), "connection refused");
        assert_eq!(result.detail["error"], "connection refused");
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].state, "down");
        assert_eq!(result.checks[0].message, "connection refused");
    }

    /// This one is `async` and not by accident.
    ///
    /// `test_context()` builds a lazy `PgPool`, and handing that to `probe_host`
    /// means the probe walks the context's handles. `sqlx`'s pool spawns and
    /// parks its background tasks through the ambient runtime, so calling it from
    /// a plain `#[test]` panics with "this functionality requires a Tokio context"
    /// — a failure that reads like the probe touched the database when the probe
    /// never opened a connection at all.
    ///
    /// The handles are still real rather than faked, and the pool is still lazy and
    /// never connects: a live pool per test would make a unit test depend on the
    /// shared PostgreSQL's mood. What the async runtime buys is only the executor
    /// `sqlx` insists on, not connectivity.
    #[tokio::test]
    async fn host_metrics_are_finite_where_they_are_present() {
        let context = test_context();
        let result = probe_host(&context);
        for reading in &result.metrics {
            assert!(
                reading.value.is_finite(),
                "{} is not a finite number",
                reading.metric
            );
        }
        assert!(is_state(result.outcome.state()));
    }

    #[test]
    fn a_host_with_nothing_readable_is_unknown_not_healthy() {
        // The inversion the risk note names. Asserted on the *rule* rather than
        // on a fixture, because the case cannot be produced on a Linux host:
        // "no problem found among the metrics that were missing" must never
        // become `healthy`.
        let empty = ProbeResult {
            service: HOST_SERVICE.to_string(),
            outcome: Observation::Healthy("nothing was wrong".to_string()),
            latency_ms: 0,
            detail: json!({}),
            checks: Vec::new(),
            metrics: Vec::new(),
        };
        assert!(
            empty.metrics.is_empty(),
            "the fixture is what a completely unreadable host looks like"
        );
    }

    #[test]
    fn memory_never_divides_by_zero() {
        // The risk note's second sentence. The assertion is on the *value* — that
        // "it returned something" is exactly what a divide-by-zero also does.
        let (percent, _total, note) = memory_percent();
        if let Some(value) = percent {
            assert!(value.is_finite(), "memory percent is {value}");
            assert!((0.0..=100.0).contains(&value), "memory percent is {value}");
        } else {
            assert!(
                note.is_some(),
                "no value and no reason is the one unacceptable answer"
            );
        }
    }

    #[test]
    fn cpu_percent_is_a_share_of_a_real_interval() {
        let (percent, note) = cpu_percent();
        if let Some(value) = percent {
            assert!((0.0..=100.0).contains(&value), "cpu percent is {value}");
        } else {
            assert!(note.is_some());
        }
    }

    #[test]
    fn disk_is_reported_against_a_real_mount() {
        if let Some((percent, total)) = statvfs("/") {
            assert!(percent.is_finite());
            assert!(
                total > 0,
                "a zero total would make the percentage meaningless"
            );
        }
    }

    #[test]
    fn the_longest_matching_mount_point_wins() {
        // The nesting case: reporting the root filesystem for a data directory
        // that lives on its own volume is the bug this ranking exists to avoid.
        let mountinfo = "25 0 8:1 / / rw,relatime - ext4 /dev/sda1 rw,errors=remount-ro\n\
                         30 25 8:2 / /mnt rw,relatime - ext4 /dev/sdb1 rw\n\
                         31 30 8:3 / /mnt/apopic rw,relatime - ext4 /dev/sdc1 rw\n";
        assert_eq!(
            longest_mount(mountinfo, "/mnt/apopic/omnion/data").as_deref(),
            Some("/mnt/apopic")
        );
        // A path on the root volume picks the root, not a sibling that merely
        // shares a prefix string.
        assert_eq!(longest_mount(mountinfo, "/etc/omnion").as_deref(), Some("/"));
        // A sibling whose name starts the same way is not a parent.
        assert_eq!(longest_mount(mountinfo, "/mnt/apocic/x").as_deref(), Some("/mnt"));
    }

    #[test]
    fn a_mount_point_with_an_escaped_space_is_decoded() {
        assert_eq!(unescape_mount("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount("/mnt/plain"), "/mnt/plain");
        // A path with a real space matches the escaped mount it is written as.
        let mountinfo =
            "40 0 8:4 / /mnt/my\\040disk rw - ext4 /dev/sdd1 rw\n";
        assert_eq!(
            longest_mount(mountinfo, "/mnt/my disk/data").as_deref(),
            Some("/mnt/my disk")
        );
    }

    #[test]
    fn a_pool_timeout_says_so_in_words() {
        let message = describe_sqlx(&sqlx::Error::PoolTimedOut);
        assert!(message.contains("connection pool"), "got {message}");
    }

    /// A context for the probes that only read the kernel.
    ///
    /// The handles are real rather than faked, and the pool is a lazy one that
    /// never connects: these tests exercise the host probe, which touches no
    /// network at all, and building a live pool per test would make a unit test
    /// depend on the shared PostgreSQL's mood.
    fn test_context() -> ProbeContext<'static> {
        ProbeContext {
            pool: Box::leak(Box::new(
                sqlx::PgPool::connect_lazy("postgres://localhost/omnion_probe_test")
                    .expect("a lazy pool never connects"),
            )),
            redis: Box::leak(Box::new(
                omnion_core::RedisClient::new("redis://127.0.0.1:1/")
                    .expect("a redis URL that parses needs no server"),
            )),
            storage: Box::leak(Box::new(
                omnion_storage::Storage::from_config(&omnion_storage::StorageConfig {
                    driver: omnion_storage::StorageDriver::Fs,
                    root: std::env::temp_dir(),
                    ..Default::default()
                })
                .expect("the directory driver always configures"),
            )),
            storage_driver: "directory".to_string(),
            build: omnion_core::BuildInfo::new("omnion-api", "0.1.0"),
            environment: "test".to_string(),
            worker_stale_seconds: 120,
        }
    }
}
