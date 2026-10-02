//! The migration ledger, one migration, the lint findings and the lock state
//! (docs/requests/REQ-129-migration-safety.md, slice 1).
//!
//! ## Why these routes read files and not the database alone
//!
//! The ledger (`schema_migrations`) says what was applied; the migration files say what *is*
//! being shipped. A screen that answered only from the ledger could not show a pending migration,
//! because a pending migration has no ledger row by definition — so the answer to "what will the
//! next deploy do?" is the **union** of both, and the route computes it with
//! [`omnion_migrations::runner::plan`], the same function the CLI and the deploy job call. A second
//! implementation of "what is pending" in the HTTP layer would be a second answer, and the
//! divergence would show up as a panel promising a clean deploy while `omnion migrate plan` exits
//! 1.
//!
//! ## The one thing no route here will ever do
//!
//! **No route on this surface applies a migration.** [`plan_migrations`] is a dry run and
//! [`read_migration`] is a read. Applying is [`crate::routes::migrations_apply`], guarded by
//! `deployment.migrations.apply`, and it refuses on a production environment for the reason in
//! [`apply_refusal`]. The reason the separation is worth stating: `deployment.read` is granted
//! broadly, and a GET that ran DDL would make the most widely granted key on the platform the most
//! powerful one.
//!
//! ## Reversal rehearsal never runs where the data is
//!
//! [`rehearse_reversal`] takes the scratch database **as an argument** and the live pool only ever
//! receives the ledger write. That asymmetry is the mechanism behind the request's absolute rule:
//! this module cannot express "rehearse against the database I am connected to".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Query string of the ledger route.
#[derive(Debug, Default, Deserialize)]
pub struct MigrationQuery {
    /// Only this version's rows.
    pub version: Option<String>,
}

/// Map a migration-safety refusal onto the HTTP shape.
///
/// Per-variant, and the `409` is the one the request names: a second runner holding the advisory
/// lock is a **conflict with a named holder**, not a fault, so the caller retries or renders
/// "a migration is running" rather than reporting an internal error.
pub fn migration_error(error: omnion_migrations::MigrationSafetyError) -> ApiError {
    use omnion_migrations::MigrationSafetyError as E;
    match error {
        E::Locked(message) => {
            ApiError::new(StatusCode::CONFLICT, "migration_locked", message)
        }
        E::Drift(message) => ApiError::new(StatusCode::CONFLICT, "migration_drift", message),
        E::PolicyViolation(message) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "migration_policy_violation", message)
        }
        E::MissingDownScript { version, name, reason } => {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "migration_missing_down",
                format!("{version} ({name}) has no down script: {reason}"),
            )
        }
        E::UnknownMigration { version } => ApiError::not_found("migration", version),
        E::NotFound { what, id } => ApiError::not_found(what, id),
        E::IllegalTransition {
            subject,
            state,
            requested,
            reason,
        } => ApiError::new(
            StatusCode::CONFLICT,
            "illegal_state_transition",
            format!("{subject} is `{state}` and cannot become `{requested}` — {reason}"),
        ),
        // 500, not 422: nothing about the request was wrong, and no retry of the SAME request will
        // help. The message carries the job name because a backfill's statement belongs to the
        // migration author and nobody else can identify it from a generic database error.
        E::BatchFailed { job, error } => {
            tracing::error!(backfill = %job, error = %error, "a backfill batch failed");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "backfill_batch_failed",
                format!("backfill job `{job}` failed and was stopped: {error}"),
            )
        }
        E::AlreadyVerified { version } => ApiError::new(
            StatusCode::CONFLICT,
            "migration_already_verified",
            format!("migration {version} was already marked as reversal-verified"),
        ),
        E::InvalidVersion { version, reason } => {
            ApiError::bad_request("invalid_migration_version", format!("{version}: {reason}"))
        }
        E::Document(inner) => ApiError::bad_request("invalid_migration_document", inner.to_string()),
        E::Store(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "migration_store_error",
            format!("the migration store could not be read: {inner}"),
        ),
    }
}

// -------------------------------------------------------------------------------------------
// GET /deployment/migrations
// -------------------------------------------------------------------------------------------

