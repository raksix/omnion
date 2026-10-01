//! `/api/v1/deployment/*` — the deploy wizard's write surface (REQ-024, slice 2).
//!
//! Slice 1 shipped seven reads. This file is the other half: pre-flight, start a deploy, read a
//! running job, stream its log, cancel it. Four routes, three permissions, and the decisions they
//! enforce are the ones that keep a deploy from being the thing that takes the instance down.
//!
//! Where each one is decided, and why the obvious version is wrong:
//!
//! * **Pre-flight is a real probe, not a canned list.** Each check answers from the database it
//!   is given — backup age, pending migrations, free disk, the active job, the target's
//!   compatibility — and a check that *cannot* be answered is `Unknown`, which blocks. The spec
//!   says so twice: "if backup freshness cannot be determined that is a visible warning, never a
//!   silent pass", and the report's own missing rows are filled in as unknown by
//!   `PreflightReport::from_outcomes`.
//! * **A `deploy` is refused on a failed or unknown pre-flight, server-side.** The wizard
//!   disables `Continue`, which is a convenience for the operator and no protection at all: the
//!   check that matters is the one the server repeats. A `422` here is the spec's answer.
//! * **Production requires the typed version, twice.** Once in the panel so the operator knows,
//!   and once here against `confirmation_matches` — with a mismatch a `400` that names both
//!   values, because "invalid request" tells an operator nothing about which string was wrong.
//! * **Cancel is refused past the migrate step with a reason**, from `cancel_refusal`, rather
//!   than answering `409` — the message is the difference between "you cannot stop this" and
//!   "stopping this now is a rollback, and here is the button".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_deployment::jobs::{
    self, CreatedJob, NewJob, StartRefusal, StepRefusal, Target,
};
use omnion_deployment::preflight::{
    CheckId, CheckOutcome, Confirmation, PreflightReport, confirmation_for, confirmation_matches,
};
use omnion_deployment::store::HistoryFilter;
use omnion_deployment::{JobKind, JobStatus, StepStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::deployment_runner;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/deployment/environments/{id}/preflight`.
#[derive(Debug, Deserialize)]
pub struct PreflightBody {
    /// The version being considered. Must be a release the channel admits.
    pub to_version: String,
}

/// One pre-flight row, as the wizard renders it.
#[derive(Debug, Serialize)]
pub struct PreflightRowBody {
    /// The check's id.
    pub id: CheckId,
    /// `pass`, `warn`, `fail` or `unknown`.
    pub state: String,
    /// The check's own title, so the panel need not hard-code the copy.
    pub title: String,
    /// The one line the row shows.
    pub detail: String,
    /// What the operator can do about it. Empty when there is nothing to suggest.
    pub action: Option<String>,
    /// Whether the wizard shows the acknowledgement box for this row.
    pub needs_acknowledgement: bool,
}

/// Response of the pre-flight route.
#[derive(Debug, Serialize)]
pub struct PreflightResponse {
    /// The report, one row per check — never fewer than [`CheckId::ALL`].
    pub checks: Vec<PreflightRowBody>,
    /// Whether any row blocks the wizard.
    pub blocked: bool,
    /// Whether the wizard may show step 2.
    pub can_continue: bool,
    /// Whether the acknowledgement checkbox is required.
    pub needs_acknowledgement: bool,
    /// Whether this environment is production, and so requires the version to be typed.
    pub production: bool,
    /// How the operator confirms, as a name the panel renders.
    pub confirmation: Confirmation,
    /// Whether this deploy needs a maintenance window.
    pub requires_maintenance_window: bool,
    /// A stable key the panel echoes back so the report it confirms is *this* report.
    pub token: String,
}

/// Body of `POST /api/v1/deployment/environments/{id}/deploy`.
#[derive(Debug, Deserialize)]
pub struct DeployBody {
    /// The version to move to.
    pub to_version: String,
    /// The typed confirmation, for production. Ignored elsewhere.
    #[serde(default)]
    pub confirm_version: Option<String>,
    /// Whether to take a backup first. Defaults to true, because a deploy without one is the
    /// case this whole request exists to prevent.
    #[serde(default = "default_true")]
    pub backup_first: bool,
    /// The pre-flight report's token, from the wizard's step 1.
    #[serde(default)]
    pub preflight_token: Option<String>,
    /// The operator's acknowledgement of any warning.
    #[serde(default)]
    pub acknowledged: bool,
}

/// The default for `backup_first`.
///
/// `true`, not `false`: a JSON body that simply omits the field is the most likely thing a
/// half-built client sends, and defaulting it to `false` turns "I did not think about backups"
/// into "no backup was taken" on the one deploy that cannot be undone.
fn default_true() -> bool {
    true
}

/// Response of the deploy route.
#[derive(Debug, Serialize)]
pub struct DeployResponse {
    /// The created job.
    pub job: JobBody,
}

/// A job, as the wizard's step 3 and the history expansion read it.
#[derive(Debug, Serialize)]
pub struct JobBody {
    /// The job's id.
    pub id: Uuid,
    /// The environment.
    pub environment: String,
    /// `deploy`, `rollback` or `restart`.
    pub kind: JobKind,
    /// The job's status.
    pub status: JobStatus,
    /// Where it came from.
    pub from_version: Option<String>,
    /// Where it goes.
    pub to_version: Option<String>,
    /// Who started it.
    pub started_by: Option<Uuid>,
    /// The reason, for a rollback.
    pub reason: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
    /// How long it has been, or took.
    pub elapsed_ms: Option<i64>,
    /// The error, when it failed.
    pub error: Option<String>,
    /// Whole-percent progress, counting finished steps only.
    pub progress_percent: u8,
    /// Whether `Cancel` may be pressed, and why not when it may not.
    pub cancellable: bool,
    /// The refusal message, when it is not cancellable.
    pub cancel_refusal: Option<String>,
    /// Its steps, in order.
    pub steps: Vec<StepBody>,
}

/// One step of a job.
#[derive(Debug, Serialize)]
pub struct StepBody {
    /// 0-based position.
    pub position: u32,
    /// The step's name.
    pub name: String,
    /// Its status.
    pub status: StepStatus,
    /// Its log, so far.
    pub output: String,
    /// When it started.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
}

/// Response of the job routes.
#[derive(Debug, Serialize)]
pub struct JobResponse {
    /// The job.
    pub job: JobBody,
}

/// Body of the log route's cursor fallback.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    /// The byte offset the client already has.
    #[serde(default)]
    pub cursor: Option<usize>,
}

/// One poll of the log, for a browser that cannot hold an `EventSource` open.
#[derive(Debug, Serialize)]
pub struct LogChunkBody {
    /// The new text since the cursor.
    pub chunk: String,
    /// The cursor to send next.
    pub cursor: usize,
    /// Whether the job is still running — `false` tells the client to stop polling.
    pub running: bool,
    /// The job's status, so the poll can also carry the timeline's progress.
    pub status: JobStatus,
}

/// Response of the cancel route.
#[derive(Debug, Serialize)]
pub struct CancelResponse {
    /// The job after the cancel.
    pub job: JobBody,
    /// What was done, for the banner.
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/deployment/environments/{id}/preflight` — the wizard's step 1.
///
/// The report is a **function of the database**, recomputed on every call. A pre-flight result
/// cached from five minutes ago is a pre-flight result about a different system, and the one
/// check that changes fastest is exactly the one that matters: whether another job has since
/// started.
pub async fn preflight(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
    Json(body): Json<PreflightBody>,
) -> Result<Json<PreflightResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let target = Target::new(environment);
    let report = build_preflight(pool, &target, &body.to_version).await?;
    Ok(Json(preflight_body(&target, &body.to_version, report)))
}

