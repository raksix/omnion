use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// The two readers are imported because this module is the one place that now decides what
// the destination actually holds. `record` and `take_safety_backup` are called through
// `super::backups::` below, deliberately NOT imported: they are the audit and the safety
// producer the *other* two restore paths share, and a private function reached by import
// invites a second, worse copy of the safety backup to be written here instead.
use super::backups::{media_index_keys, read_artifacts};

/// What a rebuild of the preview found, and whether the price is worth anything.
///
/// The flag is the whole reason this is a struct rather than a bare `RestorePreview`. The
/// preview and the restore were two copies of the same forty lines, and they disagreed at
/// exactly one point: when the live media library could not be counted, the **preview**
/// substituted zeros and rendered a reassuring "you lose nothing", while the **restore**
/// refused. Both were defensible in isolation and together they are the defect — the operator
/// is shown a price of zero by the screen and then pays a real one, or is shown a real one
/// and then gets a refusal. One function, one answer, and the two callers each decide what
/// to *do* about a fact the function only *reports*.
pub struct PricedPreview {
    /// The preview, built from re-read artifacts and live counts.
    pub preview: omnion_backup::RestorePreview,
    /// `true` when the live media side could not be counted, so the loss number is unknown
    /// rather than zero.
    pub live_comparison_failed: bool,
    /// The archive's own media index, when it could be read.
    ///
    /// **Carried rather than re-read.** The synchronous restore needs the object count, the
    /// byte total and the per-site list out of this one index read, and reading it a second
    /// time would mean a second answer to "what did the archive copy" — a question whose
    /// answer must not differ between the check the operator reads and the work that runs.
    pub media_objects: Option<Vec<omnion_backup::CopiedObject>>,
}

/// Rebuild the preview for a run: re-read every artifact, count the live side, price it.
///
/// **The single implementation.** The preview route, the synchronous restore and the queued
/// restore's worker all call this. The reason is the same one that made `read_artifacts` a
/// shared function in slice 2b: the restore *writes* live data, and a restore that read the
/// artifacts with slightly different rules than the screen would offer parts the preview
/// refused, or the reverse — the operator sees one price and pays another.
pub async fn rebuild_preview(
    state: &AppState,
    row: &omnion_backup::Backup,
    settings: &omnion_backup::BackupSettings,
) -> PricedPreview {
    let pool = state.db().pool();
    let org = row.organization_id;
    let manifest = omnion_backup::manifest_of(row);
    let mut evidence = read_artifacts(&manifest, settings).await;

    // The media index is a separate file from the media artifact, and it is the only place
    // the archive's own list of storage keys exists. Without it the preview can count what
    // the live library holds but has nothing to compare it against, and would report every
    // live object as "dropped" — the most alarming possible false positive on a screen whose
    // whole job is being believed.
    let mut media_objects: Option<Vec<omnion_backup::CopiedObject>> = None;
    let mut archive_keys: Vec<String> = Vec::new();
    let media_readable = evidence
        .iter()
        .any(|item| item.part.part == "media" && item.artifact.is_ok());
    if media_readable {
        if let Some(objects) = media_index_keys(row, settings).await {
            archive_keys = objects
                .iter()
                .map(|object| object.storage_key.clone())
                .collect();
            media_objects = Some(objects);
        }
    }

    let mut comparison_failed = false;
    for item in &mut evidence {
        item.live = if item.part.part == "media" && !archive_keys.is_empty() {
            match omnion_backup::compare_media(pool, org, &archive_keys).await {
                Ok(comparison) => comparison.counts(),
                Err(error) => {
                    // Recorded, not substituted. The caller decides whether an unpriced
                    // restore is a screen that says so or a refusal, and neither decision is
                    // this function's to make.
                    tracing::warn!(%error, "the restore preview could not compare live media");
                    comparison_failed = true;
                    omnion_backup::LiveCounts::default()
                }
            }
        } else if item.part.part == "database" {
            match omnion_backup::compare_database(pool, item.part.item_count, org).await {
                Ok(comparison) => comparison.counts(),
                Err(error) => {
                    tracing::warn!(%error, "the restore preview could not count live rows");
                    omnion_backup::LiveCounts::default()
                }
            }
        } else {
            omnion_backup::LiveCounts::default()
        };
    }

    PricedPreview {
        preview: omnion_backup::build_preview(
            &row.id.to_string(),
            &row.label,
            &manifest,
            &evidence,
            row.finished_at.map(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            }),
            time::OffsetDateTime::now_utc().unix_timestamp(),
        ),
        live_comparison_failed: comparison_failed,
        media_objects,
    }
}

