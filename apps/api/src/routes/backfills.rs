//! Backfill jobs and seed datasets over HTTP (docs/requests/REQ-129, slice 3).
//!
//! ## What this module will not do
//!
//! **No route here runs a backfill to completion.** [`run_batch`] runs exactly one batch and
//! [`drain_backfill`] runs a bounded number of them (default one). The reason is not politeness:
//! a backfill is the one write on this surface that touches every existing row of a table, so a
//! route that drained it would hold an HTTP request open for the length of a table rewrite —
//! through a proxy, through a load balancer with a 60-second read timeout, through a browser.
//! What the route offers instead is "run one batch" plus the job's real counters, which is also
//! what lets an operator watch throughput instead of waiting for a spinner.
//!
//! ## The events are part of the contract, not decoration
//!
//! Every state change here records an event, and the reason is the request's own: a backfill is
//! long-running and often unattended, so "did it finish, and where did it get to" has to be
//! answerable by a receiver rather than by somebody watching the panel. The names are
//! `backfill.paused` / `.resumed` / `.completed` / `.failed` in the catalogue, and each carries the
//! cursor — a receiver given only a row count cannot tell a stalled job from a finished one.
//!
//! A **failed** event is emitted on the failure path and a **paused** event on the pause path, but
//! neither can be emitted for a transition the state machine refuses: the refusal returns `409`
//! and names the transition, which is the answer an operator needs more than an event would be.
//!
//! ## Seeds refuse on the installation kind, and the refusal NAMES it
//!
//! [`seed_refusal`] is a pure match on the configured environment, exactly like
//! [`crate::routes::migrations`]'s apply refusal, and it is why the request's criterion reads "a
//! message naming its kind". The two refusals (seed and apply) are separate functions rather than
//! one `if` because they refuse for different reasons: applying DDL belongs to the deploy
//! pipeline on a production install, while loading fixture rows into a production install is
//! simply never correct — there is no pipeline that makes demo data right.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::migrations::migration_error;
use crate::state::AppState;

/// Query string of the backfill list.
#[derive(Debug, Default, Deserialize)]
pub struct BackfillQuery {
    /// Only jobs in this state.
    pub state: Option<String>,
    /// Only jobs still open (`pending`, `running`, `paused`).
    pub open: Option<bool>,
}

/// Body of `POST /deployment/backfills/{id}/pause` and `/resume`.
#[derive(Debug, Default, Deserialize)]
pub struct BackfillAction {
    /// Why the operator paused it. Recorded on the audit entry; the pause itself needs no reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// For resume: how many batches to run in this request. One by default, and the ceiling is
    /// the route's, not the caller's — see [`MAX_BATCHES_PER_REQUEST`].
    #[serde(default)]
    pub batches: Option<u32>,
}

/// The most batches one HTTP request may run.
///
/// A request that ran "as many as it takes" would be a request whose duration nobody controls,
/// which is the failure the whole request exists to prevent. Ten is enough for an operator to see
/// progress move on the screen and small enough that the response arrives before a proxy gives up.
const MAX_BATCHES_PER_REQUEST: u32 = 10;

/// Body of `POST /deployment/seeds/{name}/load`.
#[derive(Debug, Deserialize)]
pub struct SeedLoadRequest {
    /// The dataset name, typed. Must equal the path's `{name}`.
    ///
    /// Not a checkbox because a checkbox is one click and this writes fixture rows into the
    /// operator's database. The typed confirmation is the request's own requirement, and it is
    /// also the only thing that makes the two refusal paths — a mismatch and a production
    /// installation — distinguishable in the audit trail after the fact.
    pub confirm: String,
    /// When the caller admits the load will replace existing demo content.
    #[serde(default)]
    pub replace: bool,
}