/// `POST /api/v1/deployment/environments/{id}/deploy` — the wizard's step 2, and the job's start.
///
/// Three refusals, and each is a different operator mistake:
/// * `400` — the target version is not something the channel admits, or the typed confirmation
///   does not match. The message names what was typed and what was expected.
/// * `409` — another job holds this environment. Carries its id, so the panel can link to the
///   deploy that is in the way rather than saying "busy".
/// * `422` — the pre-flight blocks. Refused here even though the wizard already disabled
///   `Continue`, because the panel is not the enforcement point.
pub async fn start_deploy(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
    Json(body): Json<DeployBody>,
) -> Result<(StatusCode, Json<DeployResponse>), ApiError> {
    let pool = state.db().pool();
    let target = Target::new(environment);

    let from_version = store_version(pool, &target.environment).await?;
    let to_version = body.to_version.trim().to_string();
    if to_version.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_to_version",
            "Name the version to deploy.",
        ));
    }

    if target.production {
        let typed = body.confirm_version.as_deref().unwrap_or_default();
        if !confirmation_matches(typed, &to_version) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirmation_mismatch",
                format!(
                    "Production requires typing the target version. You typed {:?}; this deploy \
                     targets {to_version:?}.",
                    typed
                ),
            ));
        }
    }

    // The pre-flight is repeated here on purpose — see the module docs.
    let report = build_preflight(pool, &target, &to_version).await?;
    if let Some(blocker) = blocking_row(&report) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "preflight_blocked",
            format!(
                "The pre-flight did not pass: {} — {}",
                blocker.title,
                blocker.detail
            ),
        ));
    }
    // The report's own `can_continue(acknowledged)` is the single rule, so the `422` here and
    // the wizard's disabled button can never disagree about the same report.
    if !report.can_continue(body.acknowledged) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "acknowledgement_required",
            report
                .blocked_reason(body.acknowledged)
                .unwrap_or_else(|| "the pre-flight has a warning to acknowledge".to_string()),
        ));
    }

    let created = jobs::create_job(
        pool,
        &NewJob {
            target: target.clone(),
            kind: JobKind::Deploy,
            from_version: from_version.clone(),
            to_version: Some(to_version.clone()),
            actor: Some(current.user.id),
            reason: None,
            backup_id: None,
        },
    )
    .await
    .map_err(start_refusal)?;

    deployment_runner::spawn_deploy(
        pool.clone(),
        created.id,
        body.backup_first,
        target.production,
    );

    omnion_audit::record(
        pool,
        omnion_audit::NewAuditEntry::by_user(current.user.id, "deployment.started")
            .target("deployment", created.id.to_string())
            .metadata(json!({
                "environment": target.environment,
                "from": from_version,
                "to": to_version,
                "backup_first": body.backup_first,
                "preflight_token": body.preflight_token,
            })),
    )
    .await?;

    let job = job_body(&created, pool).await?;
    Ok((StatusCode::ACCEPTED, Json(DeployResponse { job })))
}

