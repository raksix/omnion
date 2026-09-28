//! The scanning pipeline's API (REQ-010, slice 4): policy, the sweep, the quarantine list and
//! the release.
//!
//! Five rules hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **A scanner outage never fails an upload.** The upload route does not call anything in
//!   here. The sweep is a separate, separately-failing operation, and a file whose scan could
//!   not complete is *stored* and *unserved* — the two halves of "scanning is best-effort at
//!   ingest, strict at serve" (REQ-010 *Risks*). A pipeline that propagated its own outage to
//!   the uploader would lose files the moment a scanner rebooted.
//! * **The secret is read from the environment, never from the request and never from the
//!   row.** The row stores a *name*; the process holds the value. Same rule as the storage
//!   settings, and for the same reason: a settings table that holds key material has to be
//!   scrubbed from every response and guarded as a credential.
//! * **A release is a write with a reason, an audit entry and an event.** It is the one
//!   action in the media library whose whole purpose is to put a file back into circulation
//!   after somebody decided it should not be, so it is the action that most needs a record.
//! * **The sweep is synchronous and bounded, and says so.** An operator pressing "Run now" is
//!   waiting for an answer about *their* site, and a background job that starts and returns
//!   immediately teaches them to press it again. The pass is one statement per file with a
//!   hard batch cap, and the response carries the run id so the log can be read afterwards.
//! * **A trashed file is not scanned.** Claiming rows with `deleted_at is null` means the
//!   sweep does not resurrect work for bytes on their way out, and it means a file that is
//!   restored later is `pending` again and gets scanned on the next pass.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
// The scanner client. A per-request `Client` rather than a shared one, because its timeout is
// the *site's* configured timeout: a shared client would carry whichever timeout was built
// first, so a site that raised its ceiling to 120 s would still be cut off at 30.
use reqwest::Client as HttpClient;
use omnion_media::{
    NewSiteScan, PendingScan, Quarantine, ScanRun, SiteScan,
    SweepCounts, Verdict, close_quarantine, list_quarantines, list_runs, quarantine_totals,
    read_scan_settings, write_scan_settings,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::{site_in_scope, site_of};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A site's scanning policy, as the settings screen reads it.
///
/// Built by hand from a [`SiteScan`] rather than serialised from it, for the same reason the
/// settings body is: the struct a response is built from is the place a credential leaks
/// from, and this one has nowhere to put one. The `secret_env` field is a *name*.
#[derive(Debug, Serialize)]
pub struct ScanSettingsBody {
    /// Site the policy belongs to.
    pub site_id: Uuid,
    /// Whether uploads are scanned.
    pub enabled: bool,
    /// Where the scanner lives.
    pub endpoint: String,
    /// The environment variable the shared secret is read from — a reference, never a value.
    pub secret_env: String,
    /// How long one scan may take.
    pub timeout_seconds: i32,
    /// `hold` or `serve` — what an unreachable scanner means.
    pub on_error: String,
    /// Files above this size are skipped rather than scanned.
    pub max_scan_mb: i32,
    /// Whether the process that answers the request can actually see the named variable.
    ///
    /// The *name* is stored and the *value* lives in the environment, so a site can be
    /// configured for a scanner this process cannot reach — a second API instance without the
    /// variable, or a rename that missed a deployment. The settings screen has to be able to
    /// say that, because "scanning is on and nothing is ever scanned" otherwise looks exactly
    /// like a scanner that finds nothing.
    pub secret_available: bool,
    /// A sentence about what the current policy actually does to a file.
    pub behaviour: String,
    /// How many files of this site are waiting for their first scan.
    pub pending_count: i64,
    /// Whether this row has ever been written since it was created.
    pub configured: bool,
}

impl ScanSettingsBody {
    /// Describe a stored policy for the panel.
    fn build(settings: &SiteScan, secret_available: bool, pending_count: i64) -> Self {
        let behaviour = if !settings.enabled {
            "Scanning is off: every upload is served as soon as it lands. Turn it on and point \
             it at a scanner before uploading anything you did not produce yourself."
                .to_owned()
        } else if settings.serves_on_error() {
            format!(
                "Scanning is on. A file that is clean or above the {}-MB ceiling is served; a \
                 file the scanner flags is held; and a file whose scan cannot complete is \
                 served anyway, with its row marked `error`.",
                settings.max_scan_mb
            )
        } else {
            format!(
                "Scanning is on. A file that is clean or above the {}-MB ceiling is served; a \
                 file the scanner flags is held; and a file whose scan cannot complete is NOT \
                 served until it clears.",
                settings.max_scan_mb
            )
        };
        // Same reasoning as the storage settings: `updated_at` against `created_at` is the only
        // column that distinguishes a site that was configured from one that was never
        // touched, and "every column equals the default" cannot answer that question.
        let configured = (settings.updated_at - settings.created_at).whole_seconds().abs() >= 1;
        Self {
            site_id: settings.site_id,
            enabled: settings.enabled,
            endpoint: settings.endpoint.clone(),
            secret_env: settings.secret_env.clone(),
            timeout_seconds: settings.timeout_seconds,
            on_error: settings.on_error.clone(),
            max_scan_mb: settings.max_scan_mb,
            secret_available,
            behaviour,
            pending_count,
            configured,
        }
    }
}

/// A scanning policy as the caller describes it.
#[derive(Debug, Default, Deserialize)]
pub struct ScanSettingsInput {
    /// Whether uploads are scanned.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Where the scanner lives.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The environment variable the shared secret is read from.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// How long one scan may take.
    #[serde(default)]
    pub timeout_seconds: Option<i32>,
    /// What an unreachable scanner means.
    #[serde(default)]
    pub on_error: Option<String>,
    /// Files above this size are skipped.
    #[serde(default)]
    pub max_scan_mb: Option<i32>,
}

impl ScanSettingsInput {
    /// Reduce the request into the crate's own type, where validation happens.
    fn into_new(self) -> NewSiteScan {
        NewSiteScan {
            enabled: self.enabled,
            endpoint: self.endpoint,
            secret_env: self.secret_env,
            timeout_seconds: self.timeout_seconds,
            on_error: self.on_error,
            max_scan_mb: self.max_scan_mb,
        }
    }
}

/// One quarantine, as the list and the detail screen read it.
#[derive(Debug, Serialize)]
pub struct QuarantineBody {
    /// Quarantine id — what a release names.
    pub id: Uuid,
    /// The held file.
    pub media_id: Uuid,
    /// What the scanner said, in its own words.
    pub detail: String,
    /// When the file was held.
    pub quarantined_at: OffsetDateTime,
    /// The run that produced it, when it came from one.
    pub run_id: Option<Uuid>,
}

impl QuarantineBody {
    /// Describe a quarantine for the panel.
    fn build(row: &Quarantine) -> Self {
        Self {
            id: row.id,
            media_id: row.media_id,
            detail: row.detail.clone(),
            quarantined_at: row.quarantined_at,
            run_id: row.run_id,
        }
    }
}

/// The quarantine list of a site, with its totals.
#[derive(Debug, Serialize)]
pub struct QuarantineResponse {
    /// Site the list belongs to.
    pub site_id: Uuid,
    /// How many files are held.
    pub file_count: i64,
    /// How many bytes they occupy — a held icon and a held video are different urgencies.
    pub total_bytes: i64,
    /// The open quarantines, newest first.
    pub entries: Vec<QuarantineBody>,
}

/// One run, as the log reads it.
#[derive(Debug, Serialize)]
pub struct ScanRunBody {
    /// Run id.
    pub id: Uuid,
    /// `scan`, `rescan` or `manual`.
    pub kind: String,
    /// `clean`, `flagged` or `error`.
    pub outcome: String,
    /// Files whose status was written.
    pub scanned: i64,
    /// Files the scanner flagged.
    pub flagged: i64,
    /// Files whose scan could not complete.
    pub errors: i64,
    /// Files above the size ceiling, which nobody looked at.
    pub skipped: i64,
    /// The endpoint the client used.
    pub endpoint: String,
    /// The engine the scanner named.
    pub engine: String,
    /// When the run started.
    pub started_at: OffsetDateTime,
    /// When it finished; null while it is still running.
    pub finished_at: Option<OffsetDateTime>,
    /// One sentence — never a number without a word.
    pub summary: String,
}

impl ScanRunBody {
    /// Describe a run for the panel.
    fn build(run: &ScanRun) -> Self {
        Self {
            id: run.id,
            kind: run.kind.clone(),
            outcome: run.outcome.clone(),
            scanned: run.scanned,
            flagged: run.flagged,
            errors: run.errors,
            skipped: run.skipped,
            endpoint: run.endpoint.clone(),
            engine: run.engine.clone(),
            started_at: run.started_at,
            finished_at: run.finished_at,
            summary: run.summary(),
        }
    }
}

/// The run log of a site.
#[derive(Debug, Serialize)]
pub struct ScanRunList {
    /// Site the log belongs to.
    pub site_id: Uuid,
    /// The most recent runs, newest first.
    pub runs: Vec<ScanRunBody>,
}

/// The answer to "run the sweep now".
#[derive(Debug, Serialize)]
pub struct SweepResponse {
    /// The run that was written — so the operator can open it in the log.
    pub run_id: Uuid,
    /// What the pass found, in numbers.
    pub scanned: i64,
    /// Files the scanner flagged.
    pub flagged: i64,
    /// Files whose scan could not complete.
    pub errors: i64,
    /// Files above the size ceiling.
    pub skipped: i64,
    /// `clean`, `flagged` or `error`.
    pub outcome: String,
    /// One sentence, which is what a person actually reads.
    pub summary: String,
    /// The quarantines that are open now, with their totals.
    pub quarantine: QuarantineResponse,
}

/// A release or a deletion of a held file.
#[derive(Debug, Deserialize)]
pub struct ReleaseInput {
    /// Why the file is being let go. Required: the quarantine row keeps it.
    pub reason: String,
}

/// The site a media route acts on.
#[derive(Debug, Deserialize)]
pub struct SiteScanQuery {
    /// Site whose library is addressed.
    pub site_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Handlers — the policy
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/scan-settings` — a site's scanning policy.
pub async fn read(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteScanQuery>,
) -> std::result::Result<Json<ScanSettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let settings = read_scan_settings(state.db().pool(), site.id).await?;
    let pending = pending_count(state.db().pool(), site.id).await?;
    Ok(Json(ScanSettingsBody::build(
        &settings,
        secret_is_available(&settings),
        pending,
    )))
}

/// `PUT /api/v1/media/scan-settings` — save a site's scanning policy.
pub async fn write(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteScanQuery>,
    Json(input): Json<ScanSettingsInput>,
) -> std::result::Result<Json<ScanSettingsBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let before = read_scan_settings(state.db().pool(), site.id).await?;
    let after = write_scan_settings(state.db().pool(), site.id, input.into_new()).await?;
    let pending = pending_count(state.db().pool(), site.id).await?;

    // Turning scanning on or off is a security decision about who may read what, so it is
    // audited with the fields that actually moved — "scanning was enabled" is useless
    // without the endpoint it was enabled against.
    if before.enabled != after.enabled
        || before.endpoint != after.endpoint
        || before.on_error != after.on_error
        || before.max_scan_mb != after.max_scan_mb
        || before.timeout_seconds != after.timeout_seconds
    {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "media.scan_settings_changed")
                .target("site", site.id.to_string())
                .metadata(json!({
                    "site_id": site.id,
                    "enabled": after.enabled,
                    "endpoint": after.endpoint,
                    "on_error": after.on_error,
                    "max_scan_mb": after.max_scan_mb,
                    "timeout_seconds": after.timeout_seconds,
                }))
                .ip_address(address.as_text())
                .organization(site.organization_id),
        )
        .await?;
    }

    Ok(Json(ScanSettingsBody::build(
        &after,
        secret_is_available(&after),
        pending,
    )))
}

// ---------------------------------------------------------------------------------------------
// Handlers — the sweep
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/media/scan/run` — sweep a site's pending files now.
pub async fn run_now(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteScanQuery>,
) -> std::result::Result<Json<SweepResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let settings = read_scan_settings(state.db().pool(), site.id).await?;

    if !settings.enabled {
        return Err(ApiError::bad_request(
            "scanning_disabled",
            "scanning is off for this site — turn it on and give it a scanner endpoint before \
             running a sweep",
        ));
    }

    let claimed = omnion_media::claim_pending(state.db().pool(), site.id, omnion_media::scanning::SWEEP_BATCH)
        .await?;
    let run_id =
        omnion_media::begin_run(state.db().pool(), site.id, "manual", &settings.endpoint, "", Some(current.user.id))
            .await?;

    let mut counts = SweepCounts::default();
    let mut engine = String::new();
    for file in &claimed {
        let verdict = scan_one(&state, &settings, file).await;
        engine = {
            let found = verdict.engine().to_owned();
            if found.is_empty() { engine } else { found }
        };
        // The verdict is counted **only if it could be written**. A file whose verdict the
        // database refused is not an `error` verdict and an extra `error` at the same time:
        // the first version of this counted both, and a run over a single file whose write
        // failed reported `errors = 2` — a number with no reading an operator can act on,
        // because "two scans failed" is not what happened.
        let written =
            omnion_media::apply_verdict(state.db().pool(), site.id, file, &verdict, Some(run_id), Some(current.user.id))
                .await;
        if let Err(error) = &written {
            // One file that cannot be written its status must not abandon the rest of the
            // batch: the run is logged with what did happen, and the failure is named rather
            // than swallowed.
            tracing::warn!(
                error = %error,
                media_id = %file.media_id,
                "the scan verdict could not be written"
            );
            counts.errors += 1;
            continue;
        }
        match &verdict {
            Verdict::Flagged { .. } => counts.flagged += 1,
            Verdict::Skipped { .. } => counts.skipped += 1,
            Verdict::Error { .. } => counts.errors += 1,
            Verdict::Clean { .. } => {}
        }
        if matches!(verdict, Verdict::Clean { .. } | Verdict::Flagged { .. }) {
            counts.scanned += 1;
        }
    }
    omnion_media::finish_run(state.db().pool(), run_id, &counts).await?;

    if counts.flagged > 0 {
        bus::emit(
            state.db().pool(),
            NewEvent::new("media.scan_flagged")
                .organization(site.organization_id)
                .site(site.id)
                .actor(current.user.id)
                .payload(json!({
                    "site_id": site.id,
                    "flagged": counts.flagged,
                    "run_id": run_id,
                })),
        )
        .await?;
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "media.scan_flagged")
                .target("site", site.id.to_string())
                .metadata(json!({ "site_id": site.id, "flagged": counts.flagged, "run_id": run_id }))
                .ip_address(address.as_text())
                .organization(site.organization_id),
        )
        .await?;
    }

    let (file_count, total_bytes) = quarantine_totals(state.db().pool(), site.id).await?;
    let entries = list_quarantines(state.db().pool(), site.id).await?;
    Ok(Json(SweepResponse {
        run_id,
        scanned: counts.scanned,
        flagged: counts.flagged,
        errors: counts.errors,
        skipped: counts.skipped,
        outcome: counts.outcome().to_owned(),
        summary: format!(
            "{} file(s) scanned, {} flagged, {} could not be scanned, {} above the ceiling",
            counts.scanned, counts.flagged, counts.errors, counts.skipped
        ),
        quarantine: QuarantineResponse {
            site_id: site.id,
            file_count,
            total_bytes,
            entries: entries.iter().map(QuarantineBody::build).collect(),
        },
    }))
}