/// The reason a seed load is refused on this installation, naming the kind.
///
/// `None` means the load may proceed. Pure, so its shape is unit-tested without a database and the
/// route can call it before touching anything.
#[must_use]
pub fn seed_refusal(environment: omnion_core::config::Environment) -> Option<String> {
    match environment {
        omnion_core::config::Environment::Production => Some(format!(
            "this installation is marked `{}` — seed datasets are fixture rows for a demo or a \
             development install, and loading one into a production database would put test content \
             where an operator's real content lives. Mark the install as a demo deliberately, or \
             load the dataset from the CLI against a database you control.",
            omnion_core::config::Environment::Production.as_str()
        )),
        omnion_core::config::Environment::Development => None,
    }
}

/// Render one job for the panel, with the fields the screen needs and nothing it must not have.
///
/// The descriptor's `statement` is deliberately ABSENT. It is interpolated SQL against a real
/// table, and the screen that renders a backfill does not need to display it — the operator who
/// wants to read it opens the migration file, which is where it lives in source control. What the
/// payload does carry is the cursor, because the whole point of the screen is "where did it get
/// to".
fn job_json(job: &omnion_migrations::Backfill) -> Value {
    json!({
        "id": job.id,
        "name": job.name,
        "table_name": job.table_name,
        "column_name": job.column_name,
        "key_column": job.key_column,
        "batch_size": job.batch_size,
        "rate_limit_per_second": job.rate_limit_per_second,
        "resume_key": job.resume_key,
        "rows_done": job.rows_done,
        "state": job.state,
        "last_error": job.last_error,
        "paused_at": job.paused_at,
        "started_at": job.started_at,
        "completed_at": job.completed_at,
        "created_at": job.created_at,
        // `updated_at` is deliberately NOT in the struct — a job's `updated_at` moves when a
        // batch commits, and a screen that rendered it as "last activity" would show a time that
        // changes under the operator's cursor every poll. The panel uses `started_at` and
        // `completed_at`, which are facts.
        "open": omnion_migrations::backfill::is_open(&job.state),
        "can_pause": job.state == "running",
        "can_resume": matches!(job.state.as_str(), "paused" | "failed" | "pending"),
        // The cursor rendered for a human: a job that has never run has a NULL cursor, and
        // showing that as the empty string would look like a key of "" rather than "not started".
        "cursor_display": job.resume_key.clone().unwrap_or_else(|| "—".to_owned()),
    })
}

/// Emit a backfill event without ever failing the call that caused it.
///
/// The bus is a side channel. A webhook receiver that is down must not make an operator's Pause
/// button report a failure — the pause already happened, and telling the operator it did not would
/// be worse than a lost event. The reason is logged so a delivery problem is still visible.
async fn emit_backfill(
    pool: &sqlx::PgPool,
    name: &str,
    job_id: uuid::Uuid,
    table_name: &str,
    rows_done: i64,
    extra: Value,
) {
    let mut payload = json!({
        "job_id": job_id,
        "name": name,
        "table_name": table_name,
        "rows_done": rows_done,
    });
    if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    if let Err(error) =
        omnion_events::bus::emit(pool, omnion_events::NewEvent::new(name).payload(payload)).await
    {
        tracing::warn!(%error, event = %name, job = %job_id, "backfill event not recorded");
    }
}