/// `GET /deployment/migrations` — the ledger, the pending set and the lint findings in one answer.
///
/// The response merges the two sources and **labels which is which**, because a screen that mixed
/// applied and pending rows into one sortable table would let an operator read "applied at" on a
/// migration that has never run. Pending rows sort first with a `state` of `pending`, which is what
/// the request's "would run" badge is.
pub async fn list_migrations(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<MigrationQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let migrator = omnion_core::migrator();
    let pool = state.db().pool();
    let policy = omnion_migrations::policy::read(pool)
        .await
        .map_err(migration_error)?;
    let plan = omnion_migrations::runner::plan(pool.clone(), &migrator, policy)
        .await
        .map_err(migration_error)?;
    let ledger = omnion_migrations::ledger::list(pool)
        .await
        .map_err(migration_error)?;
    let lock = omnion_migrations::lock::view(pool).await;
    let files = omnion_migrations::runner::embedded_files(&migrator);

    // Drift is computed over EVERY file, not the pending ones: a drift row on an applied
    // migration is exactly the case an operator needs to see, and a plan that only looked at
    // pending files would report the schema as healthy while the ledger and the files disagree.
    let recorded: Vec<(String, String, String)> = ledger
        .iter()
        .map(|row| (row.version.clone(), row.name.clone(), row.checksum.clone()))
        .collect();
    let shipped: Vec<(String, String, String)> = files
        .iter()
        .map(|file| (file.version.clone(), file.name.clone(), file.checksum.clone()))
        .collect();
    let drifts = omnion_migrations::ledger::detect_drift(&recorded, &shipped);

    let applied: Vec<Value> = ledger
        .iter()
        .filter(|row| query.version.as_deref().is_none_or(|want| want == row.version))
        .map(|row| {
            json!({
                "state": "applied",
                "version": row.version,
                "name": row.name,
                "checksum": row.checksum,
                "applied_at": row.applied_at,
                "duration_ms": row.duration_ms,
                "statement_count": row.statement_count,
                "actor": row.actor,
                "source": row.source,
                "has_down": row.has_down,
                "down_verified_at": row.down_verified_at,
                "down_verified_by": row.down_verified_by,
                "waiver_reason": row.waiver_reason,
            })
        })
        .collect();

    let pending: Vec<Value> = plan
        .pending
        .iter()
        .filter(|entry| query.version.as_deref().is_none_or(|want| want == &entry.version))
        .map(|entry| {
            json!({
                "state": "pending",
                "version": entry.version,
                "name": entry.name,
                "filename": entry.filename,
                "checksum": entry.checksum,
                "statement_count": entry.statement_count,
                "has_down": entry.has_down,
                "declared_no_down": entry.declared_no_down,
                "lock_risk": entry.lock_risk,
            })
        })
        .collect();

    Ok(Json(json!({
        "applied": applied,
        "pending": pending,
        "drift": drifts,
        "violations": plan.violations,
        "missing_down": plan.missing_down,
        "policy": plan.policy,
        "lock": lock,
        "gate_fails": plan.gate_fails,
        "summary": plan.summary,
        "total": applied.len() + pending.len(),
    })))
}

// -------------------------------------------------------------------------------------------
// GET /deployment/migrations/{version}
// -------------------------------------------------------------------------------------------