/// `GET /api/v1/media/scan/runs` — the run log of a site.
pub async fn runs(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteScanQuery>,
) -> std::result::Result<Json<ScanRunList>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let runs = list_runs(state.db().pool(), site.id, 25).await?;
    Ok(Json(ScanRunList {
        site_id: site.id,
        runs: runs.iter().map(ScanRunBody::build).collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Handlers — the quarantine
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/quarantine` — the open quarantines of a site.
pub async fn list_held(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteScanQuery>,
) -> std::result::Result<Json<QuarantineResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let (file_count, total_bytes) = quarantine_totals(state.db().pool(), site.id).await?;
    let entries = list_quarantines(state.db().pool(), site.id).await?;
    Ok(Json(QuarantineResponse {
        site_id: site.id,
        file_count,
        total_bytes,
        entries: entries.iter().map(QuarantineBody::build).collect(),
    }))
}

/// `POST /api/v1/media/quarantine/{id}/release` — let a held file go back into circulation.
///
/// The one action in the media library whose purpose is to undo a safety decision, so it
/// carries a reason, an audit entry and an event. A release with no stated reason is refused
/// by the crate before any row is written.
pub async fn release(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(quarantine_id): Path<Uuid>,
    Json(input): Json<ReleaseInput>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    // The row is read **and scoped** in one step. Reading it unscoped and then calling
    // `site_in_scope` turned "a quarantine belonging to another tenant" into a
    // `cross_organization` `403` — which confirms the id exists, and a `403` is exactly the
    // difference between "that is not yours" and "that is not real". A `404` does not leak
    // the existence of a row the caller may not see, and the walk that caught this asserts
    // the status rather than the body.
    let existing = find_quarantine_in_scope(&state, &current, quarantine_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "quarantine_not_found",
                "no such quarantine",
            )
        })?;
    let site = site_of(&state, existing.site_id).await?;

    let closed = close_quarantine(pool, quarantine_id, current.user.id, &input.reason)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "quarantine_already_released",
                "this quarantine was already released or deleted",
            )
        })?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.scan_released")
            .target("media", closed.media_id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "quarantine_id": quarantine_id,
                "media_id": closed.media_id,
                "reason": closed.release_reason,
                "detail": closed.detail,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(json!({ "released": true, "reason": closed.release_reason })))
}

/// `POST /api/v1/media/scan/test` — prove a scanner answers, against the posted candidate.
///
/// Same rule as the storage connection test: the operator has unsaved edits in front of
/// them, and a result about the *saved* row is a result about something they are no longer
/// looking at. The candidate is validated by the same function a save validates with.
pub async fn test_scanner(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteScanQuery>,
    Json(input): Json<ScanSettingsInput>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let stored = read_scan_settings(state.db().pool(), site.id).await?;
    let candidate = input.into_new();
    let validated = omnion_media::scanning::validate_scan_settings(NewSiteScan {
        enabled: candidate.enabled.or(Some(stored.enabled)),
        endpoint: candidate.endpoint.or(Some(stored.endpoint.clone())),
        secret_env: candidate.secret_env.or(Some(stored.secret_env.clone())),
        timeout_seconds: candidate.timeout_seconds.or(Some(stored.timeout_seconds)),
        on_error: candidate.on_error.or(Some(stored.on_error.clone())),
        max_scan_mb: candidate.max_scan_mb.or(Some(stored.max_scan_mb)),
    })?;

    if !validated.enabled.unwrap_or(false) {
        return Ok(Json(json!({
            "ok": false,
            "detail": "turn scanning on and give it an endpoint before testing it",
        })));
    }

    let settings = SiteScan {
        site_id: site.id,
        enabled: validated.enabled.unwrap_or(false),
        endpoint: validated.endpoint.unwrap_or_default(),
        secret_env: validated.secret_env.unwrap_or_default(),
        timeout_seconds: validated.timeout_seconds.unwrap_or(30),
        on_error: validated.on_error.unwrap_or_else(|| "hold".to_owned()),
        max_scan_mb: validated.max_scan_mb.unwrap_or(100),
        created_at: stored.created_at,
        updated_at: stored.updated_at,
    };

    // The probe is a *real* scan of a real, tiny, generated payload rather than a health
    // endpoint: a scanner that answers `/health` and refuses actual bytes is exactly the
    // configuration that produces a library where every file reads `error`.
    let probe = omnion_media::PendingScan {
        media_id: Uuid::new_v4(),
        storage_key: String::new(),
        filename: "omnion-scan-probe.txt".to_owned(),
        content_type: "text/plain".to_owned(),
        size_bytes: SCAN_PROBE.len() as i64,
        checksum: omnion_media::scan_identity("omnion-scan-probe"),
        site_id: site.id,
    };
    let verdict = post_scan(&state, &settings, &probe, SCAN_PROBE).await;
    Ok(Json(json!({
        "ok": matches!(verdict, Verdict::Clean { .. }),
        "status": verdict.status(),
        "detail": verdict.detail(),
        "engine": verdict.engine(),
        "note": "the probe posts a real, generated text payload; `skipped` or `error` means the \
                 scanner will not accept this site's files either",
    })))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The bytes the scanner probe posts.
///
/// Generated, not a stored file: a probe that reuses a real upload would report on whatever
/// that upload happened to contain, and a probe that stores its payload leaves an object
/// behind for every click.
const SCAN_PROBE: &[u8] = b"omnion media scanning probe\n";

/// How many files of a site are still waiting for their first scan.
async fn pending_count(pool: &sqlx::PgPool, site_id: Uuid) -> std::result::Result<i64, ApiError> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from media where site_id = $1 and scan_status = 'pending' \
           and deleted_at is null",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await
    .map_err(|error| ApiError::from(omnion_media::MediaError::Database(error)))?;
    Ok(row.0)
}

/// Whether the process answering this request can see the named secret variable.
///
/// Reported, never enforced: a *second* API instance may legitimately lack the variable
/// while the worker that does the scanning has it, so refusing the configuration would break
/// a working deployment. The screen says so instead, because "scanning is on and nothing is
/// ever scanned" otherwise looks exactly like a scanner that finds nothing.
fn secret_is_available(settings: &SiteScan) -> bool {
    settings.secret_env.is_empty() || std::env::var(&settings.secret_env).is_ok()
}

/// Scan one file and return the verdict, reading the bytes from the object store.
async fn scan_one(state: &AppState, settings: &SiteScan, file: &PendingScan) -> Verdict {
    // A file above the ceiling is never read from the store: the point of the ceiling is that
    // the *transfer* is what is expensive, and fetching 400 MB to decide not to send it is
    // the wrong order.
    let size = file.size_bytes.max(0) as u64;
    if !settings.scans_size(size) {
        return Verdict::Skipped {
            limit_mb: settings.max_scan_mb,
        };
    }
    let bytes = match state.storage().get(&file.storage_key).await {
        Ok(bytes) => bytes,
        Err(error) => {
            // The *object* is missing, which is a different failure from the scanner being
            // down: the file is stored and unservable either way, and the detail says which,
            // because "the scanner is down" would send an operator to the wrong machine.
            return Verdict::Error {
                detail: format!(
                    "the file's bytes could not be read from the object store: {error}"
                ),
            };
        }
    };
    post_scan(state, settings, file, &bytes).await
}

/// Post one file to the scanner and reduce the answer to a verdict.
///
/// Every failure mode here — no endpoint, a refused connection, a timeout, a non-JSON body —
/// becomes a [`Verdict::Error`] rather than an `Err`. That is the whole point of the ingest
/// rule: the file is already stored, and the upload has already been acknowledged, so the
/// only thing left to record is that the scan did not complete.
async fn post_scan(
    _state: &AppState,
    settings: &SiteScan,
    file: &PendingScan,
    bytes: &[u8],
) -> Verdict {
    if settings.endpoint.is_empty() {
        return Verdict::Error {
            detail: "no scanner endpoint is configured for this site".to_owned(),
        };
    }

    let timeout = std::time::Duration::from_secs(settings.timeout_seconds.max(1) as u64);
    let client = match HttpClient::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(error) => {
            return Verdict::Error {
                detail: format!("the scanner client could not be built: {error}"),
            };
        }
    };

    // The wire shape is the platform's, not a vendor's: a checksum identity, the name and
    // type as headers, and the bytes as the body. Anything that answers this shape can sit
    // behind the endpoint, which is what keeps the engine out of the platform.
    let mut request = client
        .post(format!("{}/scan", settings.endpoint.trim_end_matches('/')))
        .header("x-omnion-media-id", file.media_id.to_string())
        .header("x-omnion-checksum", file.checksum.as_str())
        .header("x-omnion-filename", sanitize_header(&file.filename))
        .header("x-omnion-content-type", file.content_type.as_str())
        .header("content-type", "application/octet-stream")
        .body(bytes.to_vec());

    // The secret is read from the process environment by the *name* the row holds. A request
    // that carries no secret is still sent, because plenty of scanners sit on a private
    // network — refusing to probe an unauthenticated scanner would be the wrong default.
    if !settings.secret_env.is_empty() {
        if let Ok(secret) = std::env::var(&settings.secret_env) {
            request = request.header("authorization", format!("Bearer {secret}"));
        }
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if !status.is_success() {
                // A non-2xx is the scanner saying it could not do the job. The status and the
                // body are both recorded, truncated: "502 Bad Gateway" alone sends an operator
                // hunting a proxy, and the proxy's own body names what it was proxying to.
                let body = response.text().await.unwrap_or_default();
                return Verdict::Error {
                    detail: format!(
                        "the scanner answered {}: {}",
                        status.as_u16(),
                        truncate(&body, 200)
                    ),
                };
            }
            match response.bytes().await {
                Ok(body) => match omnion_media::parse_response(body.as_ref()) {
                    Ok(parsed) => omnion_media::interpret(parsed, ""),
                    // An answer the platform cannot read is an error and never a pass — the
                    // same rule `interpret` applies to an unknown status, applied one layer
                    // earlier, so a scanner that swapped its API shape cannot silently pass
                    // every file it used to flag.
                    Err(reason) => Verdict::Error { detail: reason },
                },
                Err(error) => Verdict::Error {
                    detail: format!("the scanner's answer could not be read: {error}"),
                },
            }
        }
        Err(error) => Verdict::Error {
            detail: format!("the scanner could not be reached: {error}"),
        },
    }
}

/// Make a file name safe to put in a header value.
///
/// A file name may hold anything the uploader typed, and a newline in a header is a request
/// the client library will refuse — turning a bad name into an `error` verdict for a file the
/// scanner would have been happy to look at. The identity is the checksum, so the name is
/// context only and a lossy reduction loses nothing.
fn sanitize_header(value: &str) -> String {
    // Printable ASCII plus the space. A space is legal in a header value and dropping it
    // would mangle every file name somebody typed with one; a control character is not, and
    // `"` is legal but quoting a value that is never parsed as a quoted-string is a way to
    // confuse the next person reading the scanner's log.
    let cleaned: String = value
        .chars()
        .filter(|c| (c.is_ascii_graphic() || *c == ' ') && *c != '"')
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "upload".to_owned()
    } else {
        cleaned
    }
}

/// Reduce a scanner's body to something a row can hold.
fn truncate(value: &str, limit: usize) -> String {
    let flattened: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return "no detail".to_owned();
    }
    trimmed.chars().take(limit).collect()
}