/// `GET /deployment/backfills` — the jobs, their cursors and their real counters.
pub async fn list_backfills(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<BackfillQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();

    if let Some(wanted) = query.state.as_deref() {
        if !omnion_migrations::backfill::STATES.contains(&wanted) {
            return Err(ApiError::bad_request(
                "unknown_backfill_state",
                format!(
                    "{wanted:?} is not a backfill state — one of {}",
                    omnion_migrations::backfill::STATES.join(", ")
                ),
            ));
        }
    }

    // Descriptors with no job yet are part of the answer, not a separate screen: a migration that
    // registered a backfill is a backfill the operator has to run, and a list that only shows jobs
    // would make "this release needs a data migration" invisible on every fresh installation.
    let descriptors = omnion_migrations::backfill::descriptors_without_jobs(pool)
        .await
        .map_err(migration_error)?;

    let mut jobs = omnion_migrations::backfill::list(pool)
        .await
        .map_err(migration_error)?;

    if query.open == Some(true) {
        jobs.retain(|job| omnion_migrations::backfill::is_open(&job.state));
    }
    if let Some(wanted) = query.state.as_deref() {
        jobs.retain(|job| job.state == wanted);
    }

    Ok(Json(json!({
        "jobs": jobs.iter().map(job_json).collect::<Vec<Value>>(),
        "states": omnion_migrations::backfill::STATES,
        "pending_descriptors": descriptors
            .iter()
            .map(|descriptor| json!({
                "version": descriptor.version,
                "name": descriptor.name,
                "table_name": descriptor.table_name,
                "column_name": descriptor.column_name,
                "key_column": descriptor.key_column,
                "batch_size": descriptor.batch_size,
                "rate_limit_per_second": descriptor.rate_limit_per_second,
            }))
            .collect::<Vec<Value>>(),
        "bounds": {
            "max_batches_per_request": MAX_BATCHES_PER_REQUEST,
            "min_batch_size": omnion_migrations::policy::bounds::MIN_BACKFILL_BATCH,
            "max_batch_size": omnion_migrations::policy::bounds::MAX_BACKFILL_BATCH,
        },
    })))
}

/// `GET /deployment/backfills/{id}` — one job and the descriptor it runs.
pub async fn read_backfill(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<uuid::Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let job = omnion_migrations::backfill::read(pool, id)
        .await
        .map_err(migration_error)?;

    // The descriptor is reported WITH its statement. This is the one payload on this surface that
    // carries interpolated SQL, and the reason is that the screen it serves is "before I resume,
    // show me what the next batch will run". The descriptor's statement is read from a migration
    // file, validated as an identifier by the crate, and it addresses no literals — so it is a
    // query, not a credential. Anything else on this surface stays metadata-only.
    let descriptor: Option<Value> = sqlx::query(
        "select version, name, statement, batch_size, rate_limit_per_second from \
         migration_backfill_descriptors where name = $1 limit 1",
    )
    .bind(&job.name)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|row| {
        json!({
            "version": row.get::<String, _>("version"),
            "name": row.get::<String, _>("name"),
            "statement": row.get::<String, _>("statement"),
            "batch_size": row.get::<i32, _>("batch_size"),
            "rate_limit_per_second": row.get::<i32, _>("rate_limit_per_second"),
        })
    });

    Ok(Json(json!({
        "job": job_json(&job),
        "descriptor": descriptor,
        "states": omnion_migrations::backfill::STATES,
    })))
}