/// The reason a caller refuses when the live side could not be counted.
///
/// One sentence, named for both callers, because the restore's refusal and the queued
/// restore's failure are the same event seen a minute apart — and a panel that shows one
/// wording on the button and another on the worker line leaves an operator deciding whether
/// they are two problems.
pub const UNPRICED: &str = "the live media library could not be compared with this archive, so \
                            the restore refuses to price itself. Nothing was changed.";

/// `POST /api/v1/backups/{id}/restore-queue` — the operator asks for a restore that can still
/// be stopped.
///
/// The route behind `backup.restore`, like the synchronous one, and behind **nothing else**:
/// queueing a restore is a promise that live data will be overwritten, and a caller who may
/// not press the button must not be able to line it up either.
///
/// It returns **`202` with the queued job**, not the restore's outcome. A `200` with an
/// outcome would be a lie about a request that has written nothing yet, and a `200` with
/// nothing would be a row a panel cannot render. The queued job is the answer.
pub async fn queue_restore(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(body): Json<QueueRestoreBody>,
) -> std::result::Result<(StatusCode, Json<RestoreJobBody>), ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();

    // The run, through the same scoped read the preview uses, so a stranger's run is a 404
    // that does not name the tenancy rule.
    let row = omnion_backup::find_backup(pool, id, org).await?;
    let settings = omnion_backup::load_settings(pool).await?;

    // The preview, rebuilt **now** rather than at execution time, for two reasons: the
    // operator is reading it on this request, and a job queued against a run that stopped
    // being restorable is a job the worker will refuse a minute later with a worse message
    // than the one that could have been given here.
    let priced = rebuild_preview(&state, &row, &settings).await;
    if priced.live_comparison_failed {
        return Err(ApiError::bad_request("live_comparison_unavailable", UNPRICED));
    }
    if !priced.preview.restorable {
        return Err(ApiError::bad_request(
            "run_not_restorable",
            "this run cannot be restored: an artifact could not be re-read off the destination, \
             so there is nothing to restore from and no phrase to confirm. Nothing was queued.",
        ));
    }
    if !priced.preview.accepts(&body.confirmation) {
        return Err(ApiError::bad_request(
            "confirmation_mismatch",
            format!(
                "this run's confirmation phrase is {} and what you sent does not match it. \
                 Nothing was queued — a refused restore takes no safety backup and no row.",
                priced.preview.confirm_phrase
            ),
        ));
    }

    // An unknown part name is refused rather than dropped, and the selection is narrowed to
    // the run's own manifest order — the same two rules the synchronous route follows, for
    // the same reason: a request for `["media", "typo"]` that quietly restores `media` is a
    // restore that did less than it was asked and reported success.
    let known: Vec<String> = priced
        .preview
        .available_parts()
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
    for part in &body.parts {
        if !omnion_backup::PARTS.contains(&part.as_str()) {
            return Err(ApiError::bad_request(
                "unknown_part",
                format!(
                    "{part} is not one of the five parts a backup can produce. The selection was \
                     {}. Nothing was queued.",
                    omnion_backup::PARTS.join(", ")
                ),
            ));
        }
        if !known.contains(part) {
            return Err(ApiError::bad_request(
                "part_not_in_run",
                format!(
                    "this run cannot offer its {part} part — it either never produced one or \
                     the artifact could not be re-read. The parts it offers are {}. Nothing \
                     was queued.",
                    if known.is_empty() {
                        "none".to_owned()
                    } else {
                        known.join(", ")
                    }
                ),
            ));
        }
    }
    if body.parts.is_empty() {
        return Err(ApiError::bad_request(
            "nothing_selected",
            format!(
                "no parts were selected, so there is nothing to restore. This run offers {}.",
                known.join(", ")
            ),
        ));
    }

    // `database` is refused by name, here as in the synchronous route: it records a row count
    // per table, which is an inventory and not a dump.
    if body.parts.iter().any(|part| part == "database") {
        return Err(ApiError::bad_request(
            "part_not_restorable",
            "the database part records a row count per table, not a dump of those rows, so \
             restoring it would replace the platform's schema with an inventory of it. Deselect \
             `database` and restore the rest of the run. Nothing was queued.",
        ));
    }

    let job = omnion_backup::restore_jobs::queue_restore(
        pool,
        &omnion_backup::restore_jobs::NewRestoreJob {
            organization_id: org,
            backup_id: id,
            parts: body.parts.clone(),
            confirmation: body.confirmation.clone(),
            // **Carried from the preview the operator just read**, not recomputed. A job
            // re-priced at execution time would restore against today's library while the
            // operator agreed to yesterday's number, and the audit entry would name a loss
            // nobody agreed to.
            live_dropped: priced.preview.total_live_dropped,
            live_matches: priced.preview.total_live_matches,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(|error| match error {
        omnion_backup::restore_jobs::JobError::AlreadyLive(state_name) => ApiError::bad_request(
            "restore_already_queued",
            format!(
                "this run already has a restore that is {state_name}. Wait for it to finish, or \
                 cancel it, before queueing another — two restores of the same archive would \
                 take two safety backups and write every object twice."
            ),
        ),
        omnion_backup::restore_jobs::JobError::Unavailable(reason) => {
            tracing::warn!(%reason, "a queued restore could not be recorded");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "restore_queue_failed",
                format!("the restore could not be queued: {reason}"),
            )
        }
        other => ApiError::bad_request("restore_not_queued", other.to_string()),
    })?;

    super::backups::record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.restore.queued",
        id.to_string(),
        json!({
            "job_id": job.id,
            "parts": job.parts,
            "live_dropped": job.live_dropped,
            "live_matches": job.live_matches,
        }),
    )
    .await;

    Ok((StatusCode::ACCEPTED, Json(RestoreJobBody::from_row(&job))))
}

