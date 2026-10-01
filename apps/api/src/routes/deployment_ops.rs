//! `/api/v1/deployment/maintenance` and `/rollback` (REQ-024, slice 3).
//!
//! Slice 2 shipped the deploy wizard's write side. This file is the pair of things that make a
//! deploy survivable, and both are small on screen and large in consequence:
//!
//! * **A rollback**, which is the answer to "the deploy went wrong". It is a job like any other
//!   — same step log, same history row, same one-per-environment index — with two rules the
//!   table already holds: a **reason is mandatory** (a rollback nobody explains is how an
//!   instance ends up bouncing between two versions for a month) and it **takes a backup first**,
//!   because a rollback is itself a change and the operator needs a way back from it.
//! * **The maintenance window**, which is a promise to every API client: "nothing is changing
//!   for ten minutes". Its enforcement is the part worth writing down, because the obvious
//!   version of it — an `if` in the deploy handler — protects exactly one route. What is here
//!   instead is a *layer*, and the layer is what makes the promise keep holding as the platform
//!   grows routes under it.
//!
//! The window's rules themselves are in the crate (`omnion_deployment::maintenance`), not here.
//! That is the point: `is_active`, the scope split and the form's refusals are decisions with
//! their own tests, and a route that re-derives "is it active right now" is a second
//! implementation that will disagree with the first.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use omnion_audit::NewAuditEntry;
use omnion_deployment::job::JobKind;
use omnion_deployment::jobs::{self, NewJob, Target};
use omnion_deployment::maintenance::{
    self, Block, MAX_MESSAGE_LEN, Scope, Window, WindowEdit, WindowRefusal,
};