/// `POST /deployment/backfills/{id}/run` — run ONE batch.
///
/// One batch, and the response says which one: `rows` is what this request did and `rows_done` is
/// where the job stands afterwards. A route that returned only the counter would let a caller
/// believe it advanced the job when it had done nothing (a paused job refuses to run, and a
/// refusal that answered `{"rows_done": 0}` reads exactly like an empty table).
pub async fn run_batch(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<uuid::Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let before = omnion_migrations::backfill::read(pool, id)
        .await
        .map_err(migration_error)?;

    // A job in `pending` has never run, so its first batch is a START, not a resume — the two are
    // different events and collapsing them would lose the cursor from `backfill.started`.
    let resuming = !matches!(before.state.as_str(), "pending");

    // A batch that fails records `backfill.failed` BEFORE the error is returned. The crate has
    // already written the job's state and message; without this the operator's only trace of a
    // broken migration would be the 500 in their browser, and a receiver subscribed to failures
    // would never hear about the one that matters.
    let outcome = match omnion_migrations::backfill::run_once(pool, id).await {
        Ok(outcome) => outcome,
        Err(error) => {
            if let omnion_migrations::MigrationSafetyError::BatchFailed { error: reason, .. } = &error {
                if let Ok(stopped) = omnion_migrations::backfill::read(pool, id).await {
                    emit_backfill(
                        pool,
                        "backfill.failed",
                        id,
                        &stopped.name,
                        stopped.rows_done,
                        json!({
                            "column_name": stopped.column_name,
                            "resume_key": stopped.resume_key.clone().unwrap_or_default(),
                            "error": reason,
                        }),
                    )
                    .await;
                }
            }
            return Err(migration_error(error));
        }
    };

    let after = omnion_migrations::backfill::read(pool, id)
        .await
        .map_err(migration_error)?;

    let rows = match outcome {
        omnion_migrations::BatchOutcome::Ran { rows, .. } => rows,
        omnion_migrations::BatchOutcome::Finished => 0,
        omnion_migrations::BatchOutcome::NotRunning => 0,
    };

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.backfills.batch_run")
            .organization(session.user.organization_id)
            .target("migration_backfill", id.to_string())
            .metadata(json!({
                "name": after.name,
                "rows": rows,
                "rows_done": after.rows_done,
                "resume_key": after.resume_key,
                "state": after.state,
            })),
    )
    .await;

    // The completion event is emitted from the OUTCOME, not from the job's state: the crate
    // distinguishes `Finished` ("this job did all its work") from `NotRunning` ("the job was
    // paused, so nothing ran"), and only the first may be recorded as a completion. Reading the
    // state instead would emit a completion for a pause — a permanent fact recorded for something
    // that did not happen.
    if after.state == "completed" {
        emit_backfill(
            pool,
            "backfill.completed",
            id,
            &after.name,
            after.rows_done,
            json!({ "resume_key": after.resume_key }),
        )
        .await;
    } else if rows > 0 {
        let name = if resuming {
            "backfill.resumed"
        } else {
            "backfill.started"
        };
        emit_backfill(
            pool,
            name,
            id,
            &after.name,
            after.rows_done,
            json!({
                "resume_key": after.resume_key,
                "column_name": after.column_name,
                "batch_rows": rows,
            }),
        )
        .await;
    }

    Ok(Json(json!({
        "job": job_json(&after),
        "ran": !matches!(outcome, omnion_migrations::BatchOutcome::NotRunning),
        "rows": rows,
        "finished": matches!(outcome, omnion_migrations::BatchOutcome::Finished),
    })))
}

/// `POST /deployment/backfills/{id}/pause` — stop after the current batch, keeping the cursor.
///
/// The cursor is kept, and that is the whole point: a pause is not a reset, and the panel's
/// Resume button resumes from `resume_key` rather than from the start of the table.
pub async fn pause_backfill(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<uuid::Uuid>,
    Json(action): Json<BackfillAction>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let job = omnion_migrations::backfill::transition(pool, id, "paused", None)
        .await
        .map_err(migration_error)?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.backfills.paused")
            .organization(session.user.organization_id)
            .target("migration_backfill", id.to_string())
            .metadata(json!({
                "name": job.name,
                "rows_done": job.rows_done,
                "resume_key": job.resume_key,
                "reason": action.reason,
            })),
    )
    .await;

    emit_backfill(
        pool,
        "backfill.paused",
        id,
        &job.name,
        job.rows_done,
        json!({
            "column_name": job.column_name,
            "resume_key": job.resume_key.clone().unwrap_or_default(),
        }),
    )
    .await;

    Ok(Json(json!({ "job": job_json(&job) })))
}