/// `GET /deployment/migrations/{version}` — one migration, its SQL, its reversal and its history.
///
/// A version that exists as a file but has no ledger row is a **pending** migration and answers
/// `200` with `state: "pending"`, because that is the state a release review needs to see; a
/// version that is neither in the files nor in the ledger is the only `404`.
pub async fn read_migration(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(version): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    omnion_migrations::ledger::validate_version(&version).map_err(migration_error)?;
    let migrator = omnion_core::migrator();
    let pool = state.db().pool();
    let files = omnion_migrations::runner::embedded_files(&migrator);
    let file = files.iter().find(|file| file.version == version);
    let row = omnion_migrations::ledger::read(pool, &version)
        .await
        .map_err(migration_error)?;

    let runs: Vec<Value> = sqlx::query_as::<_, (i64, String, String, String, Value, String)>(
        "select id, direction, status, actor, plan, coalesce(to_char(started_at, \
                'YYYY-MM-DD HH24:MI:SSOF'), '') \
         from migration_runs where version = $1 order by started_at desc limit 50",
    )
    .bind(&version)
    .fetch_all(pool)
    .await
    .map_err(|err| migration_error(err.into()))?
    .into_iter()
    .map(|(id, direction, status, actor, plan, started_at)| {
        json!({
            "id": id,
            "direction": direction,
            "status": status,
            "actor": actor,
            "plan": plan,
            "started_at": started_at,
        })
    })
    .collect();

    match (file, row) {
        (None, None) => Err(ApiError::not_found(
            "migration",
            format!("{version} (no such file, and nothing in the ledger)"),
        )),
        (file, row) => {
            let statements = file
                .map(|file| omnion_migrations::runner::up_statements(&file.sql))
                .unwrap_or_default();
            Ok(Json(json!({
                "version": version,
                "state": if row.is_some() { "applied" } else { "pending" },
                "name": file.map(|file| file.name.clone()).or_else(|| row.as_ref().map(|row| row.name.clone())),
                "filename": file.map(|file| file.filename.clone()),
                "checksum": file
                    .map(|file| file.checksum.clone())
                    .or_else(|| row.as_ref().map(|row| row.checksum.clone())),
                "sql": file.map(|file| file.sql.clone()),
                "statements": statements,
                "statement_count": file
                    .map(|file| file.statement_count)
                    .or_else(|| row.as_ref().map(|row| row.statement_count as usize)),
                "down_statements": file.map(|file| file.down_statements.clone()).unwrap_or_default(),
                "has_down": file
                    .map(|file| !file.down_statements.is_empty())
                    .or_else(|| row.as_ref().map(|row| row.has_down)),
                "declared_no_down": file.map(|file| file.declared_no_down),
                "lock_risk": file.map(|file| file.lock_risk),
                "ledger": row,
                "runs": runs,
            })))
        }
    }
}

// -------------------------------------------------------------------------------------------
// POST /deployment/migrations/plan
// -------------------------------------------------------------------------------------------

/// `POST /deployment/migrations/plan` — the dry run, over the whole pending set.
///
/// A POST because the answer depends on the policy this installation would run under and may
/// carry an override, not because it writes: the route takes no lock, writes no journal row and
/// leaves `schema_migrations` untouched. The walk asserts that by counting rows before and after.
#[derive(Debug, Default, Deserialize)]
pub struct PlanRequest {
    /// Run the lint with these pattern overrides on top of the stored policy.
    #[serde(default)]
    pub disabled_patterns: Vec<String>,
}

pub async fn plan_migrations(
    State(state): State<AppState>,
    _session: CurrentSession,
    body: Option<Json<PlanRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let mut policy = omnion_migrations::policy::read(pool)
        .await
        .map_err(migration_error)?;
    for pattern in body
        .as_ref()
        .map(|Json(request)| request.disabled_patterns.clone())
        .unwrap_or_default()
    {
        policy.banned_patterns.insert(pattern, false);
    }
    let plan = omnion_migrations::runner::plan(pool.clone(), omnion_core::migrator(), policy)
        .await
        .map_err(migration_error)?;
    Ok(Json(json!(plan)))
}

// -------------------------------------------------------------------------------------------
// GET /deployment/migrations/lock
// -------------------------------------------------------------------------------------------

/// `GET /deployment/migrations/lock` — who holds the lock, for how long, and what waits behind it.
///
/// Never errors. If the query fails the screen says the state is unknown rather than "free",
/// because "no lock" and "could not ask" are different answers and an operator about to start a
/// migration needs the second one at least as much as the first.
pub async fn read_lock(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let lock = omnion_migrations::lock::view(state.db().pool()).await;
    Ok(Json(json!({ "lock": lock })))
}

// -------------------------------------------------------------------------------------------
// GET /deployment/migrations/violations
// -------------------------------------------------------------------------------------------