/// `GET /api/v1/backups/{id}/restore-jobs` — this run's queued and finished restores.
///
/// Behind `backup.read`, like the preview: reading that a restore is queued changes nothing,
/// and gating the *list* behind `backup.restore` means an operator's first sight of the
/// thing they are waiting for is a 403.
pub async fn list_jobs(
    state: State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<Vec<RestoreJobBody>>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    // Scoped by the same statement that reads the run, so a stranger's run is a 404.
    omnion_backup::find_backup(pool, id, org).await?;
    let jobs = omnion_backup::restore_jobs::list_restore_jobs(pool, id, org).await?;
    Ok(Json(jobs.iter().map(RestoreJobBody::from_row).collect()))
}

/// `POST /api/v1/restore-jobs/{id}/cancel` — stop a queued restore.
///
/// Behind `backup.restore`: cancelling is the same authority as pressing the button, and a
/// permission that lets an operator *un*press something they could never press is not a
/// permission, it is a way to deny the service to somebody who can see the job.
///
/// **It records an intent, not an outcome.** The response is the job as it now reads, so a
/// caller that lost the race to the worker sees `running` in the body rather than a `200`
/// that claims a cancellation that did not happen. A cancel that lied about winning the race
/// is how an operator believes a restore stopped and walks away from a library that is
/// already half replaced.
pub async fn cancel_job(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<RestoreJobBody>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();

    let job = omnion_backup::restore_jobs::find_restore_job(pool, id, org)
        .await
        .map_err(|_| not_found())?;

    if job.status != "queued" {
        return Err(ApiError::bad_request(
            "restore_not_cancellable",
            format!(
                "this restore is {} and can no longer be cancelled. A restore stops being \
                 cancellable the moment its worker claims it, because by then it has already \
                 taken the safety backup it promised to leave behind. What it wrote, it wrote.",
                job.status
            ),
        ));
    }

    let recorded = omnion_backup::restore_jobs::request_restore_cancel(pool, id).await?;
    if !recorded {
        // The `where status = 'queued'` in the update matched nothing, so the worker claimed
        // it between the read above and this write. Re-read rather than assert: the honest
        // answer is the state the row is in now, not the one we raced past.
        let current_state = omnion_backup::restore_jobs::find_restore_job(pool, id, org)
            .await
            .map_err(|_| not_found())?;
        return Ok(Json(RestoreJobBody::from_row(&current_state)));
    }

    // And a job that is *still* queued is finished here, not left for the worker's next tick:
    // a panel that keeps offering a cancel on a row the operator already stopped is a
    // control that reads as broken. The worker's own flag check remains, because a cancel can
    // also land after the worker has claimed the job — and in that case the row is `running`
    // and this statement correctly touches nothing.
    omnion_backup::restore_jobs::settle_restore_cancel(
        pool,
        id,
        "cancelled before the first write — nothing was changed",
    )
    .await?;

    let settled = omnion_backup::restore_jobs::find_restore_job(pool, id, org)
        .await
        .map_err(|_| not_found())?;

    super::backups::record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.restore.cancelled",
        settled.backup_id.to_string(),
        json!({ "job_id": settled.id, "status": settled.status }),
    )
    .await;

    Ok(Json(RestoreJobBody::from_row(&settled)))
}