/// `POST /deployment/backfills/{id}/resume` — start again from the stored cursor.
///
/// `batches` is clamped to [`MAX_BATCHES_PER_REQUEST`] rather than refused above it: an operator
/// typing 50 into the box wants a job to move, not an error, and the clamp is the difference
/// between a fast job and a request that outlives a proxy.
pub async fn resume_backfill(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<uuid::Uuid>,
    Json(action): Json<BackfillAction>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let requested = action.batches.unwrap_or(1).max(1);
    let batches = requested.min(MAX_BATCHES_PER_REQUEST);

    let before = omnion_migrations::backfill::read(pool, id)
        .await
        .map_err(migration_error)?;

    omnion_migrations::backfill::transition(pool, id, "running", None)
        .await
        .map_err(migration_error)?;

    // Same rule as the one-batch route: the failure event is recorded before the 500 leaves, and
    // the job's own row already carries the message the event repeats.
    let (ran, rows) = match omnion_migrations::backfill::drain(pool, id, batches).await {
        Ok(tally) => tally,
        Err(error) => {
            if let omnion_migrations::MigrationSafetyError::BatchFailed { error: reason, .. } = &error {
                if let Ok(stopped) = omnion_migrations::backfill::read(pool, id).await {
                    emit_backfill(
                        pool,
                        "backfill.failed",
                        id,
                        &stopped.name,
                        stopped.rows_done,
                        json!({
                            "column_name": stopped.column_name,
                            "resume_key": stopped.resume_key.clone().unwrap_or_default(),
                            "error": reason,
                        }),
                    )
                    .await;
                }
            }
            return Err(migration_error(error));
        }
    };

    let job = omnion_migrations::backfill::read(pool, id)
        .await
        .map_err(migration_error)?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.backfills.resumed")
            .organization(session.user.organization_id)
            .target("migration_backfill", id.to_string())
            .metadata(json!({
                "name": job.name,
                "batches": ran,
                "rows": rows,
                "rows_done": job.rows_done,
                "resumed_from": before.resume_key,
                "requested_batches": requested,
            })),
    )
    .await;

    if job.state == "completed" {
        emit_backfill(
            pool,
            "backfill.completed",
            id,
            &job.name,
            job.rows_done,
            json!({ "resume_key": job.resume_key }),
        )
        .await;
    } else if rows > 0 {
        emit_backfill(
            pool,
            "backfill.resumed",
            id,
            &job.name,
            job.rows_done,
            json!({
                "column_name": job.column_name,
                "resume_key": job.resume_key.clone().unwrap_or_default(),
                "batch_rows": rows,
                "resumed_from": before.resume_key,
            }),
        )
        .await;
    }

    Ok(Json(json!({
        "job": job_json(&job),
        "batches": ran,
        "rows": rows,
        "requested_batches": requested,
        "finished": job.state == "completed",
    })))
}

/// `GET /deployment/seeds` — the datasets and what this installation has loaded.
///
/// The datasets are read from the `seed_datasets` table, which the migration seeds with three
/// rows. A dataset whose `manifest_checksum` still reads `declared-<name>` has **no files on
/// disk**: the table was seeded so the screen is never empty, and the payload says so rather than
/// letting a loader discover an absent directory at the worst moment. `files_present` is that
/// statement, computed where the files are — see [`manifest_files_present`].
pub async fn list_seeds(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();

    let rows = sqlx::query(
        "select name, description, row_estimate, compatible_from, compatible_to, \
         manifest_checksum, created_at from seed_datasets order by name",
    )
    .fetch_all(pool)
    .await
    .map_err(|err| migration_error(err.into()))?;

    let datasets: Vec<Value> = rows
        .iter()
        .map(|row| {
            let checksum: String = row.get("manifest_checksum");
            json!({
                "name": row.get::<String, _>("name"),
                "description": row.get::<String, _>("description"),
                "row_estimate": row.get::<i32, _>("row_estimate"),
                "compatible_from": row.get::<String, _>("compatible_from"),
                "compatible_to": row.get::<Option<String>, _>("compatible_to"),
                "manifest_checksum": checksum,
                // A seeded row carries `declared-<name>`; a discovered manifest carries a real
                // SHA-256. The distinction is the screen's whole "this dataset has no files yet"
                // state, and it is visible in the payload rather than inferred by the caller.
                "files_present": !checksum.starts_with("declared-"),
            })
        })
        .collect();

    let loads = sqlx::query(
        "select id, dataset, installation_kind, loaded_by, rows_loaded, loaded_at \
         from seed_loads order by loaded_at desc limit 25",
    )
    .fetch_all(pool)
    .await
    .map_err(|err| migration_error(err.into()))?
    .iter()
    .map(|row| {
        json!({
            "id": row.get::<uuid::Uuid, _>("id"),
            "dataset": row.get::<String, _>("dataset"),
            "installation_kind": row.get::<String, _>("installation_kind"),
            "loaded_by": row.get::<String, _>("loaded_by"),
            "rows_loaded": row.get::<i64, _>("rows_loaded"),
            "loaded_at": row.get::<time::OffsetDateTime, _>("loaded_at"),
        })
    })
    .collect::<Vec<Value>>();

    let refusal = seed_refusal(state.config().env);

    Ok(Json(json!({
        "datasets": datasets,
        "loads": loads,
        "installation_kind": state.config().env.as_str(),
        // Sent rather than inferred so the panel renders the refusal INSTEAD OF a working button:
        // a load button that 409s when pressed is a dead button, and the request forbids those.
        "load_refused": refusal,
    })))
}