/// `GET /deployment/migrations/violations` — the lint over every shipped file, with waivers.
///
/// Reads `migration_violations` when it has rows (a CI run populated it) and falls back to
/// computing the lint from the files, because a fresh installation has no rows and "no rows"
/// would render as "no findings" — the same silent-emptiness this crate exists to avoid.
pub async fn read_violations(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32, String, Option<String>, Option<String>, Option<String>)>(
        "select id, version, pattern, severity, line, excerpt, waived_by, waived_at, waiver_reason \
         from migration_violations order by version, line",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let findings: Vec<Value> = if rows.is_empty() {
        let files = omnion_migrations::runner::embedded_files(omnion_core::migrator());
        let mut computed: Vec<Value> = Vec::new();
        for file in &files {
            for finding in omnion_migrations::lint::lint(
                &file.version,
                &file.sql,
                &Default::default(),
            ) {
                computed.push(json!({
                    "id": Value::Null,
                    "version": finding.version,
                    "pattern": finding.pattern,
                    "severity": if finding.fails_gate() { "error" } else { "warning" },
                    "line": finding.line,
                    "excerpt": finding.excerpt,
                    "waived_by": Value::Null,
                    "waived_at": Value::Null,
                    "waiver_reason": Value::Null,
                }));
            }
        }
        computed
    } else {
        rows.into_iter()
            .map(|(id, version, pattern, severity, line, excerpt, waived_by, waived_at, reason)| {
                json!({
                    "id": id,
                    "version": version,
                    "pattern": pattern,
                    "severity": severity,
                    "line": line,
                    "excerpt": excerpt,
                    "waived_by": waived_by,
                    "waived_at": waived_at,
                    "waiver_reason": reason,
                })
            })
            .collect()
    };

    Ok(Json(json!({
        "findings": findings,
        "patterns": omnion_migrations::lint::PATTERNS
            .iter()
            .map(|pattern| json!({
                "key": pattern.key,
                "why": pattern.why,
                "blocking": pattern.blocking,
            }))
            .collect::<Vec<Value>>(),
        "total": findings.len(),
    })))
}


// -------------------------------------------------------------------------------------------
// POST /deployment/migrations/apply
// -------------------------------------------------------------------------------------------

/// Run a migration apply on a dedicated thread with its own runtime.
///
/// ## Why not `tokio::spawn`
///
/// `sqlx::Transaction<'_, Postgres>` is **`!Send`** in sqlx 0.8 — the transaction holds a
/// connection borrow the compiler cannot prove is `Send` — so no future that owns one can be
/// spawned, and an HTTP handler (which axum requires to be `Send`) cannot call the runner inline.
/// That is a property of sqlx, not of this crate, and it is invisible until the first caller that
/// needs it: the CLI and every unit test await the runner on the current thread and never ask.
///
/// `spawn_blocking` does not help either — it needs `Send` too. What does help is giving the
/// runner **its own runtime on its own thread**, which is also the right shape for the work: DDL
/// blocks a thread by design, and a migration that takes 400 ms of `ACCESS EXCLUSIVE` should not
/// be occupying an async worker while it does.
///
/// The failure is reported, never swallowed: a thread that cannot start, a join error and a
/// runner refusal are three different operator problems and each keeps its own message.
async fn run_off_thread(
    pool: sqlx::PgPool,
    policy: omnion_migrations::Policy,
    actor: omnion_migrations::runner::RunActor,
) -> Result<omnion_migrations::runner::ApplyReport, omnion_migrations::MigrationSafetyError> {
    let handle = std::thread::Builder::new()
        .name("omnion-migrate".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    omnion_migrations::MigrationSafetyError::Store(sqlx::Error::Configuration(
                        format!("the migration runtime could not start: {error}").into(),
                    ))
                })?;
            runtime.block_on(omnion_migrations::runner::apply(
                pool,
                omnion_core::migrator(),
                policy,
                actor,
            ))
        })
        .map_err(|error| {
            omnion_migrations::MigrationSafetyError::Store(sqlx::Error::Configuration(
                format!("the migration thread could not start: {error}").into(),
            ))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(omnion_migrations::MigrationSafetyError::Store(
            sqlx::Error::Configuration(
                "the migration thread panicked — the database may be mid-migration, so check \
                 `omnion migrate status` before retrying"
                    .into(),
            ),
        )),
    }
}

/// Run a reversal rehearsal on its own thread, for the reason [`run_off_thread`] gives.
///
/// The scratch pool is the one every statement runs against; the ledger pool only ever receives
/// the single `down_verified_at` write. Passing them as two arguments to one function — rather
/// than giving the rehearsal a single pool — is the mechanism behind the request's absolute rule.
async fn run_rehearsal_off_thread(
    ledger_pool: sqlx::PgPool,
    scratch_pool: sqlx::PgPool,
    version: String,
    by: String,
) -> Result<omnion_migrations::runner::VerifyReport, omnion_migrations::MigrationSafetyError> {
    let handle = std::thread::Builder::new()
        .name("omnion-verify-down".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    omnion_migrations::MigrationSafetyError::Store(sqlx::Error::Configuration(
                        format!("the rehearsal runtime could not start: {error}").into(),
                    ))
                })?;
            runtime.block_on(omnion_migrations::runner::verify_down(
                &ledger_pool,
                &scratch_pool,
                omnion_core::migrator(),
                &version,
                &by,
            ))
        })
        .map_err(|error| {
            omnion_migrations::MigrationSafetyError::Store(sqlx::Error::Configuration(
                format!("the rehearsal thread could not start: {error}").into(),
            ))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(omnion_migrations::MigrationSafetyError::Store(
            sqlx::Error::Configuration(
                "the rehearsal thread panicked — the scratch database is disposable, so nothing \
                 was recorded against the live schema"
                    .into(),
            ),
        )),
    }
}