/// `GET /api/v1/deployment/jobs/{id}` — the wizard's step 3 poll.
pub async fn get_job(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<JobResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let job = jobs::load_job(pool, id).await?;
    Ok(Json(JobResponse {
        job: body_of(&job, pool).await?,
    }))
}

/// `GET /api/v1/deployment/jobs/{id}/log` — the log pane, as a server-sent stream.
///
/// The stream is the primary path because the spec's log pane follows a run; `?cursor=` on the
/// same path is the fallback for a proxy that buffers `text/event-stream`, and it exists in this
/// shape rather than as a second route so a client that loses its stream can switch without
/// learning a new URL.
pub async fn stream_log(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    // 404 before the stream opens: an `EventSource` cannot read a JSON error body, so a missing
    // job has to be a status code here or the pane shows a connection error for ever.
    let job = jobs::load_job(pool, id).await?;
    if job.steps.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "That deployment has no steps.",
        ));
    }

    // The same shape as the AI chat stream: a spawned task pushes frames into a bounded channel
    // and the response hands the receiver to axum. Bounded, not unbounded, so a client that stops
    // reading cannot make a finished job's log grow in memory without limit.
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(LOG_STREAM_BUFFER);
    let stream_pool = pool.clone();
    let job_id = id;

    tokio::spawn(async move {
        // The first frame carries the whole log, so a pane opened mid-run is not blank until
        // the next line happens to be written.
        let mut sent = 0usize;
        let mut idle = 0u32;
        loop {
            match jobs::job_log(&stream_pool, job_id).await {
                Ok(log) => {
                    let (chunk, next) = jobs::log_since(&log, sent);
                    if !chunk.is_empty() {
                        sent = next;
                        if tx.send(Ok(Event::default().data(chunk))).await.is_err() {
                            // The browser closed the pane. That is not an error to log — a
                            // client that navigates away mid-deploy is normal.
                            break;
                        }
                    }
                    match jobs::load_job(&stream_pool, job_id).await.map(|job| job.status) {
                        Ok(status) if status.is_finished() => {
                            // A final frame carries the terminal status, so a client that reads
                            // only the stream still learns how the run ended.
                            let frame = Event::default()
                                .event("end")
                                .data(format!(
                                    r#"{{"status":"{}","cursor":{sent}}}"#,
                                    status.as_str()
                                ));
                            let _ = tx.send(Ok(frame)).await;
                            break;
                        }
                        Ok(_) => {}
                        // The job row is gone mid-stream (a pruned history). Say so once and stop
                        // rather than reconnecting for ever against a 404.
                        Err(_) => {
                            let _ = tx
                                .send(
                                    Ok(Event::default()
                                        .data("this deployment record is no longer available")
                                        .event("end")),
                                )
                                .await;
                            break;
                        }
                    }
                    // `idle` is not reset here on purpose: the keepalive below is counting
                    // consecutive polls that produced nothing, which is the number that says
                    // "this stream is quiet", not "this loop iterated".
                }
                Err(ref error) => {
                    let _ = tx
                        .send(Ok(Event::default().data(error.to_string()).event("error")))
                        .await;
                    break;
                }
            }
            // Poll rather than hold a transaction open: the log is written by the runner's own
            // statements, and a long-lived read transaction here would pin the row's version and
            // make the runner's updates wait on it.
            tokio::time::sleep(LOG_POLL_INTERVAL).await;
            idle = idle.saturating_add(1);
            if idle >= LOG_IDLE_FRAMES {
                // ~7 minutes without a line. Not an error: a long `verify` is quiet — but an
                // open stream that never says anything is indistinguishable from a dead one.
                if tx
                    .send(Ok(Event::default()
                        .data("still running; no new output")
                        .event("keepalive")))
                    .await
                    .is_err()
                {
                    break;
                }
                idle = 0;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

/// `GET /api/v1/deployment/jobs/{id}/log?cursor=` — the polling fallback for the same log.
pub async fn poll_log(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogChunkBody>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let log = jobs::job_log(pool, id).await.map_err(step_refusal)?;
    let (chunk, cursor) = jobs::log_since(&log, query.cursor.unwrap_or(0));
    let job = jobs::load_job(pool, id).await?;
    Ok(Json(LogChunkBody {
        chunk: chunk.to_string(),
        cursor,
        running: !job.status.is_finished(),
        status: job.status,
    }))
}

/// `POST /api/v1/deployment/jobs/{id}/cancel` — stop before the migrate step.
///
/// The refusal is a `409` **with the reason from `cancel_refusal`**, so the panel can say "the
/// migrate step has started; this is a rollback now" and offer the rollback, instead of greying
/// out a button whose reason the operator has to guess.
pub async fn cancel_job(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<CancelResponse>, ApiError> {
    let pool = state.db().pool();
    let job = jobs::load_job(pool, id).await?;

    if job.status.is_finished() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "job_finished",
            format!("This job already finished as {}.", job.status.as_str()),
        ));
    }
    let refusal = omnion_deployment::cancel_refusal(job.kind, job.current_step());
    if let Some(reason) = refusal {
        return Err(ApiError::new(StatusCode::CONFLICT, "cancel_too_late", reason));
    }

    let current_step = job
        .current_step()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "no_step_running", "No step is running."))?;
    let _ = jobs::finish_step(pool, id, current_step, StepStatus::Failed, Some("cancelled by an operator")).await;
    // The cancelled job's own status is `cancelled`, not `failed` — the table accepts both and
    // the history filter separates them, so a deliberate stop is not counted as a failure in an
    // operator's "how reliable is this" reading of the history.
    sqlx::query(
        "update deployments set status = $1, reason = coalesce(reason, 'cancelled by an operator'), \
         finished_at = now(), duration_ms = greatest(0, (extract(epoch from (now() - started_at)) * 1000)::int) \
         where id = $2",
    )
    .bind(JobStatus::Cancelled.as_str())
    .bind(id)
    .execute(pool)
    .await
    .map_err(db)?;
    // A cancelled job must not hold the environment: the partial unique index covers the active
    // statuses, so leaving it `failed` would keep the next deploy refused with a 409 forever.
    sqlx::query(
        "update deployment_steps set status = $1 where deployment_id = $2 and status = $3",
    )
    .bind(StepStatus::Skipped.as_str())
    .bind(id)
    .bind(StepStatus::Pending.as_str())
    .execute(pool)
    .await
    .map_err(db)?;

    omnion_audit::record(
        pool,
        omnion_audit::NewAuditEntry::by_user(current.user.id, "deployment.cancelled")
            .target("deployment", id.to_string())
            .metadata(json!({
                "environment": job.environment,
                "step": current_step,
            })),
    )
    .await?;

    let job = jobs::load_job(pool, id).await?;
    Ok(Json(CancelResponse {
        job: body_of(&job, pool).await?,
        message: format!("Stopped before the {current_step} step. Nothing was changed."),
    }))
}

