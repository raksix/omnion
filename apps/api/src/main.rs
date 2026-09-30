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
    analytics_runner, automation_runner, backup_schedule_runner, backup_sweep_runner,
    crm_autoresponder_runner, crm_sla_runner, event_retention_runner, event_runner,
    notification_runner, project_limit_runner, restore_job_runner, search_runner,
    workflow_runner,
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

    // The autoresponder worker (REQ-117, slice 3) sends the acknowledgements whose configured
    // send delay has elapsed. A source that sets a delay relies on this worker: the slot was
    // reserved at capture, and without the sweep the reservation is a promise with no clock
    // behind it — the reply simply never goes out, on exactly the sources that asked for it
    // to wait.
    if state.config().crm_autoresponder.runner_enabled {
        let _autoresponder = crm_autoresponder_runner::spawn(state.clone());
    } else {
        tracing::info!(
            "the crm autoresponder worker is disabled (OMNION_CRM_AUTORESPONDER_RUNNER=false)"
        );
    }

    // The SLA worker (REQ-117, slice 3) escalates the leads whose first-response deadline has
    // passed and reminds the owners of the ones about to. Until this existed, the store read
    // `due_breaches` and nothing called it: the panel computed a `breached` state that no
    // timer ever acted on, so a policy with a 60-minute target was a number in a column rather
    // than an escalation. The reminder is the half that had *no* reader at all — the column
    // existed and the editor rendered it, which is exactly what makes a missing worker hard to
    // notice: every screen said the reminder was configured and nothing said it was not running.
    if state.config().crm_sla.runner_enabled {
        let _sla = crm_sla_runner::spawn(state.clone());
    } else {
        tracing::info!("the crm sla worker is disabled (OMNION_CRM_SLA_RUNNER=false)");
    }

    // The project limit notice worker (REQ-133, slice 4) emits
    // `automation.project.limit.warning` and `.limit_exceeded` exactly once per limit per period.
    // Everything about the limits already worked -- the screen drew the amber bar and the engine
    // refused the over-quota run by name -- but the REQ's two events were emitted by nothing, and
    // the warning a client saw was computed per read, so reloading re-warned for ever. The
    // once-ness is a claim row rather than a property of this process, so a second API node
    // sweeping the same database cannot double-notify.
    if state.config().project_limit.runner_enabled {
        let _project_limit = project_limit_runner::spawn(state.clone());
    } else {
        tracing::info!("the project limit worker is disabled (OMNION_PROJECT_LIMIT_RUNNER=false)");
    }

    // The rate-limit document is read here, once, and handed to the layer the router is about to
    // install (REQ-012, slice 3). Reading it per request would make every request's cost depend on
    // the database, which is how a settings screen turns into an outage; reading it here and
    // failing open on the shipped defaults means a platform whose database is briefly unreachable
    // still limits, instead of answering every caller in the world.
    let limiter = omnion_api::rate_limit_middleware::RateLimiter::from_store(&state).await;
    let _ = omnion_api::rate_limit_middleware::install(limiter);

    let app = routes::router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

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