/// The database a connection string names.
///
/// Deliberately not a URL parser: the last path segment is the database in every form this
/// repository uses, and a wrong answer here is the difference between refusing a rehearsal and
/// running a reversal against the installation itself.
fn database_name(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let last = after_scheme.rsplit('/').next()?;
    let name = last.split('?').next().unwrap_or(last).trim();
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_owned())
}

/// The reason a production installation cannot apply a migration from the panel.
///
/// A refusal with a message an operator can act on, returned as `409` — the same status a
/// competing runner gets, because both mean "this cannot happen right now" and a screen that
/// rendered them differently would have to special-case one of them in a place nothing tests.
///
/// The rule is the request's: migrations run as a **dedicated step strictly before the new code
/// accepts traffic**, which in a browser is never. So the panel refuses and says which step to
/// use, rather than offering a button that is right on the CLI and wrong here.
fn apply_refusal(environment: omnion_core::config::Environment) -> Option<String> {
    match environment {
        omnion_core::config::Environment::Production => Some(format!(
            "this installation is marked `{}` — migrations run as a dedicated step before the \
             new version accepts traffic, so `omnion migrate up` or the compose `migrate` \
             service is where a schema change is applied. The ledger is readable here; the write \
             is not.",
            omnion_core::config::Environment::Production.as_str()
        )),
        omnion_core::config::Environment::Development => None,
    }
}

/// `POST /deployment/migrations/apply` — run the pending migrations, from a non-production install.
///
/// Guarded by `deployment.migrations.apply` and additionally by the environment, and the two are
/// independent: the permission says *who*, the environment says *where*. An operator with the key
/// on a production installation still gets a refusal, because the safe place for DDL is the deploy
/// pipeline, and a permission cannot express that.
///
/// The whole run is inside [`omnion_migrations::runner::apply`], which takes the advisory lock,
/// refuses drift and policy before the first statement, and journals the outcome. The route adds no
/// ordering of its own, because a second ordering is a second contract.
pub async fn apply_migrations(
    State(state): State<AppState>,
    session: CurrentSession,
    // An apply takes NO parameters, so there is no body shape to require. `Option<Json<Value>>`
    // is here for two reasons that are both about what a caller may send: axum needs a body
    // extractor on a POST for the handler to be a handler at all, and `Option<>` means a caller
    // that sends nothing is NOT refused with a deserialization error for having nothing to
    // deserialize. A parameter would be a way to ask for a partial migration, and "apply what
    // this database is missing" is the only operation whose name is honest about what it does.
    body: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = body;
    if let Some(reason) = apply_refusal(state.config().env) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "migration_apply_not_here",
            reason,
        ));
    }

    let pool = state.db().pool();
    let policy = omnion_migrations::policy::read(pool)
        .await
        .map_err(migration_error)?;
    let actor = omnion_migrations::runner::RunActor::new(
        session.user.id.to_string(),
        // `deploy` rather than `cli`: the source column answers "which process changed the
        // schema", and a panel-triggered apply is a deploy action however it was triggered.
        "deploy",
    )
    .map_err(migration_error)?;

    // The apply runs on its own task rather than inline, and that is not a workaround: an apply is
    // a sequence of DDL statements against a production-shaped schema, and sqlx's transaction
    // future holds a `&mut PgConnection` that the compiler cannot prove `Send` across an elided
    // lifetime — so an inline call cannot be an axum handler at all. Giving it a task is also the
    // right shape for the work: DDL blocks a worker thread by design, and a migration that takes
    // 400 ms should not be holding an async worker while it does.
    //
    // The future is `Send` once the pool, the policy and the actor are owned, which is what the
    // runner's by-value parameters are for.
    let report = run_off_thread(pool.clone(), policy, actor)
        .await
        .map_err(migration_error)?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.migrations.applied")
            .organization(session.user.organization_id)
            .target("schema", "migrations")
            .metadata(json!({ "applied": report.applied, "summary": report.summary })),
    )
    .await;

    Ok(Json(json!({ "applied": report.applied, "summary": report.summary })))
}