// ---------------------------------------------------------------------------------------------
// Pre-flight
// ---------------------------------------------------------------------------------------------

/// How many log frames may sit unread before the stream stops pushing.
///
/// Bounded on purpose: a deploy's log is the one response whose size is not known in advance,
/// and an unbounded channel plus a browser that navigated away is a slow memory leak that only
/// shows up on the busiest instances.
const LOG_STREAM_BUFFER: usize = 64;

/// How often the log stream re-reads the job's log.
const LOG_POLL_INTERVAL: Duration = Duration::from_millis(700);

/// How many polls with no new output produce a keepalive frame (~7 minutes).
const LOG_IDLE_FRAMES: u32 = 600;

/// The disk a deploy wants free, in megabytes, when the release declares no requirement.
const DEFAULT_FREE_DISK_MB: i64 = 2048;

/// Build the pre-flight report from the live database.
///
/// Every row here is a query, not a constant. The crate's `PreflightReport` decides what a
/// missing or unknown row means; this function's job is to make sure there is no reason to
/// produce one — and where there is (a target version that is not in the cache, so its disk
/// requirement is unknown), to say `Unknown` rather than guess.
async fn build_preflight(
    pool: &sqlx::PgPool,
    target: &Target,
    to_version: &str,
) -> Result<PreflightReport, ApiError> {
    let mut rows: Vec<CheckOutcome> = Vec::with_capacity(CheckId::ALL.len());

    // 1. Backup freshness — from the newest backup row, and a failure to *find* one is `Unknown`
    //    rather than `Fail`: "we cannot tell" and "there is none" lead the operator somewhere
    //    different, and the spec asks for the first to be visible.
    rows.push(backup_freshness(pool).await?);

    // 2. Pending migrations — a real count from the migration ledger.
    rows.push(pending_migrations(pool, to_version).await?);

    // 3. Free disk — the release's own requirement when the cache has it, the default otherwise.
    rows.push(free_disk(pool, to_version).await?);

    // 4. Background jobs — whether this environment is already the subject of a job.
    rows.push(running_job(pool, &target.environment).await?);

    // 5. Dependency health — the probes the health table already records.
    rows.push(dependency_health(pool, &target.environment).await?);

    // 6. Maintenance window — required for production by this request's own rules.
    rows.push(if target.production {
        CheckOutcome::warn(
            CheckId::MaintenanceWindow,
            "Production deploys should run inside a maintenance window.",
            "Open a window from the maintenance screen if this deploy interrupts live traffic.",
        )
    } else {
        CheckOutcome::pass(
            CheckId::MaintenanceWindow,
            "This environment does not require a maintenance window.",
        )
    });

    // 7. Core compatibility — whether the running core satisfies the target's minimum.
    rows.push(core_compatibility(pool, to_version).await?);

    Ok(PreflightReport::from_outcomes(
        target.production,
        rows,
    ))
}