/// What a route sends when it asks for a queued restore.
#[derive(Debug, serde::Deserialize)]
pub struct QueueRestoreBody {
    /// The parts they left ticked.
    #[serde(default)]
    pub parts: Vec<String>,
    /// What they typed into the confirmation box.
    #[serde(default)]
    pub confirmation: String,
}

/// A queued restore, as the panel lists it and polls it.
#[derive(Debug, serde::Serialize)]
pub struct RestoreJobBody {
    /// Job id.
    pub id: Uuid,
    /// The run it restores.
    pub backup_id: Uuid,
    /// The parts it will restore.
    pub parts: Vec<String>,
    /// `queued|running|succeeded|failed|aborted`.
    pub status: String,
    /// Whether an abort is still possible. **A field rather than a derivation the panel
    /// makes**, because "cancellable" is the answer to the only question the operator is
    /// asking, and a panel that re-derives it from a status list is one edit away from
    /// offering a cancel on a running restore.
    pub cancellable: bool,
    /// Whether somebody has asked for it to stop.
    pub cancel_requested: bool,
    /// When it was queued.
    pub created_at: String,
    /// When the worker claimed it.
    pub started_at: Option<String>,
    /// When it stopped.
    pub finished_at: Option<String>,
    /// The protected run to go back to, on a success.
    pub safety_backup_id: Option<Uuid>,
    /// The loss the operator agreed to.
    pub live_dropped: i64,
    /// The coverage they agreed to.
    pub live_matches: i64,
    /// What it did, once it has.
    pub result: Option<serde_json::Value>,
    /// Why it failed, once it has.
    pub error: Option<String>,
}

