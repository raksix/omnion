//! Two acceptance criteria that only a *real* dependency can close (REQ-014, slice 4).
//!
//! Both of these have been open since slice 1, and both are open for the same reason: every
//! walk in this suite reaches for an **unreachable** Redis (`redis://127.0.0.1:1`) and a
//! `NULL` organization, because nine sibling writers share the box's real Redis. That fixture is
//! correct for what it claims — a closed port *is* `down` — and it is the reason two sentences in
//! the request have never been true of the platform:
//!
//! 1. **"Stopping Redis flips its row to `down` within one interval and restores on recovery."**
//!    Half of that sentence can never be proven against a port that was never open. The
//!    `down` leg is proven; "restores on recovery" was unreachable *by construction*, and the
//!    recovery path is exactly where the interesting code lives: `RedisClient::connection()`
//!    caches a `ConnectionManager` and a cached manager to a server that went away is the
//!    classic "it says down for ever after it comes back" bug. This suite starts a **throwaway
//!    `redis-server` on its own port**, stops it, and starts it again — so the assertion is
//!    about the platform, not about a fixture that was never up.
//!
//! 2. **"an operations endpoint subscribes to `health.service.degraded` and
//!    `health.service.recovered`"** and *"webhook for degraded/recovered"*. The emitters shipped
//!    in `f57144f9`/`1df104af` with unit tests over `Announcement::new`, and the fan-out
//!    (`subscribers()` → `bus::emit` → `enqueue_fanout`) was written without a single live
//!    recipient. That is the same defect class as this REQ's own history — a writer proven
//!    against a fixture — one level up: an emitter that fans out to nobody is green forever. So
//!    this walk creates a **real organization with a real enabled endpoint subscribed to
//!    `health`**, drives a real transition, and reads the delivery out of the queue.
//!
//! ## Why the server is ours and not the box's
//!
//! `docker stop omnion-redis` would take out every sibling writer, and the QA pass, and the
//! running platform. A port of our own costs a `redis-server` process that this file starts and
//! kills, which is the only kind of outage a test may cause on a shared box.
//!
//! ## What the walks refuse to accept
//!
//! * A green recovery leg is not "the probe returned healthy again" — it is *the row's state
//!   moved back through the real store*, and the incident that the outage opened was **resolved
//!   with a duration**. A recovery that never resolved its incident leaves the panel showing a
//!   permanent outage, and the probe is green while the timeline says otherwise.
//! * A delivered event is not "an `events` row exists". `enqueue_fanout` is where an event becomes
//!   a *delivery*, and the request's promise is that an operator's endpoint receives one. The
//!   walk asserts on `webhook_deliveries`, on the organization it belongs to, and on the payload
//!   fields the catalogue calls required — because a row in `events` whose `organization_id` is
//!   `NULL` is precisely the failure `emit_for_subscribers` exists to prevent, and it is the one
//!   a `select count(*) from events` assertion would have called a pass.

#![allow(clippy::too_many_lines)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration as StdDuration;

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{Db, RedisClient};
use omnion_health::ProbeContext;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// A scratch database
// ---------------------------------------------------------------------------------------------

struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let maintenance = match Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        {
            Ok(db) => db,
            Err(error) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({error})");
                return None;
            }
        };
        let database = format!("omnion_healthr_{}", Uuid::new_v4().simple());
        if let Err(error) = sqlx::query(&format!(r#"create database "{database}""#))
            .execute(maintenance.pool())
            .await
        {
            eprintln!("SKIP: a scratch database could not be created ({error})");
            return None;
        }
        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            r#"drop database if exists "{database}" with (force)"#
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

// ---------------------------------------------------------------------------------------------
// A throwaway Redis we are allowed to stop
// ---------------------------------------------------------------------------------------------

/// A `redis-server` on a port of our own, which this file may stop and start.
struct TestRedis {
    port: u16,
    dir: PathBuf,
    child: Option<Child>,
}

/// The first port nobody is listening on, so two concurrent walks do not collide.
///
/// Bound and released rather than probed with a connect: a `TcpListener` on port 0 is handed a
/// port by the kernel that is free *now*, and the walk that holds the reservation wins it. A
/// "try to connect, if refused then use" probe has the opposite property — it finds a port that
/// was free a moment ago, and three concurrent walks all find the same one.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a loopback listener must bind")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

impl TestRedis {
    /// Start a server on a free port, in its own directory.
    ///
    /// `maxmemory 0` and no persistence: the walk is about whether a socket answers, and a
    /// server that starts slowly because it is loading a dump from a previous run is a flake
    /// that has nothing to do with the platform.
    fn start(port: u16, dir: &Path) -> Self {
        let child = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                dir.to_str().expect("a temp path is UTF-8"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("redis-server must be on PATH for this walk to mean anything");
        let server = Self {
            port,
            dir: dir.to_path_buf(),
            child: Some(child),
        };
        server.wait_until_answering();
        server
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}/", self.port)
    }

    /// The handle the probe reads through. Rebuilt on demand, so the walk can hold a client
    /// that existed *before* the outage and prove it recovers rather than proving a fresh one.
    fn client(&self) -> RedisClient {
        RedisClient::new(&self.url()).expect("a parseable URL needs no server")
    }

    /// A dead child's port is free only once the kernel has released it, and a `redis-server` that
    /// has not been reaped still holds the socket, so this is `wait()` rather than `kill()` alone.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let deadline = std::time::Instant::now() + StdDuration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&self.addr(), StdDuration::from_millis(200))
                .is_err()
            {
                return;
            }
            std::thread::sleep(StdDuration::from_millis(50));
        }
        panic!(
            "the throwaway redis on :{} never released its port",
            self.port
        );
    }

    /// Start it again on the **same port**, which is the whole point of the recovery leg.
    fn restart(&mut self) {
        self.child = Some(
            Command::new("redis-server")
                .args([
                    "--port",
                    &self.port.to_string(),
                    "--bind",
                    "127.0.0.1",
                    "--save",
                    "",
                    "--appendonly",
                    "no",
                    "--dir",
                    self.dir.to_str().expect("a temp path is UTF-8"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("redis-server must be on PATH for this walk to mean anything"),
        );
        self.wait_until_answering();
    }

    /// Block until the port accepts a connection, or give up loudly.
    ///
    /// A TCP connect rather than a `PING`, and that choice is deliberate. `redis-server` binds
    /// its listener *after* it is ready to answer, so an accepted connection is a readiness
    /// signal that costs nothing — and doing it with `PING` would mean either blocking the
    /// walk's runtime on a setup wait (this is a `#[tokio::test]`, where a blocking connect
    /// stalls every other task) or standing up a second runtime inside it, which panics rather
    /// than waits. The probe's own `PING` is the real proof, and it runs a few lines later
    /// through the same handle the platform uses.
    fn wait_until_answering(&self) {
        let deadline = std::time::Instant::now() + StdDuration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&self.addr(), StdDuration::from_millis(250))
                .is_ok()
            {
                return;
            }
            std::thread::sleep(StdDuration::from_millis(50));
        }
        panic!(
            "the throwaway redis on :{} never started answering",
            self.port
        );
    }

    fn addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.port))
    }
}

impl Drop for TestRedis {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------------------------
// The probe fixture
// ---------------------------------------------------------------------------------------------

fn directory_storage() -> omnion_storage::Storage {
    let mut config = omnion_storage::StorageConfig::default();
    config.driver = omnion_storage::StorageDriver::Fs;
    config.root = std::env::temp_dir();
    omnion_storage::Storage::from_config(&config).expect("the directory driver always configures")
}

/// A context whose Redis handle is the one under test.
fn context<'a>(
    pool: &'a PgPool,
    redis: &'a RedisClient,
    storage: &'a omnion_storage::Storage,
) -> ProbeContext<'a> {
    ProbeContext {
        pool,
        redis,
        storage,
        storage_driver: "directory".to_string(),
        build: omnion_core::BuildInfo::new("omnion-api", "0.1.0"),
        environment: "test".to_string(),
        worker_stale_seconds: 120,
    }
}

