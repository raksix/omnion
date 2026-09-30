//! Omnion API binary entrypoint.
//!
//! Boot order (docs/02-ARCHITECTURE.md, "Reliability"): typed config → telemetry →
//! database + migrations → first-administrator bootstrap → Redis → HTTP server with
//! graceful shutdown.

use std::net::SocketAddr;
use std::process::ExitCode;

use omnion_api::retention_runner;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_api::{
    ai_agent_runner, ai_health_runner, ai_log_runner, analytics_runner, automation_runner,
    backup_schedule_runner, backup_sweep_runner, restore_job_runner, event_retention_runner,
    event_runner, notification_runner, search_runner, workflow_runner,
};
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient, telemetry};
use omnion_identity::users::{self, BootstrapOutcome};
use omnion_storage::Storage;
use tokio::net::TcpListener;

/// Service identifier used in logs and health payloads.
const SERVICE: &str = "omnion-api";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("omnion-api: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = Config::from_env()?;
    let telemetry = telemetry::init(&config.log)?;
    let build = BuildInfo::new(SERVICE, env!("CARGO_PKG_VERSION"));

    tracing::info!(
        service = build.service,
        version = build.version,
        environment = config.env.as_str(),
        "omnion-api starting"
    );

    let db = Db::connect(&config.database).await?;
    db.migrate().await?;
    tracing::info!("database ready and migrations applied");

    bootstrap_admin(&config, &db).await?;
    seed_iam(&db).await?;
    seed_ai_tools(&db).await?;

    let redis = RedisClient::new(&config.redis.url)?;
    if let Err(err) = redis.ping().await {
        // Redis is not needed to serve traffic: the process boots and `/readyz` keeps
        // reporting the dependency as unavailable until it recovers.
        tracing::warn!(error = %err, "redis is not reachable yet");
    }

    // The object store behind the media library (crates/storage). Like Redis it is opened, not
    // required: a store that is down leaves media uploads answering `503` while the rest of the
    // platform keeps working, and the next boot or upload brings it back.
    let storage = Storage::from_env()?;
    tracing::info!(store = %storage.describe(), "object store configured");
    if let Err(err) = storage.ensure_ready().await {
        tracing::warn!(
            error = %err,
            store = %storage.describe(),
            "the object store is not ready yet"
        );
    }

    let address = config.http.bind_address()?;
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "listening");

    let state = AppState::new(build, config, db, redis, storage);

    // The workflow engine ticks in this process (docs/BUILD-BACKLOG.md P09). It is opened,
    // not awaited: every tick is a query against the durable store, so a tick that cannot
    // reach the database logs a warning and the next one picks the work up again.
    if state.config().workflows.runner_enabled {
        let _runner = workflow_runner::spawn(state.clone());
    } else {
        tracing::info!("the workflow runner is disabled (OMNION_WORKFLOW_RUNNER=false)");
    }

    // The webhook delivery runner ticks in this process too (docs/BUILD-BACKLOG.md P12). Same
    // shape as the workflow engine: the queue is durable, so a tick that cannot reach the
    // database is logged and the next one picks the deliveries up.
    if state.config().events.runner_enabled {
        if event_runner::spawn(state.clone()).is_none() {
            tracing::warn!("the webhook delivery runner is not running");
        }
    } else {
        tracing::info!("the webhook delivery runner is disabled (OMNION_EVENTS_RUNNER=false)");
    }

    // The notification delivery runner ticks here for the same reason and under the same flag
    // (REQ-021, slice 4): `notification_deliveries` is a durable queue with a claim lease, so a
    // tick that cannot reach the database is logged and the next one picks the work up. It
    // shares the events cadence rather than growing a second set of knobs, because the two
    // queues are drained by the same kind of work at the same kind of rate — and a second
    // `*_POLL_MS` variable would be one more thing an operator has to match between a
    // web node and a dedicated worker.
    if state.config().events.runner_enabled {
        if notification_runner::spawn(state.clone()).is_none() {
            tracing::warn!("the notification delivery runner is not running");
        }
    } else {
        tracing::info!(
            "the notification delivery runner is disabled (OMNION_EVENTS_RUNNER=false)"
        );
    }

    // The event-retention sweeper ticks in this process too (REQ-016, slice 3), under its own
    // flag: it deletes history rather than sending it, so an installation that drains the
    // queue from a dedicated worker and not at all from the web nodes still wants retention
    // where it is — and vice versa. The window is each organization's own, read inside the
    // delete, so one tenant's compliance policy never shortens another's history.
    if state.config().events.retention_enabled {
        let _sweeper = event_retention_runner::spawn(state.clone());
    } else {
        tracing::info!(
            "the event retention sweeper is disabled (OMNION_EVENT_RETENTION_RUNNER=false)"
        );
    }

    // The automation matcher reads the bus in this process (docs/BUILD-BACKLOG.md P13): each
    // tick evaluates the events after its durable cursor and starts one run per matching rule —
    // a run the workflow engine above then advances.
    if state.config().automation.runner_enabled {
        let _matcher = automation_runner::spawn(state.clone());
    } else {
        tracing::info!("the automation matcher is disabled (OMNION_AUTOMATION_RUNNER=false)");
    }

    // The search indexer applies the bus to the search index in this process (REQ-002): each
    // tick reads the events above its cursor and updates the documents they touch, so a page
    // published here answers a search a moment later.
    if state.config().search.runner_enabled {
        let _indexer = search_runner::spawn(state.clone());
    } else {
        tracing::info!("the search indexer is disabled (OMNION_SEARCH_RUNNER=false)");
    }

    // The analytics rollup worker rebuilds the recent hourly and daily buckets in this process
    // (REQ-007): each tick recomputes from the raw rows, which is idempotent, so a tick that
    // cannot reach the database is logged and the next one writes the same buckets.
    // The retention worker prunes superseded versions and the trash of every site that has a
    // library (REQ-010, slice 4), and repairs the reference rows whose referent is gone. A
    // tick that finds nothing writes no row and logs at debug — a worker that warns on every
    // empty tick is a worker whose real warnings stop being read.
    if state.config().retention.runner_enabled {
        let _retention = retention_runner::spawn(state.clone());
    } else {
        tracing::info!("the retention worker is disabled (OMNION_RETENTION_RUNNER=false)");
    }

    // The health runner publishes this process's heartbeat and runs the scheduled probes
    // (REQ-014, slice 4). Both halves of it were missing before: `worker_heartbeats` had a
    // reader and no writer, so the `n/m` worker card could only ever say "no worker has
    // registered a heartbeat", and `run_and_record` was reached from the four route handlers
    // and nowhere else, so samples existed only while somebody was looking at the panel.
    let _health = omnion_api::health_runner::spawn(state.clone());

    // The backup retention sweep removes expired runs from the destination, artifacts first
    // (REQ-013, slice 3). It is gated by its own flag rather than by `OMNION_RETENTION_RUNNER`
    // because the two sweep different things: an installation that keeps every backup for
    // ever must be able to keep its media sweeper. `prune_candidates` shipped in slice 1 and
    // had no caller at all, so this is the tick that gives it one.
    // The schedule worker takes the backups a `backup_schedules` row asked for. The table,
    // the `next_due_schedules` query and the `cadence` sentence in the API all shipped in
    // slice 1 and slice 2a; nothing wrote `next_run_at` and nothing called that query, so a
    // schedule could be created, listed and rendered with an empty next-run cell for ever.
    // This is the tick that gives both a writer and a reader.
    let _backup_schedules = backup_schedule_runner::spawn(state.clone());
    // The queued-restore worker. Deliberately ungated: a restore queued by an operator is a
    // person watching a screen, and a feature that only runs when a flag is set is a restore
    // that silently never happens on the installation that forgot to set it. It is cheap —
    // one indexed query per poll, and a tick that finds nothing costs nothing.
    let _restore_jobs = restore_job_runner::spawn(state.clone());

    if state.config().retention.backup_sweep_enabled {
        let _backup_sweep = backup_sweep_runner::spawn(state.clone());
    } else {
        tracing::info!(
            "the backup retention sweep is disabled (OMNION_BACKUP_SWEEP=false) — expired runs \
             and their artifacts stay on the destination"
        );
    }

    if state.config().analytics.runner_enabled {
        let _rollups = analytics_runner::spawn(state.clone());
    } else {
        tracing::info!("the analytics rollup worker is disabled (OMNION_ANALYTICS_RUNNER=false)");
    }

    // The AI health probe runner samples every enabled provider on its own cadence (REQ-097,
    // slice 3). It calls the same `probe_now` the "Probe now" button calls, so the background
    // sample and the manual one are the same measurement rather than two implementations of it.
    if state.config().ai_hub.runner_enabled {
        let _probes = ai_health_runner::spawn(state.clone());
    } else {
        tracing::info!("the AI health probe runner is disabled (OMNION_AI_HEALTH_RUNNER=false)");
    }

    // The agent runner (REQ-099, slice 1) claims queued agent runs and executes the loop. It has
    // its **own** switch, not a reading of the health probe's, because it is the one background
    // task that spends money: an installation that wants its providers probed but refuses to let
    // an agent act on its own says `OMNION_AI_RUNNER=false`, and the run-start endpoint then
    // answers `503 runner_disabled` rather than queueing work nothing will pick up.
    if state.config().ai_hub.agent_runner_enabled {
        let _agents = ai_agent_runner::spawn(state.clone());
    } else {
        tracing::info!("the agent runner is disabled (OMNION_AI_RUNNER=false)");
    }

    // The route decision pruner (REQ-098, slice 3) drops decisions past the 90-day window once
    // a day. It is behind its own switch (`OMNION_AI_LOG_RUNNER`) because it has nothing to do
    // with the health probe: one dials providers, the other issues a bulk delete, and an
    // installation that disables one almost never wants to disable the other.
    if state.config().ai_hub.log_runner_enabled {
        let _pruner = ai_log_runner::spawn(state.clone());
    } else {
        tracing::info!("the AI decision pruner is disabled (OMNION_AI_LOG_RUNNER=false)");
    }

    // The rate-limit document is read here, once, and handed to the layer the router is about to
    // install (REQ-012, slice 3). Reading it per request would make every request's cost depend on
    // the database, which is how a settings screen turns into an outage; reading it here and
    // failing open on the shipped defaults means a platform whose database is briefly unreachable
    // still limits, instead of answering every caller in the world.
    let limiter = omnion_api::rate_limit_middleware::RateLimiter::from_store(&state).await;
    let _ = omnion_api::rate_limit_middleware::install(limiter);

    // The runner spawns each got a clone; this one is kept so the shutdown path can still
    // reach `state` after `router(state)` has taken the original by value.
    let state_for_runners = state.clone();
    let app = routes::router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Mark this worker's heartbeat stopped on the way out, so a clean stop is distinguishable
    // from a crash. Without it the panel says "stale" for a worker that was deliberately
    // restarted, and those are the two answers an operator needs apart.
    omnion_api::health_runner::stopped(&state_for_runners).await;

    tracing::info!("shutdown complete");
    telemetry.shutdown();
    Ok(())
}