impl RestoreJobBody {
    /// One row, rendered.
    ///
    /// Instants are formatted **here**, as RFC 3339 strings, rather than serialised as bare
    /// `OffsetDateTime`s. `time`'s human-readable `Serialize` is gated on a feature the
    /// workspace does not enable, so a bare instant crosses the wire as a nine-element array
    /// and the panel's `formatTimestamp` renders an em dash — the same em dash it renders for
    /// a value that has not happened yet. A loss and a designed answer must not look alike.
    fn from_row(job: &omnion_backup::restore_jobs::RestoreJob) -> Self {
        Self {
            id: job.id,
            backup_id: job.backup_id,
            parts: job.parts.clone(),
            status: job.status.clone(),
            cancellable: job.status == "queued",
            cancel_requested: job.cancel_requested,
            // NOT `rfc3339(Some(…))`: `created_at` is `not null` in the schema, so an
            // `Option` here would invite a panel branch for a value that cannot be absent —
            // and the branch it would draw is the "not happened yet" em dash again.
            created_at: rfc3339(Some(job.created_at)).unwrap_or_default(),
            started_at: rfc3339(job.started_at),
            finished_at: rfc3339(job.finished_at),
            safety_backup_id: job.safety_backup_id,
            live_dropped: job.live_dropped,
            live_matches: job.live_matches,
            result: job.result.clone(),
            error: job.error.clone(),
        }
    }
}

/// An instant as RFC 3339, or `None` when there is no instant to report.
fn rfc3339(at: Option<time::OffsetDateTime>) -> Option<String> {
    at.map(|value| {
        value
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    })
}

/// The audit and event trail a finished or refused restore writes.
///
/// One function for the worker, because the queued restore must leave the **same** trail the
/// synchronous one does: an audit entry that names the job, the parts and the safety backup
/// is what an operator reads a month later, and a second spelling of it is a second thing to
/// forget to add.
pub async fn record_restore_outcome(
    pool: &sqlx::PgPool,
    organization_id: Option<Uuid>,
    actor: Option<Uuid>,
    backup_id: Option<Uuid>,
    action: &'static str,
    outcome: &serde_json::Value,
) {
    if let (Some(actor), Some(backup_id)) = (actor, backup_id) {
        super::backups::record(
            pool,
            organization_id,
            actor,
            None,
            action,
            backup_id.to_string(),
            outcome.clone(),
        )
        .await;
    }

    if let Err(error) = omnion_events::bus::emit(
        pool,
        omnion_events::NewEvent::new(action)
            .organization(organization_id)
            .actor(actor)
            .payload(outcome.clone()),
    )
    .await
    {
        // A run that restored the library has *happened*; refusing to say so because the
        // event insert failed would be the platform lying about a fact it can see.
        tracing::warn!(%error, action, "a restore happened; its event did not");
    }
}

/// The audit entry a queued restore writes when it could not run.
pub async fn record_restore_job_refused(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    reason: &str,
) {
    let actor: Option<Uuid> = sqlx::query_scalar("select created_by from backup_restore_jobs where id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let organization: Option<Option<Uuid>> =
        sqlx::query_scalar("select organization_id from backup_restore_jobs where id = $1")
            .bind(job_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    // The actor is the job's owner, not the worker's — the worker pressed nothing. `unwrap_or`
    // on the actor is wrong here: an audit row with no actor is better than one attributing a
    // restore failure to the operator who queued it an hour ago, so a job with no creator is
    // left unaudited and the row itself carries the reason.
    if let Some(actor) = actor {
        super::backups::record(
            pool,
            organization.flatten(),
            actor,
            None,
            "backup.restore.refused",
            job_id.to_string(),
            json!({ "job_id": job_id, "reason": reason }),
        )
        .await;
    }
}

/// The `404` a missing run or a stranger's run answers.
///
/// Built in one place because it is answered from three: the queue route, the list route and
/// the cancel route. The message names neither the tenancy rule nor the id, because a `404`
/// that says "that backup belongs to another organization" confirms the backup exists — which
/// is the one thing a `404` is chosen over a `403` to avoid.
fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "backup_not_found", "no such backup")
}