/// The state the panel would render for one service, read back through the real store.
async fn stored_state(pool: &PgPool, service: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "select state from health_samples where service = $1 \
         order by sampled_at desc, id desc limit 1",
    )
    .bind(service)
    .fetch_optional(pool)
    .await
    .expect("the latest sample must be readable")
}

/// The open incident for a service, if any.
async fn open_incident_row(pool: &PgPool, service: &str) -> Option<(Uuid, String)> {
    sqlx::query_as::<_, (Uuid, String)>(
        "select id, to_state from health_incidents \
         where service = $1 and resolved_at is null",
    )
    .bind(service)
    .fetch_optional(pool)
    .await
    .expect("the incident list must be readable")
}

// ---------------------------------------------------------------------------------------------
// Walk 1 — a dependency that comes back
// ---------------------------------------------------------------------------------------------

/// Redis answers, stops answering, and answers again — and the incident it opened is **resolved**.
#[tokio::test]
async fn a_stopped_dependency_recovers_and_resolves_its_incident() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();
    let storage = directory_storage();
    let dir = std::env::temp_dir().join(format!("omnion-health-redis-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("the temp dir must be creatable");

    if Command::new("redis-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: redis-server is not installed, so a real outage cannot be staged");
        harness.dispose().await;
        return;
    }
    let port = free_port();
    let mut server = TestRedis::start(port, &dir);
    // One client for the whole walk, built while the server was already up. If the recovery leg
    // only worked with a *fresh* handle, the panel's long-lived handle would still be stuck —
    // which is the bug this test exists to catch.
    let client = server.client();
    let ctx = context(pool, &client, &storage);

    // 1. Up.
    let (healthy, _) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    assert_eq!(
        healthy
            .services
            .iter()
            .find(|service| service.service == "redis")
            .map(|service| service.state.as_str()),
        Some("healthy"),
        "a server that is up must be healthy before the outage means anything"
    );
    assert!(
        open_incident_row(pool, "redis").await.is_none(),
        "and it must not have opened an incident on the way there"
    );

    // 2. Down, for real.
    server.stop();
    let (down, policy) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    assert_eq!(
        down.services
            .iter()
            .find(|service| service.service == "redis")
            .map(|service| service.state.as_str()),
        Some("down"),
        "a stopped dependency is down, not a missing row"
    );
    assert_eq!(
        stored_state(pool, "redis").await.as_deref(),
        Some("down"),
        "and the *stored* sample says down, because the panel reads stored samples"
    );

    let (incident_id, to_state) = open_incident_row(pool, "redis")
        .await
        .expect("a real outage must open exactly one incident");
    assert_eq!(
        to_state, "down",
        "the incident records the state that opened it"
    );

    let degraded = policy
        .transitions
        .iter()
        .find(|entry| entry.service == "redis")
        .expect("the outage is a transition the store acted on");
    assert!(
        degraded.outcome.incident().is_some(),
        "and the store must have acted on it, not been beaten by a concurrent run"
    );

    // A second run while the outage is ongoing opens nothing new. Without this the walk would
    // accept an implementation that opens one incident per probe run, which is exactly what the
    // request's "one incident per transition" promise is about.
    let (_, repeat) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    assert!(
        repeat
            .transitions
            .iter()
            .all(|entry| entry.service != "redis"),
        "a steady outage is not a new transition on every run"
    );
    assert_eq!(
        open_incident_row(pool, "redis").await.map(|(_, _)| ()),
        Some(()),
        "and the second run must not have left a second open incident"
    );

    // 3. Up again, on the same port, through the same client.
    server.restart();
    let (recovered, policy) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    assert_eq!(
        recovered
            .services
            .iter()
            .find(|service| service.service == "redis")
            .map(|service| service.state.as_str()),
        Some("healthy"),
        "the same client handle must recover when the server comes back, not stay down for ever"
    );
    assert_eq!(
        stored_state(pool, "redis").await.as_deref(),
        Some("healthy"),
        "and the recovery must be a stored sample, so the 24 h trend draws the dip"
    );

    let resolution = policy
        .transitions
        .iter()
        .find(|entry| entry.service == "redis")
        .expect("the recovery is a transition the store acted on");
    let resolved = resolution
        .outcome
        .incident()
        .expect("a recovery resolves the open incident");
    assert_eq!(
        resolved.id, incident_id,
        "recovery must resolve *the* incident the outage opened, not open a second one"
    );
    // `Some`, not `Some(> 0)`. `duration_seconds` is `resolved_at - started_at` in whole
    // seconds, and a local `redis-server` genuinely goes down and back up inside one second —
    // so `> 0` would be a claim about how fast this box starts a process, and it would flake
    // on a fast one. What the criterion actually claims is that recovery *records* the
    // duration; `None` is what an unresolved incident returns, so `Some` is the assertion
    // that distinguishes "resolved" from "forgotten".
    assert!(
        resolved.duration_seconds().is_some(),
        "a resolved incident must carry a duration, not read as ongoing for ever"
    );
    assert!(
        resolved.duration_seconds().unwrap_or(-1) >= 0,
        "and the duration the database computed cannot be negative"
    );

    assert!(
        open_incident_row(pool, "redis").await.is_none(),
        "nothing may be left open after the recovery"
    );

    server.stop();
    drop(server);
    drop(storage);
    harness.dispose().await;
}