/// Load one quarantine row only if its site is inside the caller's organization.
///
/// The organization check is a `where` clause rather than a second lookup on purpose: a
/// caller who may not see a row must get the *same* answer as a caller for whom it does not
/// exist, and doing the check afterwards turns the first into the second with a status code
/// that says "forbidden" instead of "not found".
async fn find_quarantine_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> std::result::Result<Option<Quarantine>, ApiError> {
    let row = sqlx::query_as::<_, Quarantine>(
        "select q.id, q.media_id, q.site_id, q.detail, q.run_id, q.quarantined_at, \
                q.quarantined_by, q.released_at, q.released_by, q.release_reason \
           from media_quarantines q \
           join sites s on s.id = q.site_id \
          where q.id = $1 \
            and (s.organization_id is not distinct from $2 or $2 is null)",
    )
    .bind(id)
    .bind(current.user.organization_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_media::MediaError::Database(error)))?;
    Ok(row)
}

/// Write an audit entry.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_media::ServeRefusal;

    fn policy(enabled: bool, endpoint: &str, on_error: &str) -> SiteScan {
        SiteScan {
            site_id: Uuid::nil(),
            enabled,
            endpoint: endpoint.to_owned(),
            secret_env: String::new(),
            timeout_seconds: 30,
            on_error: on_error.to_owned(),
            max_scan_mb: 100,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// The behaviour sentence is what an operator actually reads, so each policy gets its
    /// own and none of them is a bare number.
    #[test]
    fn the_behaviour_sentence_follows_the_policy() {
        let off = ScanSettingsBody::build(&policy(false, "", "hold"), true, 0);
        assert!(off.behaviour.contains("Scanning is off"));
        assert!(!off.configured, "two equal timestamps is an untouched row");

        let hold = ScanSettingsBody::build(&policy(true, "http://s:1", "hold"), true, 3);
        assert!(hold.behaviour.contains("NOT"));
        assert!(hold.behaviour.contains("100-MB"), "the ceiling is named");
        assert_eq!(hold.pending_count, 3);

        let serve = ScanSettingsBody::build(&policy(true, "http://s:1", "serve"), false, 0);
        assert!(serve.behaviour.contains("served anyway"));
        assert!(!serve.secret_available);
    }

    /// A configured row is distinguishable from one the trigger inserted.
    #[test]
    fn a_saved_row_is_marked_configured() {
        let mut saved = policy(true, "http://s:1", "hold");
        saved.updated_at = saved.created_at + time::Duration::seconds(5);
        assert!(ScanSettingsBody::build(&saved, true, 0).configured);
    }

    /// A file name with a newline or a quote cannot become a header injection or a refusal.
    #[test]
    fn a_file_name_is_made_safe_for_a_header() {
        assert_eq!(sanitize_header("hero.png"), "hero.png");
        // Control characters are *dropped*, not replaced with a space: a `\r\n` in a name is
        // either a header injection or a client-library refusal, and both are worse than a
        // name that lost two invisible characters. The identity is the checksum, so nothing
        // downstream depends on the exact spelling.
        assert_eq!(
            sanitize_header("bad\r\nx-injected: 1"),
            "badx-injected: 1",
            "the CRLF is gone, the rest of the name is not"
        );
        assert_eq!(sanitize_header("Q3 report.pdf"), "Q3 report.pdf", "a space survives");
        assert_eq!(sanitize_header("with\"quote.png"), "withquote.png");
        assert_eq!(sanitize_header(""), "upload");
        // A name that is *only* characters the filter drops falls back rather than sending
        // an empty header — some clients refuse an empty value outright.
        assert_eq!(sanitize_header("\u{1F4C8}"), "upload");
        let long: String = "n".repeat(500);
        assert_eq!(sanitize_header(&long).chars().count(), 200);
    }

    /// A scanner's error body is flattened and bounded before it reaches a row.
    #[test]
    fn a_scanner_body_is_flattened_and_bounded() {
        assert_eq!(truncate("line one\nline two", 100), "line one line two");
        assert_eq!(truncate("   ", 100), "no detail");
        assert_eq!(truncate(&"x".repeat(500), 50).chars().count(), 50);
    }

    /// The four serve refusals reach the caller as four codes, so a client can tell a
    /// quarantined file from a file whose scan merely has not run.
    #[test]
    fn every_serve_refusal_has_a_code_the_client_can_branch_on() {
        let mut codes: Vec<&str> = [
            ServeRefusal::Trashed,
            ServeRefusal::Quarantined,
            ServeRefusal::NotScanned,
            ServeRefusal::ScanFailed,
        ]
        .iter()
        .map(ServeRefusal::code)
        .collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 4);
    }
}