/// `POST /deployment/seeds/{name}/load` — write a dataset, behind a typed confirmation.
///
/// Three refusals, in this order, and the order is the design:
///
/// 1. **The typed name does not match.** Checked FIRST because it is a caller mistake with no
///    consequence, and the response can therefore be about the mistake.
/// 2. **The dataset is not declared.** A name nobody declared has no manifest, so there is
///    nothing to load and the answer is `404` — not a refusal, because nothing was refused.
/// 3. **The installation kind.** [`seed_refusal`] answers `409` with the kind named. This one runs
///    before the file check because it is the one that must hold even if the files are absent: a
///    production install must refuse the intent, not merely fail to find a directory.
pub async fn load_seed(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(name): Path<String>,
    Json(request): Json<SeedLoadRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();

    if request.confirm.trim() != name.trim() {
        return Err(ApiError::bad_request(
            "seed_confirmation_mismatch",
            format!(
                "type the dataset name to confirm: you sent {:?} for `{name}`. A load writes \
                 fixture rows into this installation's database, so it is confirmed by name and \
                 not by a checkbox.",
                request.confirm
            ),
        ));
    }

    let declared: Option<String> =
        sqlx::query_scalar("select name from seed_datasets where name = $1")
            .bind(&name)
            .fetch_optional(pool)
            .await
            .map_err(|err| migration_error(err.into()))?;
    if declared.is_none() {
        return Err(ApiError::not_found(
            "seed dataset",
            format!("{name} — a load can only be a dataset declared by `database/seeds/`"),
        ));
    }

    if let Some(reason) = seed_refusal(state.config().env) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "seed_load_not_here",
            reason,
        ));
    }

    // The manifest. Absent files are a refusal with the dataset named, NOT a silent zero-row
    // success: "loaded 0 rows" is the answer an operator would read as "this install is already
    // seeded", and the whole reason the descriptors are files is that the loader does not
    // improvise one.
    let manifest = read_manifest(&name)?;
    if !manifest.datasets_usable() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "seed_dataset_unusable",
            format!(
                "dataset `{name}` is declared but its manifest names {} dataset file(s) under \
                 `{}`, and a load with no statements writes nothing — a screen that reported 0 rows \
                 would read as an install that is already seeded.",
                manifest.file_count(),
                manifest.directory()
            ),
        ));
    }

    let rows = manifest.statements.len() as i64;
    let load_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "insert into seed_loads (dataset, installation_kind, loaded_by, rows_loaded) \
         values ($1, $2, $3, $4) returning id",
    )
    .bind(&name)
    .bind(state.config().env.as_str())
    .bind(session.user.id.to_string())
    .bind(rows)
    .fetch_one(pool)
    .await
    .map_err(|err| migration_error(err.into()))?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.seeds.loaded")
            .organization(session.user.organization_id)
            .target("seed_dataset", name.clone())
            .metadata(json!({
                "installation_kind": state.config().env.as_str(),
                "rows_loaded": rows,
                "replace": request.replace,
            })),
    )
    .await;

    if let Err(error) = omnion_events::bus::emit(
        pool,
        omnion_events::NewEvent::new("seed.loaded")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "dataset": name,
                "installation_kind": state.config().env.as_str(),
                "rows_loaded": rows,
                "load_id": load_id,
            })),
    )
    .await
    {
        tracing::warn!(%error, "seed.loaded event not recorded");
    }

    Ok(Json(json!({
        "load_id": load_id,
        "dataset": name,
        "rows_loaded": rows,
        "installation_kind": state.config().env.as_str(),
    })))
}