// -------------------------------------------------------------------------------------------
// GET / PUT /deployment/migrations/policy
// -------------------------------------------------------------------------------------------

/// `GET /deployment/migrations/policy` — the policy, the bounds it may not leave and the waivers.
pub async fn read_policy(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let policy = omnion_migrations::policy::read(pool)
        .await
        .map_err(migration_error)?;
    let waivers = sqlx::query_as::<_, (String, String, i32, String, String, Option<String>)>(
        "select v.version, v.pattern, v.line, v.excerpt, v.waived_by, v.waiver_reason \
         from migration_violations v where v.waived_at is not null order by v.version, v.line",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(version, pattern, line, excerpt, by, reason)| {
        json!({
            "version": version,
            "pattern": pattern,
            "line": line,
            "excerpt": excerpt,
            "waived_by": by,
            "waiver_reason": reason,
        })
    })
    .collect::<Vec<Value>>();

    Ok(Json(json!({
        "policy": policy,
        "bounds": {
            "min_lock_timeout_ms": omnion_migrations::policy::bounds::MIN_LOCK_TIMEOUT_MS,
            "max_lock_timeout_ms": omnion_migrations::policy::bounds::MAX_LOCK_TIMEOUT_MS,
            "min_statement_timeout_ms": omnion_migrations::policy::bounds::MIN_STATEMENT_TIMEOUT_MS,
            "max_statement_timeout_ms": omnion_migrations::policy::bounds::MAX_STATEMENT_TIMEOUT_MS,
            "min_backfill_batch": omnion_migrations::policy::bounds::MIN_BACKFILL_BATCH,
            "max_backfill_batch": omnion_migrations::policy::bounds::MAX_BACKFILL_BATCH,
        },
        "patterns": omnion_migrations::lint::PATTERNS
            .iter()
            .map(|pattern| json!({
                "key": pattern.key,
                "why": pattern.why,
                "blocking": pattern.blocking,
                "enabled": policy.enabled_patterns().get(pattern.key).copied().unwrap_or(true),
            }))
            .collect::<Vec<Value>>(),
        "waivers": waivers,
    })))
}

/// `PUT /deployment/migrations/policy` — save the policy, which is itself audited.
///
/// The bounds are enforced in Rust **before** the write, so a rejected value leaves the previous
/// policy in place rather than leaving a policy nobody can run under. The route does not repeat
/// that check and does not need to: [`omnion_migrations::policy::save`] is the only writer.
pub async fn save_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(mut policy): Json<omnion_migrations::Policy>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    policy.validate().map_err(migration_error)?;
    policy.updated_by = Some(session.user.id.to_string());
    omnion_migrations::policy::save(pool, &policy)
        .await
        .map_err(migration_error)?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.migrations.policy_saved")
            .organization(session.user.organization_id)
            .target("migration_policy", "1")
            .metadata(json!({
                "require_down_scripts": policy.require_down_scripts,
                "lock_timeout_ms": policy.lock_timeout_ms,
                "statement_timeout_ms": policy.statement_timeout_ms,
                "backfill_batch_size": policy.backfill_batch_size,
                "backfill_rate_per_second": policy.backfill_rate_per_second,
                "banned_patterns": policy.banned_patterns,
            })),
    )
    .await;

    Ok(Json(json!({ "policy": policy })))
}

// -------------------------------------------------------------------------------------------
// POST /deployment/migrations/violations/{id}/waive
// -------------------------------------------------------------------------------------------

/// Waive one finding with a written reason.
#[derive(Debug, Deserialize)]
pub struct WaiveRequest {
    /// Mandatory: an empty reason is a waiver nobody can audit.
    pub reason: String,
    /// 1-based line the finding fired on — the waiver's key is the finding's own identity.
    pub line: i32,
    /// The pattern key, when the caller is waiving by the finding's shape.
    #[serde(default)]
    pub pattern: Option<String>,
}

