//! `/api/v1/secrets/root-key` — the key ring, the seal self-check and the rotation ceremony
//! (docs/requests/REQ-125, slice 1).
//!
//! The surface is deliberately small, because everything here is irreversible if an operator
//! gets it wrong:
//!
//! * `GET /secrets/root-key` — the ring as the panel reads it: the active key's fingerprint, the
//!   retired ones with the versions they still carry, the seal self-check's verdict and the
//!   live re-wrap job's real counter. **Metadata only** — no wrapped material, no checksum, no
//!   key id beyond what a version already names.
//! * `POST /secrets/root-key/rotate` — the ceremony. It generates the replacement, wraps it,
//!   flips the active key and opens a resumable re-wrap job. It refuses when a job is already
//!   live, and refuses when the self-check says the operator key cannot open the ring — that is
//!   exactly the situation in which a rotation destroys data.
//! * `GET` / `POST .../rewrap-jobs/{id}` — progress, and pause/resume.
//!
//! The screen this backs (`/secrets/root-key`) is a three-step wizard for a reason: the ceremony
//! is safe only if the operator knows what losing the key-encryption key costs, and this route
//! never pretends otherwise — `GET` returns `operator_key.source` so the panel can say *where*
//! the key came from, and a missing key is a `503 operator_key_missing` with the exact variable
//! names to set, not a silent default.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_secrets::store::{self, RewrapJob, RootKeyRow};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// One key in the ring, as the panel reads it. No material, no checksum.
#[derive(Debug, Serialize)]
pub struct RootKeyView {
    /// The id a stored version names.
    pub key_id: String,
    /// `active`, `retiring` or `retired`.
    pub status: String,
    /// The operator-comparable fingerprint.
    pub fingerprint: String,
    /// How many stored versions still sit on this key — the re-wrap coverage.
    pub version_count: i32,
    /// When it entered the ring.
    pub created_at: time::OffsetDateTime,
    /// When it was retired, if it was.
    pub retired_at: Option<time::OffsetDateTime>,
    /// Why it was retired.
    pub retired_reason: Option<String>,
}

impl From<RootKeyRow> for RootKeyView {
    fn from(row: RootKeyRow) -> Self {
        Self {
            key_id: row.key_id,
            status: row.status,
            fingerprint: row.fingerprint,
            version_count: row.version_count,
            created_at: row.created_at,
            retired_at: row.retired_at,
            retired_reason: row.retired_reason,
        }
    }
}

/// The seal self-check's verdict, in the panel's words.
#[derive(Debug, Serialize)]
pub struct SealReport {
    /// `true` when the operator key opens the whole ring.
    pub healthy: bool,
    /// How many keys it opened.
    pub sealed: usize,
    /// The keys it could not open, with a stable code each.
    pub unsealed: Vec<(String, String)>,
    /// Where the operator key was read from, so the panel can name it.
    pub source: Option<String>,
    /// What an operator has to do when the check fails.
    pub guidance: String,
}

/// The whole key ring state, in one read.
#[derive(Debug, Serialize)]
pub struct RootKeyState {
    /// The keys, newest first.
    pub keys: Vec<RootKeyView>,
    /// The self-check verdict.
    pub seal: SealReport,
    /// The re-wrap job that is walking the ring, if any.
    pub job: Option<RewrapJobView>,
    /// The most recent finished jobs, newest first, for the history strip.
    pub recent_jobs: Vec<RewrapJobView>,
    /// Versions still on a non-active key — what a rotation would have to walk.
    pub versions_to_rewrap: i32,
    /// `true` when the ring has an active key at all.
    pub has_active_key: bool,
}

/// The re-wrap job, as the progress bar and the buttons read it.
#[derive(Debug, Serialize)]
pub struct RewrapJobView {
    /// Job id.
    pub id: Uuid,
    /// `pending`, `running`, `paused`, `completed` or `failed`.
    pub status: String,
    /// The key being walked.
    pub from_key_id: String,
    /// The key versions move to.
    pub to_key_id: String,
    /// Versions re-wrapped so far.
    pub rewrapped_count: i32,
    /// Versions to walk in total.
    pub total_count: i32,
    /// `0..=1`.
    pub progress: f32,
    /// Where a restart resumes from, when the job is paused.
    pub resume_note: Option<String>,
    /// Why it paused.
    pub pause_reason: Option<String>,
    /// The last error, if any.
    pub last_error: Option<String>,
    /// When it started.
    pub started_at: time::OffsetDateTime,
    /// When it finished.
    pub completed_at: Option<time::OffsetDateTime>,
}