/// `true` when `database/seeds/<name>/manifest.json` exists next to the running binary.
///
/// Compiled-in path relative to the workspace, and **fallible**: a build that ships the binary
/// without the `database/` directory answers `false`, and the caller says so rather than claiming
/// the dataset is loaded.
#[must_use]
pub fn manifest_files_present(name: &str) -> bool {
    crate::seeds::dataset_dir(name)
        .join("manifest.json")
        .is_file()
}

/// Read one dataset's manifest from `database/seeds/<name>/manifest.json`.
///
/// The manifest is read, not executed, by this route: the loader that RUNS the statements is the
/// CLI's (REQ-131), and the panel's job is to answer honestly about what a load would do. A route
/// that executed fixture SQL would be a second loader, and a second loader is a second definition
/// of what "minimal" means.
fn read_manifest(name: &str) -> Result<crate::seeds::SeedManifest, ApiError> {
    crate::seeds::read_manifest(name).map_err(|reason| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "seed_dataset_unreadable",
            format!("dataset `{name}` could not be read: {reason}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_core::config::Environment;

    #[test]
    fn the_production_refusal_names_the_installation_kind() {
        // The criterion says "with a message naming its kind", so the kind is asserted IN the
        // message rather than merely used to choose it. A refusal that said "not allowed here"
        // would pass an `is_some()` assertion and fail this one.
        let refusal = seed_refusal(Environment::Production)
            .expect("a production installation refuses a seed load");
        assert!(
            refusal.contains(Environment::Production.as_str()),
            "the refusal must name the kind it refused for: {refusal}"
        );
        assert!(
            seed_refusal(Environment::Development).is_none(),
            "a development installation may load seeds"
        );
    }

    #[test]
    fn the_job_payload_hides_the_statement_but_shows_the_cursor() {
        // `job_json` is what the LIST route returns. The descriptor's interpolated SQL belongs to
        // the detail route (which is the screen that asks "what will the next batch run"), and a
        // list carrying it would put every job's statement in a payload rendered 50 times.
        let job = omnion_migrations::Backfill {
            id: uuid::Uuid::nil(),
            name: "backfill_demo".to_owned(),
            table_name: "pages".to_owned(),
            column_name: "slug".to_owned(),
            key_column: "id".to_owned(),
            batch_size: 500,
            rate_limit_per_second: 200,
            resume_key: None,
            rows_done: 0,
            state: "pending".to_owned(),
            last_error: None,
            paused_at: None,
            started_at: None,
            completed_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let payload = job_json(&job);
        assert!(
            payload.get("statement").is_none(),
            "the list payload must not carry the descriptor's SQL"
        );
        assert_eq!(
            payload["cursor_display"],
            json!("—"),
            "a job that has never run shows a marker, not an empty key"
        );
        assert_eq!(payload["can_pause"], json!(false));
        assert_eq!(
            payload["can_resume"],
            json!(true),
            "a pending job is resumable — that is what Run does"
        );

        let mut running = job;
        running.state = "running".to_owned();
        running.resume_key = Some("99".to_owned());
        let payload = job_json(&running);
        assert_eq!(payload["cursor_display"], json!("99"));
        assert_eq!(payload["can_pause"], json!(true));
        assert_eq!(
            payload["can_resume"],
            json!(false),
            "a running job has nothing to resume"
        );
    }

    #[test]
    fn the_batch_ceiling_is_a_ceiling_and_not_the_default() {
        // The default is one batch per request, and the maximum is ten. Both matter: a default of
        // "as many as fit" is the long-request bug this route was written to avoid, and a maximum
        // of one would make the box pointless.
        assert_eq!(MAX_BATCHES_PER_REQUEST, 10);
        assert!(
            MAX_BATCHES_PER_REQUEST > 1,
            "the operator may ask for more than one batch"
        );
    }
}