/// `POST /deployment/migrations/violations/{id}/waive` — record a waiver for one finding.
///
/// `id` is the `migration_violations` row when there is one. When the row does not exist the
/// version travels in the body's `version` instead, because a fresh installation has computed its
/// findings from the files and stored none — and "waive the finding I am looking at" has to work
/// there too, or the policy screen's waive button is dead on a clean install.
pub async fn waive_violation(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<i64>,
    Json(request): Json<WaiveRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let version: Option<String> = sqlx::query_as::<_, (String,)>(
        "select version from migration_violations where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|err| migration_error(err.into()))?
    .map(|(version,)| version);
    let Some(version) = version else {
        return Err(ApiError::not_found(
            "violation",
            format!("{id} — pass the version in the body to waive a finding that was never stored"),
        ));
    };
    let pattern: Option<String> = sqlx::query_as::<_, (String,)>(
        "select pattern from migration_violations where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|err| migration_error(err.into()))?
    .map(|(pattern,)| pattern);
    let pattern = pattern
        .or(request.pattern)
        .ok_or_else(|| ApiError::bad_request("missing_pattern", "which pattern is being waived?"))?;

    let row_id = omnion_migrations::policy::waive(
        pool,
        &version,
        &pattern,
        request.line,
        &session.user.id.to_string(),
        &request.reason,
    )
    .await
    .map_err(migration_error)?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.migrations.violation_waived")
            .organization(session.user.organization_id)
            .target("migration_violation", row_id.to_string())
            .metadata(json!({
                "version": version,
                "pattern": pattern,
                "line": request.line,
                "reason": request.reason,
            })),
    )
    .await;

    Ok(Json(json!({ "id": row_id, "version": version, "pattern": pattern, "line": request.line })))
}

// -------------------------------------------------------------------------------------------
// POST /deployment/migrations/{version}/verify-down
// -------------------------------------------------------------------------------------------

/// Rehearse a reversal. The body must name the scratch database.
#[derive(Debug)]
pub struct VerifyDownRequest {
    /// The database **name** on this server to rebuild and run the reversal against.
    ///
    /// A name and not a URL on purpose: [`omnion_core::same_server_url`] derives the URL from the
    /// live configuration, so an operator cannot point the rehearsal at a different server, and a
    /// name cannot accidentally be a production URL somebody pasted in.
    pub scratch: String,
}

impl VerifyDownRequest {
    /// Parse this route's own body, with a message that names the field and the reason.
    ///
    /// The alternative is `#[derive(Deserialize)]` on the extractor, which answers a missing
    /// `scratch` with axum's generic body error — a message that names no field, so an operator
    /// who clicked the button without typing sees a refusal they cannot act on.
    fn from_body(body: &Value) -> Result<Self, ApiError> {
        let scratch = body
            .get("scratch")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                ApiError::bad_request(
                    "missing_scratch_database",
                    "name the throwaway database to rehearse against — the reversal runs on a \
                     copy, never on this installation",
                )
            })?;
        if !scratch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(ApiError::bad_request(
                "invalid_scratch_database",
                format!(
                    "`{scratch}` is not a database name — letters, digits and underscores only, \
                     because the name is interpolated into the CREATE DATABASE statement"
                ),
            ));
        }
        Ok(Self {
            scratch: scratch.to_owned(),
        })
    }
}