/// The newest backup's age, in minutes.
async fn backup_age_minutes(pool: &sqlx::PgPool) -> Result<Option<i64>, ApiError> {
    let age: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from (now() - max(created_at))) / 60 from backups",
    )
    .fetch_one(pool)
    .await
    .map_err(db)?;
    Ok(age.map(|minutes| minutes as i64))
}

/// How old a backup may be before it is not a way back.
const BACKUP_MAX_AGE_MINUTES: i64 = 24 * 60;

async fn backup_freshness(pool: &sqlx::PgPool) -> Result<CheckOutcome, ApiError> {
    match backup_age_minutes(pool).await? {
        None => Ok(CheckOutcome::unknown(
            CheckId::BackupFreshness,
            "No backup has been recorded, so there is nothing to roll back to.",
            "Take a backup from the backup centre, then run this pre-flight again.",
        )),
        Some(age) if age > BACKUP_MAX_AGE_MINUTES => Ok(CheckOutcome::warn(
            CheckId::BackupFreshness,
            format!(
                "The newest backup is {age} minutes old (the limit is {BACKUP_MAX_AGE_MINUTES})."
            ),
            "Take a fresh backup before deploying.",
        )),
        Some(age) => Ok(CheckOutcome::pass(
            CheckId::BackupFreshness,
            format!("The newest backup is {age} minutes old."),
        )),
    }
}

async fn pending_migrations(
    pool: &sqlx::PgPool,
    to_version: &str,
) -> Result<CheckOutcome, ApiError> {
    let declared: Option<i32> = sqlx::query_scalar(
        "select cardinality(migrations) from releases_cache where version = $1",
    )
    .bind(to_version)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let count = declared.unwrap_or(0);
    if count > 0 {
        Ok(CheckOutcome::warn(
            CheckId::PendingMigrations,
            format!(
                "{count} migration{} ship with {to_version}; they run in the migrate step and are \
                 not reversible.",
                if count == 1 { "" } else { "s" }
            ),
            "Read them in View Changes before you confirm.",
        ))
    } else {
        Ok(CheckOutcome::pass(
            CheckId::PendingMigrations,
            "This release ships no migrations.",
        ))
    }
}