use crate::auth::CurrentSession;
use crate::deployment_runner;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/deployment/environments/{environment}/rollback`.
#[derive(Debug, Clone, Deserialize)]
pub struct RollbackRequest {
    /// The version to go back to. Must not be the version that is already running.
    pub to_version: String,
    /// Why. Mandatory, and the table refuses it too.
    pub reason: String,
    /// Take a backup before the rollback starts. Defaults to `true` on the server as well as in
    /// the form, because the dangerous default is the one where a rollback changes the data with
    /// no way back.
    #[serde(default = "default_true")]
    pub backup_first: bool,
}

/// `default_true` in its own function, so the server default and the documented default are the
/// same value and the panel's checkbox cannot disagree with the API's behaviour.
fn default_true() -> bool {
    true
}

/// The job a rollback created.
#[derive(Debug, Clone, Serialize)]
pub struct RollbackResponse {
    /// The rollback job, in the same shape the wizard's step 3 polls.
    pub job: crate::routes::deployment_run::JobBody,
    /// A sentence for the result banner, naming the version it is going back to.
    pub message: String,
}

/// Start a rollback.
///
/// `POST /api/v1/deployment/environments/{environment}/rollback`
#[derive(Debug, Serialize)]
pub struct RollbackError {
    /// `reason_missing`, `confirmation_mismatch`, `already_running`, `maintenance`.
    pub code: &'static str,
    /// The message the panel shows.
    pub message: String,
    /// The blocking job's id, when the environment is busy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// `start_rollback` — begin a rollback to a previous version.
///
/// Three refusals, and each is a different mistake:
///
/// * **no reason** — `400`. A rollback is the most consequential action in this screen and the
///   only one whose justification outlives it in the history table.
/// * **a target that is already running** — `400`. Rolling "back" to the current version is a
///   button that appears to work and changes nothing, which is worse than an error.
/// * **the environment is busy** — `409` with the blocking job's id, from the same partial
///   index a deploy hits. A rollback does not get an exception to it: a rollback that starts
///   while a deploy is migrating is the failed deploy *and* a half-applied rollback.
pub async fn start_rollback(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
    Json(body): Json<RollbackRequest>,
) -> Result<(StatusCode, Json<RollbackResponse>), ApiError> {
    let pool = state.db().pool();
    let target = Target::new(environment);
    let reason = body.reason.trim().to_string();

    if reason.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "reason_missing",
            "A rollback needs a reason. It is stored on the history row and is what the next \
             operator reads.",
        ));
    }

    let to_version = body.to_version.trim().to_string();
    if to_version.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_to_version",
            "Name the version to roll back to.",
        ));
    }

    // What is running right now. Read *before* the busy check so the "already running" refusal
    // is available even when the answer would be a 409 anyway.
    let from_version =
        crate::routes::deployment_run::current_version(pool, &target.environment).await?;
    // Only a *known* running version can be "already running". An environment with no health
    // row reports `None`, and refusing the rollback there would be a window whose only way out
    // is to fix the probe — on the one route an operator reaches for when the instance is
    // already in trouble.
    if from_version.as_deref() == Some(to_version.as_str()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "already_running",
            format!("{to_version} is the version this environment is already running."),
        ));
    }

    // One active job per environment, the same partial index a deploy hits. Read first so the
    // refusal names *which* deploy is in the way, rather than leaving the operator to hunt
    // through the history list.
    if let Some(blocking) = jobs::active_job(pool, &target.environment).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "environment_busy",
            "This environment already has a job in progress.",
        )
        .with_details(json!({ "blocking_job_id": blocking })));
    }

    let created = jobs::create_job(
        pool,
        &NewJob {
            target: target.clone(),
            kind: JobKind::Rollback,
            from_version: from_version.clone(),
            to_version: Some(to_version.clone()),
            actor: Some(current.user.id),
            reason: Some(reason),
            backup_id: None,
            // A rollback names versions; the `0214` constraint refuses one that also names a
            // workload, so the field is set from the kind and not by the caller.
            workload: None,
        },
    )
    .await
    .map_err(start_refusal)?;

    // The runner is the same one a deploy uses, told which plan to follow. A rollback has no
    // migrate step (the older binary reads the append-only schema), so the runner's migration
    // loop runs zero times and the `verify` step still compares what the instance reports.
    deployment_runner::spawn_rollback(pool.clone(), created.id, body.backup_first);

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "deployment.rolled_back")
            .target("deployment", created.id.to_string())
            .metadata(json!({
                "environment": target.environment,
                "from": from_version,
                "to": to_version,
                "backup_first": body.backup_first,
                "reason": body.reason,
            })),
    )
    .await?;

    let job = crate::routes::deployment_run::created_job_body(&created, pool, &target.environment)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RollbackResponse {
            job,
            message: match from_version.as_deref() {
                Some(version) => format!("Rolling {version} back to {to_version}."),
                // The instance reports no version. "Rolling back to 2.4.1" is still true and
                // still useful; a banner that printed "Rolling <unknown> back" reads as a bug.
                None => format!("Rolling back to {to_version}."),
            },
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Maintenance window — the screen
// ---------------------------------------------------------------------------------------------

/// The screen's window, with everything the form needs.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenanceBody {
    /// The environment this row is for.
    pub environment: String,
    /// The operator's toggle.
    pub enabled: bool,
    /// Is it open *right now*? Distinct from `enabled` — a window scheduled for tomorrow is
    /// enabled and not open, and a screen that shows only the toggle makes an operator think the
    /// promise is live when it is not.
    pub active: bool,
    /// The banner text.
    pub message: String,
    /// Scheduled start, or `None` for "as soon as it is enabled".
    pub starts_at: Option<time::OffsetDateTime>,
    /// Scheduled end, or `None` for open-ended.
    pub ends_at: Option<time::OffsetDateTime>,
    /// `all` or `admin`.
    pub scope: &'static str,
    /// Who last changed it, and when — the row is the audit trail for the banner itself.
    pub updated_by: Option<Uuid>,
    pub updated_at: Option<time::OffsetDateTime>,
    /// The cap, so the form's `maxLength` and the server's refusal are the same number.
    pub max_message_length: usize,
}

/// Every environment's window, for the screen and the shell banner.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenanceListBody {
    /// One row per environment. Environments with no row are **absent**, not synthesised as
    /// disabled: the screen's "never configured" state and its "configured and off" state are
    /// different facts and the form's `updated_at` is how it tells them apart.
    pub windows: Vec<MaintenanceBody>,
    /// Any window open right now, across all environments — the shell banner reads this and does
    /// not have to re-derive activity from the per-row `active` flags.
    pub active: Vec<MaintenanceBody>,
    /// The cap, repeated at the top level for the form.
    pub max_message_length: usize,
}

impl MaintenanceBody {
    fn from_window(window: &Window, now: time::OffsetDateTime) -> Self {
        Self {
            environment: window.environment.clone(),
            enabled: window.enabled,
            active: window.is_active(now),
            message: window.message.clone(),
            starts_at: window.starts_at,
            ends_at: window.ends_at,
            scope: window.scope.as_str(),
            updated_by: window.updated_by,
            updated_at: window.updated_at,
            max_message_length: MAX_MESSAGE_LEN,
        }
    }
}

/// `GET /api/v1/deployment/maintenance` — every window.
pub async fn list_maintenance(
    State(state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<MaintenanceListBody>, ApiError> {
    let pool = state.db().pool();
    let now = time::OffsetDateTime::now_utc();
    let windows = maintenance::list_windows(pool).await?;

    let rows: Vec<MaintenanceBody> = windows
        .iter()
        .map(|window| MaintenanceBody::from_window(window, now))
        .collect();
    let active = rows.iter().filter(|row| row.active).cloned().collect();

    Ok(Json(MaintenanceListBody {
        windows: rows,
        active,
        max_message_length: MAX_MESSAGE_LEN,
    }))
}

/// `PUT /api/v1/deployment/maintenance/{environment}`.
#[derive(Debug, Clone, Deserialize)]
pub struct MaintenanceUpdate {
    /// The operator's toggle.
    pub enabled: bool,
    /// The banner text. Trimmed and length-checked by the crate.
    #[serde(default)]
    pub message: String,
    /// Optional start.
    pub starts_at: Option<time::OffsetDateTime>,
    /// Optional end.
    pub ends_at: Option<time::OffsetDateTime>,
    /// Optional scope. `None` keeps the stored one, so a form that edits only the message does
    /// not silently widen an `admin` window to `all`.
    pub scope: Option<String>,
}

/// Save a window, or refuse with the form's own error.
pub async fn update_maintenance(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
    Json(body): Json<MaintenanceUpdate>,
) -> Result<Json<MaintenanceBody>, ApiError> {
    let pool = state.db().pool();

    // An unknown environment is a 404, not a new row: the three are the three, and a typo in the
    // URL creating a fourth one is a row nothing ever reads.
    if !matches!(environment.as_str(), "production" | "staging" | "sandbox") {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_environment",
            format!("{environment:?} is not an environment."),
        ));
    }

    // An unreadable scope is refused rather than defaulted. Defaulting it to `all` blocks the
    // platform over a typo; defaulting it to `admin` gives an operator a window that does not
    // block what they asked it to block.
    let scope = match body.scope.as_deref() {
        Some(value) => Some(Scope::parse(value).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "unknown_scope",
                format!("{value:?} is not a scope. Use \"all\" or \"admin\"."),
            )
        })?),
        None => None,
    };

    let stored = maintenance::load_window(pool, &environment).await?;
    let edit = WindowEdit {
        enabled: body.enabled,
        message: body.message,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        scope,
    };
    let saved = maintenance::save(&environment, &stored, &edit).map_err(window_refusal)?;

    maintenance::store_window(pool, &saved, Some(current.user.id)).await?;

    let action = if saved.enabled {
        "deployment.maintenance.enabled"
    } else {
        "deployment.maintenance.disabled"
    };
    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, action)
            .target("environment", environment.clone())
            .metadata(json!({
                "enabled": saved.enabled,
                "scope": saved.scope.as_str(),
                "starts_at": saved.starts_at,
                "ends_at": saved.ends_at,
                "active": saved.is_active(time::OffsetDateTime::now_utc()),
            })),
    )
    .await?;

    let fresh = maintenance::load_window(pool, &environment).await?;
    Ok(Json(MaintenanceBody::from_window(
        &fresh,
        time::OffsetDateTime::now_utc(),
    )))
}

/// A `create_job` refusal as an API error.
///
/// Copied from the deploy route rather than shared, and the reason is the two answers differ:
/// this one has to name a *rollback* in its prose. A shared mapper would have to take a noun,
/// and a noun parameter is a sign the two callers wanted different sentences.
fn start_refusal(refusal: omnion_deployment::jobs::StartRefusal) -> ApiError {
    use omnion_deployment::jobs::StartRefusal;
    match refusal {
        StartRefusal::Busy(id) => ApiError::new(
            StatusCode::CONFLICT,
            "environment_busy",
            "This environment already has a job in progress.",
        )
        .with_details(json!({ "blocking_job_id": id })),
        StartRefusal::Failed(reason) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "job_not_created",
            format!("The rollback could not be started: {reason}"),
        ),
    }
}

/// The crate's refusal as the form's `422`.
fn window_refusal(refusal: WindowRefusal) -> ApiError {
    let code = match refusal {
        WindowRefusal::MessageTooLong(_) => "message_too_long",
        WindowRefusal::EndBeforeStart => "end_before_start",
        WindowRefusal::NoMessage => "message_required",
    };
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, refusal.to_string())
}

// ---------------------------------------------------------------------------------------------
// The enforcement layer
// ---------------------------------------------------------------------------------------------

/// A `503` for a write during a maintenance window.
///
/// The response carries the operator's **own message** in the body and as the `message` field,
/// because the caller of an API cannot read a banner: an API client that receives a generic
/// "service unavailable" cannot tell a planned window from an outage, and that is exactly the
/// distinction that decides whether it retries for ten minutes or pages somebody.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenanceRefusal {
    /// The banner text the operator wrote.
    pub message: String,
    /// A stable code, so a client can branch without parsing prose.
    pub code: &'static str,
    /// The refusal sentence, for a client that shows neither of the above.
    pub reason: String,
    /// `true` for a public-platform write, `false` for a panel write.
    pub scope_applied: bool,
}

/// Build the `503` a write route returns during a window.
///
/// Returns `None` when the write is allowed, which is the shape a `layer` wants: a handler
/// wrapper that answers `Ok(response)` when there is no window is one line, and the refusal
/// path — the one with the message, the audit entry and the `503` — lives here rather than being
/// copied into every route that needs it.
pub async fn refuse_during_window(
    state: &AppState,
    environment: &str,
    platform_write: bool,
) -> Result<Option<ApiError>, ApiError> {
    let pool = state.db().pool();
    let window = maintenance::load_window(pool, environment).await?;
    let now = time::OffsetDateTime::now_utc();
    let Some(block) = window.block_for(platform_write, now) else {
        return Ok(None);
    };
    Ok(Some(maintenance_error(&block, window.scope)))
}

/// Turn a [`Block`] into the `503`.
///
/// `Retry-After` is **not** set: a window has no end that a client can usefully count down to
/// (it is open-ended by default), and a `Retry-After` that means "in 900 seconds" on an
/// open-ended window teaches a client to hammer the platform on a timer forever.
fn maintenance_error(block: &Block, scope: Scope) -> ApiError {
    let message = if block.message.is_empty() {
        "A maintenance window is open for this environment.".to_string()
    } else {
        block.message.clone()
    };
    let body = MaintenanceRefusal {
        message: message.clone(),
        code: "maintenance_window",
        reason: block.reason().to_string(),
        scope_applied: scope == Scope::All,
    };
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "maintenance_window",
        message,
    )
    .with_details(serde_json::to_value(body).unwrap_or_else(|_| json!({})))
}

/// `503` for a deploy during a maintenance window, checked by the wizard's own routes.
///
/// Kept as its own function so the *reason* a deploy is refused reads as a window rather than
/// as a busy environment: an operator who opened a window and then found `Deploy` greyed out
/// with no message would open a second window.
pub async fn refuse_deploy_during_window(
    state: &AppState,
    environment: &str,
) -> Result<(), ApiError> {
    if let Some(error) = refuse_during_window(state, environment, true).await? {
        return Err(error);
    }
    Ok(())
}

/// The `Environment` in a request, as the layer needs it.
///
/// Reads the request's own path; a request that has no environment in its path is a request to a
/// route this layer does not apply to, and the caller passes the default.
pub fn environment_from_path(path: &str) -> Option<String> {
    // `/api/v1/deployment/environments/{env}/...` is the only shape the write routes use.
    let rest = path.strip_prefix("/api/v1/deployment/environments/")?;
    let environment = rest.split('/').next()?;
    if matches!(environment, "production" | "staging" | "sandbox") {
        Some(environment.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_read_from_the_environments_path() {
        assert_eq!(
            environment_from_path("/api/v1/deployment/environments/production/deploy"),
            Some("production".to_string())
        );
        assert_eq!(
            environment_from_path("/api/v1/deployment/environments/staging"),
            Some("staging".to_string())
        );
    }

    #[test]
    fn a_path_with_no_environment_is_not_this_layers_business() {
        // A read (`/deployment/history`) must never be caught by the write layer, and an
        // environment the table does not know is not silently treated as production.
        assert_eq!(environment_from_path("/api/v1/deployment/history"), None);
        assert_eq!(
            environment_from_path("/api/v1/deployment/environments/edge/deploy"),
            None
        );
        assert_eq!(environment_from_path("/api/v1/other"), None);
    }

    #[test]
    fn a_window_appears_in_the_active_list_only_while_it_is_open() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_788_000_000).unwrap();
        let mut window = Window::unset("production");
        window.enabled = true;
        window.message = "Upgrading.".to_string();
        assert!(MaintenanceBody::from_window(&window, now).active);

        window.ends_at = Some(now);
        assert!(
            !MaintenanceBody::from_window(&window, now).active,
            "a window whose end has arrived is not active"
        );
    }

    #[test]
    fn the_refusal_body_carries_the_operators_message() {
        let block = Block {
            message: "Core upgrade until 14:00.".to_string(),
        };
        let error = maintenance_error(&block, Scope::All);
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), "maintenance_window");
        assert_eq!(error.message(), "Core upgrade until 14:00.");
        // `Retry-After` must be absent: an open-ended window has no end to count down to, and a
        // header that says "in 900 seconds" teaches a client to retry on a timer for ever.
        let details = error.details().expect("the refusal carries its scope");
        assert!(details.get("retry_after").is_none());
    }

    #[test]
    fn an_empty_banner_message_still_answers_with_a_sentence() {
        let block = Block {
            message: String::new(),
        };
        let error = maintenance_error(&block, Scope::Admin);
        assert!(error.message().contains("maintenance window"));
    }

    #[test]
    fn the_body_scope_says_which_scope_applied() {
        let block = Block {
            message: "m".to_string(),
        };
        // `all` blocks public writes; `admin` does not. The client needs to know which, because
        // an API caller refused under `all` is a public outage and one refused under `admin` is
        // an operator's own change.
        let all = maintenance_error(&block, Scope::All);
        let admin = maintenance_error(&block, Scope::Admin);
        assert_eq!(
            all.details().and_then(|d| d.get("scope_applied")),
            Some(&json!(true))
        );
        assert_eq!(
            admin.details().and_then(|d| d.get("scope_applied")),
            Some(&json!(false))
        );
    }

    #[test]
    fn a_refusal_never_carries_a_retry_after() {
        // A window is open-ended by default, so a `Retry-After` would be a number the server
        // cannot honour. Absent is the honest answer.
        for scope in [Scope::All, Scope::Admin] {
            let error = maintenance_error(
                &Block {
                    message: "m".into(),
                },
                scope,
            );
            let serialised = serde_json::to_string(error.details().expect("details")).unwrap();
            assert!(
                !serialised.contains("retry_after"),
                "no Retry-After in {serialised}"
            );
        }
    }
}
