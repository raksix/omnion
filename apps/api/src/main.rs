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
    restore_job_runner,
    event_retention_runner, event_runner, notification_runner, search_runner, secrets_runner,
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

    // The metric registry's durable catalogue (REQ-126, slice 2). Seeded here, right after the
    // migrations, because the panel's metric screen reads the table and not the registry: the
    // table is the one place a module can add its own families without the core knowing their
    // names in advance. A failure is logged and the boot continues — an instance with no
    // catalogue still serves `/metrics`, and a metrics screen that is temporarily empty is a much
    // smaller problem than an API that will not start.
    if let Err(error) = seed_metric_catalog(&db).await {
        tracing::warn!(error = %error, "the metric catalogue could not be seeded");
    }

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

    // The secret re-wrap walk (REQ-125): each tick re-seals one small batch of a live
    // rotation's versions, resuming from the job's cursor after a restart.
    if state.config().secrets.runner_enabled {
        let _rewrap = secrets_runner::spawn(state.clone());
    } else {
        tracing::info!("the secret re-wrap runner is disabled (OMNION_SECRETS_RUNNER=false)");
    }

    // The exporter flush loop (REQ-126, slice 3). It drains each configured exporter's bounded
    // buffer on that row's `batch_ms` and folds the drop counter and the health chip back into
    // the row. Spawned, never awaited: a backend that is unreachable must not be able to keep
    // the API from serving traffic, and every failure inside a sweep is logged and the next
    // sweep runs anyway.
    if state.config().telemetry.exporter_flush_enabled {
        // The pool is cloned from the STATE, not from `db`: `db` moved into `AppState` at the
        // line above, and a loop that held the whole handle would keep a second reference to
        // configuration the server no longer owns.
        let _flush = omnion_telemetry::exporter_flush::run(state.db().pool().clone());
    } else {
        tracing::info!("the exporter flush loop is disabled (OMNION_EXPORTER_FLUSH=false)");
    }

    // The alert evaluator (REQ-126, slice 4). It moves rules through pending → firing → resolved
    // and claims each newly-firing event exactly once, so "notifies once" is a property of the
    // UPDATE rather than of the caller. The same rules ship as `infra/observability/alerts.yml`,
    // so an operator who runs them in Prometheus turns this loop off rather than receiving two
    // notifications per incident.
    if state.config().telemetry.alerts_evaluator_enabled {
        if let Err(error) =
            omnion_telemetry::alert_loop::seed_bundled_rules(state.db().pool()).await
        {
            // A failed seed is logged and the process continues: an instance with no bundled rules
            // still evaluates the custom ones, and an API that refuses to start because a
            // monitoring table was briefly unreachable is strictly worse.
            tracing::warn!(error = %error, "the bundled alert rules were not seeded");
        }
        let _evaluator = omnion_telemetry::alert_loop::run(state.db().pool().clone());
    } else {
        tracing::info!("the alert evaluator is disabled (OMNION_ALERTS_EVALUATOR=false)");
    }

    // The retention sweep (REQ-126, slice 4). Without it the three prune functions are reachable
    // only from their own tests: an instance left up for a year keeps a year of log lines and
    // trace rows while the settings screen shows a window nothing honours. This is the caller that
    // makes "retention prunes log rows and trace-index rows past the window" a property of the
    // process rather than a property of a test suite.
    if state.config().telemetry.retention_sweep_enabled {
        let _retention = omnion_telemetry::retention::run(state.db().pool().clone());
    } else {
        tracing::info!("the retention sweep is disabled (OMNION_RETENTION_SWEEP=false)");
    }

    // The rate-limit document is read here, once, and handed to the layer the router is about to
    // install (REQ-012, slice 3). Reading it per request would make every request's cost depend on
    // the database, which is how a settings screen turns into an outage; reading it here and
    // failing open on the shipped defaults means a platform whose database is briefly unreachable
    // still limits, instead of answering every caller in the world.
    let limiter = omnion_api::rate_limit_middleware::RateLimiter::from_store(&state).await;
    let _ = omnion_api::rate_limit_middleware::install(limiter);

    // The platform-wide budgets (REQ-127 slice 1), read here for the same reason and with the
    // opposite fallback: `RateLimiter::from_store` falls back to its SHIPPED DEFAULTS because a
    // gateway with no document should still limit, while this one falls back to an EMPTY set
    // because the platform budgets are an operator-written document and inventing numbers for it
    // would be a second policy nobody chose. The gateway limiter above is enforcing throughout.
    //
    // The fail mode is the documented per-deployment choice the request demands: `RELIABILITY_FAIL_MODE`
    // is `open` unless the operator says otherwise, and the panel reads it from the installed
    // layer rather than from this environment variable — a behaviour that only exists in a config
    // file is a behaviour nobody can see.
    let fail_mode = match std::env::var("RELIABILITY_FAIL_MODE").as_deref() {
        Ok("closed") | Ok("CLOSE") | Ok("close") => {
            omnion_reliability::limiter_redis::FailMode::Closed
        }
        _ => omnion_reliability::limiter_redis::FailMode::Open,
    };
    let platform = omnion_api::reliability_middleware::PlatformLimiter::from_store(&state, fail_mode).await;
    let _ = omnion_api::reliability_middleware::install(platform);

    let app = routes::router(state.clone());
    // The graceful shutdown sequence (REQ-126, slice 4). The order is the contract and it lives
    // in `omnion_telemetry::lifecycle`:
    //
    //   1. SIGTERM arrives → `lifecycle::begin_drain` flips the flag, so `/readyz` answers `503`
    //      from this instant and a load balancer stops sending. `/healthz` stays `200`.
    //   2. axum's own graceful shutdown stops the listener and waits for in-flight connections;
    //      our guard in `request_log` is what the drain counts down.
    //   3. `drain_and_flush` waits for that count to reach zero, to a deadline, then does ONE
    //      final telemetry sweep so the lines the requests just wrote are not lost to a process
    //      that exits before the next interval tick.
    //   4. One summary line, and the pools close as the runtime unwinds.
    //
    // Steps 1 and 2 race by design: axum stops the listener the moment the future resolves, and
    // the flag has to be set BEFORE that, not after, or a balancer polling in the gap keeps
    // sending into a socket that is about to close.
    let lifecycle = omnion_telemetry::lifecycle::global();
    // The `Arc` is cloned into the shutdown future and kept here: both need it, and cloning an
    // `Arc` for a future that runs once at the end of the process is far cheaper than the
    // borrow-checker gymnastics of sharing one.
    let on_signal = std::sync::Arc::clone(&lifecycle);
    let shutdown = async move {
        shutdown_signal().await;
        on_signal.begin_drain();
    };

    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown);
    let server_result = server.await;

    // The drain, the flush and the summary. `state.db().pool()` is passed so the final sweep
    // can fold the drop counter and the health chip back into their rows — the same sweep the
    // loop runs, not a second one that could drift from it.
    let summary = omnion_telemetry::lifecycle::drain_and_flush(
        &lifecycle,
        Some(state.db().pool()),
        std::time::Duration::from_millis(
            // From the config, not the constant: the drain has to fit inside the deployment's
            // termination grace period, and only the operator knows what that is set to.
            state.config().telemetry.drain_timeout_ms,
        ),
    )
    .await;

    server_result?;

    tracing::info!(summary = %summary.to_line(), "shutdown sequence finished");
    telemetry.shutdown();
    Ok(())
}