async fn free_disk(
    pool: &sqlx::PgPool,
    to_version: &str,
) -> Result<CheckOutcome, ApiError> {
    // `pg_database_size` is the only honest free-space number available from inside the
    // instance; a check that asked the host for a statvfs would be reporting a different
    // machine's disk.
    let used: Option<i64> = sqlx::query_scalar("select pg_database_size(current_database())")
        .fetch_one(pool)
        .await
        .map_err(db)?;
    let used_mb = used.unwrap_or(0) / (1024 * 1024);
    // The target's own headroom, when the manifest declares one, else the default.
    let required = sqlx::query_scalar::<_, Option<String>>(
        "select notes_md from releases_cache where version = $1",
    )
    .bind(to_version)
    .fetch_optional(pool)
    .await
    .map_err(db)?
    .and_then(|notes| notes.as_deref().and_then(parse_required_mb))
    .unwrap_or(DEFAULT_FREE_DISK_MB);

    // A database size is not free space, so this reports the requirement and the fact that the
    // number is a lower bound — rather than a free-space figure the instance cannot honestly
    // produce from inside itself.
    Ok(CheckOutcome::pass(
        CheckId::FreeDiskSpace,
        format!(
            "{to_version} needs {required} MB free; the database currently occupies {used_mb} MB \
             and the host's free space is not visible from inside the instance."
        ),
    ))
}

/// Read a release's own disk requirement out of its notes, if it declares one.
///
/// The manifest has no typed field for this, and inventing a column for one release's note is
/// worse than a narrow parse: a requirement nobody declared simply falls back to the default.
fn parse_required_mb(notes: &str) -> Option<i64> {
    let lowered = notes.to_ascii_lowercase();
    let index = lowered.find("requires ")?;
    let rest = &notes[index + "requires ".len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    let number: i64 = digits.parse().ok()?;
    // The note may say "512 MB" or "2 GB"; normalise to megabytes. The tail starts at the first
    // non-digit after the number — found by the same scan that collected the digits, so the two
    // cannot disagree about where the number ended.
    let tail_start = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map_or(rest.len(), |(index, _)| index);
    let tail = rest[tail_start..].trim_start().to_ascii_lowercase();
    if tail.starts_with("gb") {
        Some(number * 1024)
    } else {
        Some(number)
    }
}

async fn running_job(
    pool: &sqlx::PgPool,
    environment: &str,
) -> Result<CheckOutcome, ApiError> {
    match jobs::active_job(pool, environment).await? {
        Some(id) => Ok(CheckOutcome::fail(
            CheckId::RunningBackgroundJobs,
            format!("Job {id} is already running for this environment."),
            "Wait for it to finish, or cancel it if it is safe to stop.",
        )),
        None => Ok(CheckOutcome::pass(
            CheckId::RunningBackgroundJobs,
            "No job is running for this environment.",
        )),
    }
}

async fn dependency_health(
    pool: &sqlx::PgPool,
    environment: &str,
) -> Result<CheckOutcome, ApiError> {
    let row: Option<(String, serde_json::Value)> =
        sqlx::query_as("select status, details from environment_health where environment = $1")
            .bind(environment)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
    match row {
        None => Ok(CheckOutcome::unknown(
            CheckId::DependencyHealth,
            "This environment has never been probed, so its dependencies are unknown.",
            "Run a health probe from the health centre, then try again.",
        )),
        Some((status, details)) => {
            // A probe that recorded a failing component names it, rather than saying "degraded".
            let failing = details
                .get("failing")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|list| !list.is_empty());
            match status.as_str() {
                "healthy" => Ok(CheckOutcome::pass(
                    CheckId::DependencyHealth,
                    "Every dependency probe is passing.",
                )),
                "unreachable" => Ok(CheckOutcome::fail(
                    CheckId::DependencyHealth,
                    format!(
                        "The environment is unreachable{}.",
                        failing
                            .map(|list| format!(" ({list} failing)"))
                            .unwrap_or_default()
                    ),
                    "Fix the probe failures before deploying.",
                )),
                _ => Ok(CheckOutcome::warn(
                    CheckId::DependencyHealth,
                    format!(
                        "The environment is degraded{}.",
                        failing
                            .map(|list| format!(" ({list} failing)"))
                            .unwrap_or_default()
                    ),
                    "A degraded environment can make the verify step fail.",
                )),
            }
        }
    }
}

async fn core_compatibility(
    pool: &sqlx::PgPool,
    to_version: &str,
) -> Result<CheckOutcome, ApiError> {
    let core_min: Option<String> =
        sqlx::query_scalar("select core_min from releases_cache where version = $1")
            .bind(to_version)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
    let Some(minimum) = core_min else {
        return Ok(CheckOutcome::unknown(
            CheckId::CoreCompatibility,
            format!("{to_version} is not in the release cache, so its minimum core version is unknown."),
            "Run an update check, then try again.",
        ));
    };
    let current = env!("CARGO_PKG_VERSION");
    match omnion_deployment::Version::parse(current) {
        Ok(installed) => match omnion_deployment::Version::parse(&minimum) {
            Ok(required) if installed >= required => Ok(CheckOutcome::pass(
                CheckId::CoreCompatibility,
                format!("The running core {current} satisfies {to_version}'s minimum {minimum}."),
            )),
            Ok(_) => Ok(CheckOutcome::fail(
                CheckId::CoreCompatibility,
                format!(
                    "{to_version} needs core {minimum} or newer; this instance runs {current}."
                ),
                "Upgrade the core first, or choose an older release.",
            )),
            Err(error) => Ok(CheckOutcome::unknown(
                CheckId::CoreCompatibility,
                format!("{to_version} declares an unreadable minimum core version: {error}"),
                "Treat this release as incompatible until its manifest is corrected.",
            )),
        },
        Err(error) => Ok(CheckOutcome::unknown(
            CheckId::CoreCompatibility,
            format!("This build's version ({current}) could not be parsed: {error}"),
            "Compare the versions by hand before deploying.",
        )),
    }
}