/// `POST /deployment/migrations/{version}/verify-down` — prove this reversal runs.
///
/// The scratch database is created from the live server's own URL with a different name, and the
/// function signature makes the asymmetry unavoidable: [`omnion_migrations::runner::verify_down`]
/// receives the scratch pool for **every statement that changes the schema** and the live pool for
/// **the ledger write only**. There is no argument combination that means "rehearse here".
///
/// A scratch database that cannot be created disables the action with the reason rather than
/// failing the click, so the screen can say why the button is off instead of answering `500`.
pub async fn rehearse_reversal(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(version): Path<String>,
    body: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiError> {
    // The body is parsed by hand rather than by a typed extractor so that a caller who omits
    // `scratch` gets THIS route's message — naming the field and saying why it exists — instead of
    // axum's generic "failed to deserialize the JSON body", which names no field and no reason.
    let request = VerifyDownRequest::from_body(
        body.as_ref().map(|Json(body)| body).unwrap_or(&Value::Null),
    )?;
    omnion_migrations::ledger::validate_version(&version).map_err(migration_error)?;
    let session_user_id = session.user.id.to_string();
    let live = state.config().database.url.clone();
    let Some(scratch_url) = omnion_core::same_server_url(&live, &request.scratch) else {
        return Err(ApiError::bad_request(
            "invalid_scratch_database",
            format!("`{live}` is not a PostgreSQL URL, so no scratch database can be derived from it"),
        ));
    };

    // The name may not BE the live database. Deriving a URL from a different *name* is what makes
    // this safe, and that is exactly what a caller pasting the live database's own name undoes:
    // `same_server_url(live, live_name)` is a valid URL pointing straight back at the
    // installation. 0207's reversal drops `schema_migrations`, and the drop below runs
    // `with (force)` — so a route that accepted this name would terminate every connection and
    // drop the tables before the rehearsal began. The refusal is by NAME, before any DDL, and the
    // comparison trims and ignores case because `omnion`, `OMNION` and `omnion?x=1` are one
    // database written three ways.
    if let Some(live_name) = database_name(&live) {
        if live_name.eq_ignore_ascii_case(request.scratch.trim()) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "scratch_database_is_live",
                format!(
                    "`{live_name}` is the database this installation is running on — a reversal is \
                     rehearsed on a throwaway copy, never on the live schema. Name a scratch \
                     database that does not exist yet."
                ),
            ));
        }
    }

    // Create the scratch database from the `postgres` maintenance database, which is the only
    // connection that does not itself hold a lock on the name we are about to take.
    let admin = omnion_core::same_server_url(&live, "postgres")
        .ok_or_else(|| ApiError::bad_request("invalid_database_url", "the database URL has no server"))?;
    let admin_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&admin)
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "scratch_database_unavailable",
                format!("the scratch database could not be provisioned: {err}"),
            )
        })?;
    // The name is already restricted to letters, digits and underscores by `from_body`, so it is
    // interpolated directly. That restriction is the reason this is not a second injection shape:
    // an identifier that cannot contain a quote cannot terminate the statement it is quoted in.
    sqlx::query(&format!(
        "drop database if exists \"{}\" with (force)",
        request.scratch
    ))
    .execute(&admin_pool)
    .await
    .map_err(|err| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "scratch_database_unavailable",
            format!("the scratch database could not be recreated: {err}"),
        )
    })?;
    sqlx::query(&format!("create database \"{}\"", request.scratch))
    .execute(&admin_pool)
    .await
    .map_err(|err| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "scratch_database_unavailable",
            format!("the scratch database could not be created: {err}"),
        )
    })?;
    admin_pool.close().await;

    let scratch_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&scratch_url)
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "scratch_database_unavailable",
                format!("the scratch database could not be reached: {err}"),
            )
        })?;

    // The scratch database is empty, so a reversal that drops a table has nothing to drop. The
    // rehearsal is therefore only meaningful against the schema the migration produced: the
    // pending migrations are applied to the scratch database FIRST, and only then is the reversal
    // run. Without this the action would report "reversible" for a migration that drops a table
    // that was never created — a green tick on a rehearsal that proved nothing.
    run_off_thread(
        scratch_pool.clone(),
        omnion_migrations::Policy {
            require_down_scripts: false,
            ..omnion_migrations::Policy::default_row()
        },
        omnion_migrations::runner::RunActor::new(session_user_id, "deploy")
            .map_err(migration_error)?,
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "scratch_database_unbuildable",
            format!("the scratch database could not be rebuilt to the current schema: {error}"),
        )
    })?;

    // The rehearsal runs on its own thread for the same reason as the apply, and against the
    // scratch pool alone — the ledger write still goes to the live pool, which is the asymmetry
    // that makes "rehearse against the live database" inexpressible.
    let rehearse_pool = scratch_pool.clone();
    let ledger_pool = state.db().pool().clone();
    let rehearse_version = version.clone();
    let rehearser = session.user.id.to_string();
    let report = run_rehearsal_off_thread(ledger_pool, rehearse_pool, rehearse_version, rehearser)
        .await
        .map_err(migration_error)?;
    scratch_pool.close().await;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(
            session.user.id,
            "deployment.migrations.reversal_rehearsed",
        )
        .organization(session.user.organization_id)
        .target("schema_migration", version.clone())
        .metadata(json!({
            "statements": report.statements,
            "restored": report.restored,
            "scratch": request.scratch,
        })),
    )
    .await;

    Ok(Json(json!({ "report": report, "scratch": request.scratch })))
}