/// Seed the first administrator on a fresh database.
///
/// Driven by `OMNION_ADMIN_EMAIL` / `OMNION_ADMIN_PASSWORD` (docs/07-IAM.md): the account is
/// created only when the `users` table is still empty, and the password is hashed here at
/// boot — never stored or logged in plain text.
async fn bootstrap_admin(config: &Config, db: &Db) -> Result<(), omnion_identity::IdentityError> {
    match &config.admin {
        Some(admin) => {
            match users::bootstrap_first_admin(db.pool(), &admin.email, &admin.password).await? {
                BootstrapOutcome::Created { user_id, email } => {
                    tracing::info!(%user_id, %email, "first administrator account created");
                }
                BootstrapOutcome::SkippedExistingUsers => {
                    tracing::info!(
                        email = %admin.email,
                        "administrator bootstrap skipped: accounts already exist"
                    );
                }
            }
        }
        None => {
            if !users::has_any(db.pool()).await? {
                tracing::warn!(
                    "no accounts exist yet — set OMNION_ADMIN_EMAIL and OMNION_ADMIN_PASSWORD \
                     to seed the first administrator"
                );
            }
        }
    }
    Ok(())
}

/// Bring the IAM tables in line with the code and guarantee the Owner invariant.
///
/// Afterwards the permission catalogue and the six base roles exist, and when no live Owner
/// binding is left the earliest active account receives one — an installation that bootstrapped
/// before roles existed would otherwise be locked out of its own IAM surface
/// (docs/07-IAM.md §20). The binding is a platform action, so it is audited as one.
async fn seed_iam(db: &Db) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let report = omnion_permissions::seed::ensure(db.pool()).await?;
    tracing::info!(
        permissions = report.permissions,
        roles_created = report.roles_created,
        owner_synced = report.owner_synced,
        "permission catalogue and base roles ready"
    );

    if let Some(user_id) = report.owner_bound {
        omnion_audit::record(
            db.pool(),
            omnion_audit::NewAuditEntry::system("iam.bootstrap.owner_bound")
                .target("user", user_id.to_string())
                .metadata(serde_json::json!({ "role": "owner", "scope": "global" })),
        )
        .await?;
        tracing::info!(%user_id, "owner role assigned to the earliest active account");
    }

    Ok(())
}