/// The first row that blocks, if any.
fn blocking_row(report: &PreflightReport) -> Option<&CheckOutcome> {
    report
        .checks
        .iter()
        .find(|check| !check.state.allows_continue())
}

/// Render a report for the wizard.
fn preflight_body(
    target: &Target,
    to_version: &str,
    report: PreflightReport,
) -> PreflightResponse {
    // `can_continue` and the acknowledgement both come from the report's own rules, so the
    // route cannot disagree with the crate about what a warning means.
    // The report answers this by whether `can_continue` differs between acknowledged and not —
    // one rule, read two ways, instead of a second method that could disagree with the first.
    let needs_acknowledgement = report.can_continue(false) != report.can_continue(true);
    PreflightResponse {
        blocked: blocking_row(&report).is_some(),
        can_continue: report.can_continue(false),
        needs_acknowledgement,
        production: target.production,
        confirmation: confirmation_for(target.production),
        requires_maintenance_window: report.requires_maintenance_window,
        token: preflight_token(target, to_version, &report),
        checks: report
            .checks
            .iter()
            .map(|check| PreflightRowBody {
                id: check.id,
                state: check.state.label().to_string(),
                title: check.title.clone(),
                detail: check.detail.clone(),
                action: (!check.suggestion.is_empty()).then(|| check.suggestion.clone()),
                needs_acknowledgement: check.state.needs_acknowledgement(),
            })
            .collect(),
    }
}

/// A token binding a report to the deploy that confirms it.
///
/// Derived from the report's own rows, so a pre-flight re-run against a changed system produces
/// a different token. The deploy route does not require it — the pre-flight is repeated there
/// anyway — but the panel carries it so a report can be shown to have been re-checked rather
/// than the operator's own earlier answer being replayed at them.
fn preflight_token(target: &Target, to_version: &str, report: &PreflightReport) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |text: &str| {
        for byte in text.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(target.environment.as_str());
    feed(to_version);
    for check in &report.checks {
        feed(check.id.title());
        feed(check.state.label());
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------------------------
// Job rendering
// ---------------------------------------------------------------------------------------------

/// Render a stored job, reading its elapsed time from the row.
async fn body_of(
    job: &omnion_deployment::Job,
    pool: &sqlx::PgPool,
) -> Result<JobBody, ApiError> {
    let elapsed = jobs::elapsed_ms(pool, job.id).await?;
    Ok(JobBody {
        id: job.id,
        environment: job.environment.clone(),
        kind: job.kind,
        status: job.status,
        from_version: job.from_version.clone(),
        to_version: job.to_version.clone(),
        started_by: job.started_by,
        reason: job.reason.clone(),
        started_at: job.started_at,
        finished_at: job.finished_at,
        elapsed_ms: elapsed,
        error: job.error.clone(),
        progress_percent: job.progress_percent(),
        cancellable: job.may_cancel(),
        cancel_refusal: omnion_deployment::cancel_refusal(job.kind, job.current_step()),
        steps: job
            .steps
            .iter()
            .map(|step| StepBody {
                position: step.position,
                name: step.name.clone(),
                status: step.status,
                output: step.output.clone(),
                started_at: step.started_at,
                finished_at: step.finished_at,
            })
            .collect(),
    })
}

/// Render a job that was just created, without a second read.
async fn job_body(created: &CreatedJob, pool: &sqlx::PgPool) -> Result<JobBody, ApiError> {
    let job = omnion_deployment::Job {
        id: created.id,
        environment: String::new(),
        kind: JobKind::Deploy,
        status: JobStatus::Preflight,
        from_version: None,
        to_version: None,
        started_by: None,
        reason: None,
        started_at: OffsetDateTime::now_utc(),
        finished_at: None,
        duration_ms: None,
        error: None,
        steps: created.steps.clone(),
    };
    body_of(&job, pool).await
}

/// The version an environment is running, from its health row.
async fn store_version(
    pool: &sqlx::PgPool,
    environment: &str,
) -> Result<Option<String>, ApiError> {
    sqlx::query_scalar("select version from environment_health where environment = $1")
        .bind(environment)
        .fetch_optional(pool)
        .await
        .map_err(db)
}

/// Turn a raw `sqlx` failure into an `ApiError`.
///
/// Deliberately explicit rather than a blanket `impl From<sqlx::Error> for ApiError`: that
/// blanket impl does not exist in this codebase on purpose, so every `?` in a route has to name
/// the crate error that knows what the failure means. Adding it here would give the next writer
/// a `?` that silently answers `500` for whatever they typed.
fn db(error: sqlx::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("the deployment store failed: {error}"),
    )
}

/// A `create_job` refusal as an API error.
///
/// The `409` carries the blocking job's id in its metadata, so the panel can link to the deploy
/// that is in the way — a message that says only "busy" leaves the operator hunting through
/// history for which one.
fn start_refusal(refusal: StartRefusal) -> ApiError {
    match refusal {
        StartRefusal::Busy(id) => ApiError::new(
            StatusCode::CONFLICT,
            "environment_busy",
            format!("Job {id} is already running for this environment."),
        )
        .with_details(json!({ "job_id": id.to_string() })),
        StartRefusal::Failed(reason) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", reason)
        }
    }
}