/// Seed the first administrator on a fresh database.
///
/// Driven by `OMNION_ADMIN_EMAIL` / `OMNION_ADMIN_PASSWORD` (docs/07-IAM.md): the account is
/// created only when the `users` table is still empty, and the password is hashed here at
/// boot — never stored or logged in plain text.
/// Seed `obs_metric_catalog` from the registry, and publish the build info family.
///
/// `omnion_build_info` is the one family the process has to record itself: every other family
/// is recorded by the layer that does the work, and this one exists so a dashboard can attribute
/// a spike to the release it arrived in. The commit label is `env!("OMNION_COMMIT")` when the
/// build set it and `unknown` when it did not — a made-up sha is worse than an honest `unknown`,
/// because a wrong one points an incident review at a release that did not ship.
async fn seed_metric_catalog(db: &Db) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = db.pool();
    let declarations: Vec<omnion_telemetry::metric_catalog::FamilyDeclaration> =
        omnion_telemetry::metrics::FAMILIES
            .iter()
            .map(|spec| omnion_telemetry::metric_catalog::FamilyDeclaration::from_spec(spec, 0))
            .collect();
    omnion_telemetry::metric_catalog::sync_from_registry(pool, &declarations).await?;

    let version = env!("CARGO_PKG_VERSION").to_owned();
    let commit = option_env!("OMNION_COMMIT").unwrap_or("unknown").to_owned();
    let registry = omnion_telemetry::metrics::global();
    registry.gauge_set(
        "omnion_build_info",
        &[version.as_str(), commit.as_str()],
        1.0,
    );

    // The families that were recorded before the first boot finished are marked as seen, so the
    // screen's "never recorded" column stays meaningful instead of being zero for everything the
    // process did during its own startup.
    let seen: Vec<&str> = omnion_telemetry::metrics::FAMILIES
        .iter()
        .map(|spec| spec.name)
        .collect();
    omnion_telemetry::metric_catalog::mark_seen(pool, &seen).await?;
    Ok(())
}

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

    // Re-assert the invariant AFTER the bootstrap, never only before it.
    //
    // `bootstrap_admin` runs one line above and can be the thing that creates the very first
    // account, on a database whose `role_bindings` table was empty a moment earlier. The `ensure`
    // above ran before it, so the Owner invariant was evaluated against a database with no
    // accounts, bound nobody, and the first account was left with no role at all: the panel
    // rendered, the sign-in succeeded, and every permission-guarded route answered
    // `403 permission_denied … "observability.read"`. The onboarding screen did not help —
    // `status()` derives `steps.owner` from `has_users`, not from a binding, so the wizard
    // reported the account as the owner while holding no role.
    //
    // It is also the wrong place to stop at boot. `seed_iam` runs once, and a fresh install
    // reaches its first account through the WIZARD, long after this function returned — so on the
    // ordinary path the second call is the only one that ever sees the account. Calling it here
    // makes the guarantee hold for both: the bootstrap path (bound by this call) and the wizard
    // path (bound by the wizard's own `create_owner`, which already asserts it). A platform
    // whose first account holds no role can sign in and see nothing, which is the one failure
    // shape an installation cannot recover from by itself.
    //
    // `ensure` is idempotent and cheap when the invariant already holds, so the common case costs
    // one `exists` query and writes nothing.
    if let Some(user_id) = omnion_permissions::seed::ensure_owner_binding(db.pool()).await? {
        tracing::info!(
            %user_id,
            "owner role bound to the first account after the bootstrap — without it every \
             permission-guarded route would answer 403"
        );
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