/// Seed the AI tool registry from the compiled catalogue (REQ-100 slice 1).
///
/// **A seeding failure is logged, not fatal.** The registry is a screen an operator configures;
/// refusing to boot the whole platform because a row could not be written would mean one failed
/// upsert takes down publishing, media and the public renderer with it. The registry screen
/// answers `seeded: false` and shows its banner, which is the diagnosis the spec asks for, and the
/// next boot retries.
///
/// The log line carries `decisions_preserved` deliberately: it is the number that makes "seeding
/// does not overwrite operator edits" visible in production rather than only in a test, because a
/// pass that refreshed 23 rows and preserved 23 decisions is the expected line, and one that
/// reports 0 preserved is the one somebody needs to look at.
async fn seed_ai_tools(db: &Db) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match omnion_ai_hub::registry::seed(db.pool()).await {
        Ok(outcome) => {
            tracing::info!(
                tools_inserted = outcome.inserted,
                tools_refreshed = outcome.refreshed,
                tools_retired = outcome.retired,
                decisions_preserved = outcome.decisions_preserved,
                "AI tool registry seeded from the compiled catalogue"
            );
        }
        Err(err) => {
            tracing::error!(
                error = %err,
                "the AI tool registry could not be seeded; /ai/tools will show its banner"
            );
        }
    }
    Ok(())
}

/// Resolve when the process is asked to stop: Ctrl-C, or SIGTERM from an orchestrator.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).ok();
        let wait_for_terminate = async {
            match terminate.as_mut() {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = wait_for_terminate => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received");
}