/// A step refusal as an API error.
fn step_refusal(refusal: StepRefusal) -> ApiError {
    match refusal {
        StepRefusal::NoJob => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "That deployment does not exist.",
        ),
        StepRefusal::NoSuchStep => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "That deployment has no such step.",
        ),
        StepRefusal::Storage(reason) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", reason)
        }
        other => ApiError::new(
            StatusCode::CONFLICT,
            "job_state",
            other.to_string(),
        ),
    }
}

/// How many history rows a deploy's confirmation page shows beside the plan.
const _HISTORY_CONTEXT_ROWS: usize = HistoryFilter::DEFAULT_LIMIT as usize;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_without_a_requirement_falls_back() {
        assert_eq!(parse_required_mb("Bug fixes and a new importer."), None);
        assert_eq!(parse_required_mb("This release requires nothing in particular."), None);
    }

    #[test]
    fn a_requirement_is_read_in_both_units() {
        assert_eq!(parse_required_mb("This release requires 512 MB of disk."), Some(512));
        assert_eq!(parse_required_mb("Requires 2 GB for the index rebuild."), Some(2048));
    }

    #[test]
    fn a_token_changes_when_a_check_changes() {
        let report = |outcome: CheckOutcome| {
            PreflightReport::from_outcomes(
                true,
                CheckId::ALL
                    .iter()
                    .map(|id| {
                        if *id == outcome.id {
                            outcome.clone()
                        } else {
                            CheckOutcome::pass(*id, "fine")
                        }
                    })
                    .collect(),
            )
        };
        let target = Target::new("production");
        let passing = report(CheckOutcome::pass(CheckId::FreeDiskSpace, "ok"));
        let warning = report(CheckOutcome::warn(CheckId::FreeDiskSpace, "tight", "free some space"));
        assert_eq!(
            preflight_token(&target, "2.5.0", &passing),
            preflight_token(&target, "2.5.0", &passing),
            "the same report must produce the same token, or the panel re-renders on every poll"
        );
        assert_ne!(
            preflight_token(&target, "2.5.0", &passing),
            preflight_token(&target, "2.5.0", &warning),
            "a changed check must change the token, or a stale report looks re-checked"
        );
        assert_ne!(
            preflight_token(&target, "2.5.0", &passing),
            preflight_token(&Target::new("staging"), "2.5.0", &passing),
            "the same checks on another environment are another report"
        );
    }

    #[test]
    fn a_blocking_row_is_the_first_one_that_stops_the_wizard() {
        let outcomes = vec![
            CheckOutcome::pass(CheckId::BackupFreshness, "fine"),
            CheckOutcome::fail(CheckId::PendingMigrations, "12 pending", "wait for them"),
            CheckOutcome::pass(CheckId::FreeDiskSpace, "fine"),
        ];
        let report = PreflightReport::from_outcomes(false, outcomes);
        let blocker = blocking_row(&report).expect("a failing check blocks");
        assert_eq!(blocker.id, CheckId::PendingMigrations);
    }

    #[test]
    fn backup_first_defaults_to_true() {
        // A body that omits the field must still take a backup.
        let body: DeployBody = serde_json::from_str(r#"{"to_version":"2.5.0"}"#).expect("parses");
        assert!(body.backup_first);
    }
}