// ---------------------------------------------------------------------------------------------
// Walk 2 — a fact that reaches somebody
// ---------------------------------------------------------------------------------------------

/// One organization, one real endpoint subscribed to `health`, one real outage, one real
/// delivery.
#[tokio::test]
async fn a_degradation_reaches_the_endpoints_that_subscribed_to_health() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool();

    // The helper is `create_organization`, not `organization`: a local binding called
    // `organization` would shadow it, and the *second* call in this walk is exactly where
    // that shadowing shows up — as a "expected function, found Uuid" error three lines after
    // the line that caused it.
    let subscriber_org = create_organization(pool, "ops").await;
    // A second tenant that subscribed to something else: the assertion below that only the
    // `health` subscriber receives anything is what stops "fans out to everybody" from passing.
    let other = create_organization(pool, "unrelated").await;
    let subscriber = create_endpoint(
        pool,
        subscriber_org,
        "operations",
        &[
            "health",
            "health.service.degraded",
            "health.service.recovered",
        ],
    )
    .await;
    create_endpoint(pool, other, "content", &["content.published"]).await;

    let dir =
        std::env::temp_dir().join(format!("omnion-health-fanout-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("the temp dir must be creatable");
    let port = free_port();
    let mut server = TestRedis::start(port, &dir);
    let client = server.client();
    let storage = directory_storage();
    let ctx = context(pool, &client, &storage);

    // Healthy first, so the outage below is a *transition* and not a first sighting.
    omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");

    server.stop();
    let (_, degraded_policy) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    let announced = omnion_api::health_events::announce_changes(pool, &degraded_policy).await;
    assert_eq!(
        announced, 1,
        "exactly one fact: the one organization subscribed to `health`"
    );

    server.restart();
    let (_, recovery_policy) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    let announced = omnion_api::health_events::announce_changes(pool, &recovery_policy).await;
    assert_eq!(announced, 1, "and exactly one on the way back");

    // The delivery, not just the event row. `enqueue_fanout` is where a fact becomes something a
    // receiver gets, and an event with no organization is recorded and delivered to nobody —
    // which is the exact failure this slice was written to prevent.
    let deliveries: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "select d.status::text, e.name, e.payload from webhook_deliveries d \
         join events e on e.id = d.event_id \
         where d.endpoint_id = $1 order by d.id",
    )
    .bind(subscriber)
    .fetch_all(pool)
    .await
    .expect("the deliveries must be readable");
    let names: Vec<&str> = deliveries
        .iter()
        .map(|(_, name, _)| name.as_str())
        .collect();
    assert!(
        names.contains(&"health.service.degraded") && names.contains(&"health.service.recovered"),
        "the subscribed endpoint must be queued both facts, got {names:?}"
    );
    for (_, name, payload) in &deliveries {
        if name == "health.service.degraded" {
            assert_eq!(payload["service"], serde_json::json!("redis"));
            assert_eq!(payload["from_state"], serde_json::json!("healthy"));
            assert_eq!(payload["to_state"], serde_json::json!("down"));
            assert!(
                payload["incident_id"].is_uuid(),
                "the catalogue calls incident_id required: {}",
                payload
            );
        }
        if name == "health.service.recovered" {
            assert!(payload["duration_seconds"].is_i64(), "{}", payload);
        }
    }

    // The tenant that subscribed to something else must have received **nothing**: a fan-out that
    // ignored the subscription would pass every assertion above.
    let leaked: i64 = sqlx::query_scalar(
        "select count(*)::bigint from events e join webhook_deliveries d on d.event_id = e.id \
         where e.organization_id = $1",
    )
    .bind(other)
    .fetch_one(pool)
    .await
    .expect("the leak count must be readable");
    assert_eq!(
        leaked, 0,
        "a health fact must not reach an endpoint that did not subscribe"
    );

    // And with no endpoint at all, nothing is recorded — the "no receiver, no row" promise, which
    // is what keeps a fresh installation's `events` table from filling with dead weight.
    sqlx::query("delete from webhook_endpoints")
        .execute(pool)
        .await
        .expect("the endpoints must be removable");
    let before: i64 = sqlx::query_scalar("select count(*)::bigint from events")
        .fetch_one(pool)
        .await
        .expect("the count must be readable");
    server.stop();
    let (_, second) = omnion_health::run_and_record(pool, &ctx)
        .await
        .expect("the run works");
    omnion_api::health_events::announce_changes(pool, &second).await;
    let after: i64 = sqlx::query_scalar("select count(*)::bigint from events")
        .fetch_one(pool)
        .await
        .expect("the count must be readable");
    assert_eq!(
        after, before,
        "with no subscriber there is nothing to deliver to, so nothing is recorded"
    );

    server.stop();
    drop(server);
    drop(storage);
    harness.dispose().await;
}

/// A uuid field, as a value check. `serde_json::Value::is_uuid` does not exist, and comparing to
/// a parsed uuid would accept a string that is not one.
trait IsUuid {
    fn is_uuid(&self) -> bool;
}

impl IsUuid for serde_json::Value {
    fn is_uuid(&self) -> bool {
        self.as_str()
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .is_some()
    }
}

async fn create_organization(pool: &PgPool, label: &str) -> Uuid {
    let slug = format!("health-fanout-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Health Fanout {label}"))
        .bind(&slug)
        .fetch_one(pool)
        .await
        .expect("the test organization must be created")
}

async fn create_endpoint(
    pool: &PgPool,
    organization_id: Uuid,
    label: &str,
    events: &[&str],
) -> Uuid {
    let bound: Vec<String> = events.iter().map(|name| (*name).to_string()).collect();
    sqlx::query_scalar(
        "insert into webhook_endpoints (organization_id, name, url, secret, events) \
         values ($1, $2, $3, $4, $5) returning id",
    )
    .bind(organization_id)
    .bind(label)
    .bind("https://example.invalid/hook")
    .bind("a-secret-long-enough-for-the-constraint")
    .bind(&bound)
    .fetch_one(pool)
    .await
    .expect("the test endpoint must be created")
}