impl From<RewrapJob> for RewrapJobView {
    fn from(job: RewrapJob) -> Self {
        let progress = job.progress();
        let resume_note = job.resume_note();
        Self {
            id: job.id,
            status: job.status.clone(),
            from_key_id: job.from_key_id,
            to_key_id: job.to_key_id,
            rewrapped_count: job.rewrapped_count,
            total_count: job.total_count,
            progress,
            resume_note,
            pause_reason: job.pause_reason,
            last_error: job.last_error,
            started_at: job.started_at,
            completed_at: job.completed_at,
        }
    }
}

/// Map a secrets-crate failure onto the API surface.
///
/// A missing operator key is a `503`, not a `500`: the request is correct, the *installation* is
/// not configured, and an operator reading the response learns exactly which variables to set.
///
/// Slice 2 reuses this so a failure from the credential or slot store is rendered the same way
/// wherever it happens, and only refines one case — a self-referencing slot fallback becomes a
/// `409`, because "the fallback is the primary" is a conflict with the row, not a bad request.
pub(crate) fn map_error(error: omnion_secrets::SecretsError) -> ApiError {
    let status = match &error {
        omnion_secrets::SecretsError::OperatorKeyMissing => StatusCode::SERVICE_UNAVAILABLE,
        omnion_secrets::SecretsError::NoActiveKey | omnion_secrets::SecretsError::NotFound(_) => {
            StatusCode::NOT_FOUND
        }
        omnion_secrets::SecretsError::RotationInProgress => StatusCode::CONFLICT,
        omnion_secrets::SecretsError::Invalid(_)
        | omnion_secrets::SecretsError::ReadOnly
        | omnion_secrets::SecretsError::LeaseUnavailable(_)
        | omnion_secrets::SecretsError::DeploymentKeyUnavailable(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut mapped = ApiError::new(status, error.code(), error.to_string());
    if matches!(
        error,
        omnion_secrets::SecretsError::OperatorKeyMissing | omnion_secrets::SecretsError::Crypto
    ) {
        // The self-check's own sentence: what losing the operator key costs, said where the
        // operator is looking rather than in a log.
        mapped = mapped.with_details(json!({
            "operator_key_env": omnion_secrets::KEY_ENCRYPTION_ENV,
            "operator_key_file_env": omnion_secrets::KEY_ENCRYPTION_FILE_ENV,
            "guidance": "the root key is wrapped by this operator key and is never stored by the platform; \
                         losing it makes every locally stored secret unrecoverable",
        }));
    }
    mapped
}

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Write an audit row, and never let an audit problem fail the caller's request.
async fn audit(state: &AppState, entry: NewAuditEntry) {
    if let Err(error) = omnion_audit::entries::record(state.db().pool(), entry).await {
        tracing::warn!(error = %error, "the audit row could not be written");
    }
}

/// `GET /api/v1/secrets/root-key` — the ring, the self-check and the live job.
pub async fn read_root_key(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<RootKeyState>, ApiError> {
    let pool = state.db().pool();

    // A flip whose job row was lost leaves versions stranded with nothing walking them. The
    // read repairs it rather than showing a ring that is quietly out of date.
    if let Err(error) = store::recover_missing_job(pool).await {
        tracing::warn!(error = %error, "a lost re-wrap job could not be recovered");
    }

    let rows = store::list_root_keys(pool).await.map_err(map_error)?;
    let keys: Vec<RootKeyView> = rows.iter().cloned().map(RootKeyView::from).collect();

    let seal = match store::self_check(pool).await {
        Ok(report) => {
            let source = store::operator_key()
                .ok()
                .and_then(|key| key.source().map(str::to_owned));
            SealReport {
                healthy: report.is_healthy(),
                sealed: report.sealed.len(),
                unsealed: report
                    .unsealed
                    .iter()
                    .map(|(key_id, code)| (key_id.clone(), (*code).to_owned()))
                    .collect(),
                source,
                guidance: if report.is_healthy() {
                    "The operator key opens every key in the ring.".to_owned()
                } else {
                    "The operator key cannot open every key in the ring. A rotation is refused \
                     until this is fixed, because it would make the affected secrets unreadable."
                        .to_owned()
                },
            }
        }
        Err(error) => SealReport {
            healthy: false,
            sealed: 0,
            unsealed: Vec::new(),
            source: None,
            guidance: error.to_string(),
        },
    };

    let job = store::live_rewrap_job(pool)
        .await
        .map_err(map_error)?
        .map(Into::into);
    let recent_jobs: Vec<RewrapJobView> = sqlx::query_as::<_, RewrapJob>(
        "select id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                pause_reason, last_error, started_at, completed_at \
         from secret_rewrap_jobs where status in ('completed', 'failed') \
         order by started_at desc limit 5",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?
    .into_iter()
    .map(Into::into)
    .collect();

    let versions_to_rewrap: i32 = sqlx::query_scalar(
        "select count(*)::int from secret_versions v \
         join secret_root_keys k on k.key_id = v.key_id where k.status <> 'active'",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    let has_active_key = keys.iter().any(|key| key.status == "active");
    let _ = session;

    Ok(Json(RootKeyState {
        keys,
        seal,
        job,
        recent_jobs,
        versions_to_rewrap,
        has_active_key,
    }))
}

/// `POST /api/v1/secrets/root-key/rotate` — start the ceremony.
pub async fn rotate_root_key(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
) -> Result<(StatusCode, Json<RewrapJobView>), ApiError> {
    let pool = state.db().pool();
    let job = store::start_rotation(pool).await.map_err(map_error)?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.root_key.rotated")
            .target("root_key", job.to_key_id.clone())
            .metadata(json!({
                "job_id": job.id,
                "from_key_id": job.from_key_id,
                "to_key_id": job.to_key_id,
                "total_count": job.total_count,
            }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.root_key_rotated")
            .actor(session.user.id)
            .payload(json!({
                "job_id": job.id,
                "from_key_id": job.from_key_id,
                "to_key_id": job.to_key_id,
                "versions": job.total_count,
            })),
    )
    .await;

    Ok((StatusCode::ACCEPTED, Json(job.into())))
}

/// `GET /api/v1/secrets/root-key/rewrap-jobs/{id}` — the job's real counter.
pub async fn read_rewrap_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RewrapJobView>, ApiError> {
    let job = store::find_rewrap_job(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "rewrap_job_not_found",
                "no such re-wrap job",
            )
        })?;
    Ok(Json(job.into()))
}

/// `POST /api/v1/secrets/root-key/rewrap-jobs/{id}/pause` — pause a running walk.
pub async fn pause_rewrap_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    session: CurrentSession,
    address: ClientAddress,
) -> Result<Json<RewrapJobView>, ApiError> {
    store::pause_job(state.db().pool(), id, "paused by an operator")
        .await
        .map_err(map_error)?;
    let job = store::find_rewrap_job(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "rewrap_job_not_found",
                "no such re-wrap job",
            )
        })?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.root_key.rewrap_paused")
            .target("rewrap_job", id.to_string())
            .ip_address(address.as_text()),
    )
    .await;
    Ok(Json(job.into()))
}

/// `POST /api/v1/secrets/root-key/rewrap-jobs/{id}/resume` — pick the walk up from its cursor.
pub async fn resume_rewrap_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    session: CurrentSession,
    address: ClientAddress,
) -> Result<Json<RewrapJobView>, ApiError> {
    let job = store::resume_job(state.db().pool(), id)
        .await
        .map_err(map_error)?;
    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.root_key.rewrap_resumed")
            .target("rewrap_job", id.to_string())
            .ip_address(address.as_text()),
    )
    .await;
    Ok(Json(job.into()))
}
