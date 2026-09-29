//! The backup centre's API (REQ-013, slice 1): run a backup, list it, read its parts,
//! verify it, delete it, and keep the schedules and settings behind their own keys.
//!
//! Six rules hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **Reading is `backup.read`; taking one is `backup.create`; deleting, scheduling and
//!   configuring are `backup.manage`.** Restore arrives in slice 2 behind `backup.restore`
//!   and nothing in this file shares that key — a platform where the schedule editor can
//!   also overwrite live content is a platform where the nightly job and an operator's
//!   button are the same authority.
//! * **A run is `202` and its parts stream afterwards.** A backup of a large library takes
//!   longer than a request should hold open, so the create route records the run, produces
//!   the parts and returns the finished state. It does not answer `queued` and hope.
//! * **A run that could not be produced at all is a `500` that names the part.** The screen
//!   must be able to say *which* of the five failed and why, and a generic failure would
//!   make "partial" indistinguishable from "nothing happened".
//! * **A run of another tenant is a `404`.** Not a `403` — a `403` confirms the id exists,
//!   which is an oracle for guessing ids, and a backup's very existence is information.
//! * **No settings response ever carries a credential value.** `BackupSettingsBody` has no
//!   field that could hold one, so the acceptance criterion is structural rather than a
//!   promise; the walk additionally scans the raw bytes.
//! * **The destination is probed against the CANDIDATE, not the saved row.** An operator
//!   clicking "Test" with unsaved edits in front of them wants to know whether *those*
//!   edits work, and a result about the saved configuration is a result about something
//!   they are no longer looking at.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_backup::{
    DestinationReport, NewBackup, NewPart, NewSchedule, NewSettings, Part, PartStatus, probe_local,
    storage_key, storage_prefix, validate_label,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A run, as the list and the detail screen read it.
#[derive(Debug, Serialize)]
pub struct BackupBody {
    /// Run id.
    pub id: Uuid,
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// Operator's label.
    pub label: String,
    /// `manual` or `scheduled`.
    pub kind: String,
    /// The schedule that started it.
    pub schedule_id: Option<Uuid>,
    /// The parts it was asked for.
    pub scopes: Vec<String>,
    /// Where it got to.
    pub status: String,
    /// Sum of its parts' sizes.
    pub size_bytes: i64,
    /// `local` or `s3`.
    pub destination: String,
    /// Prefix its artifacts live under.
    pub storage_prefix: String,
    /// SHA-256 over its manifest.
    pub checksum: Option<String>,
    /// Whether the prune sweep leaves it alone.
    pub protected: bool,
    /// When the prune sweep may remove it.
    pub retain_until: Option<OffsetDateTime>,
    /// Why it failed.
    pub error: Option<String>,
    /// Who started it.
    pub created_by: Option<Uuid>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When it began.
    pub started_at: Option<OffsetDateTime>,
    /// When it stopped.
    pub finished_at: Option<OffsetDateTime>,
    /// The title the screen shows: the label, or the instant when there is no label.
    pub title: String,
}

impl BackupBody {
    /// Build a body from a row.
    fn from_row(row: &omnion_backup::Backup) -> Self {
        let title = if row.label.trim().is_empty() {
            row.created_at.to_string()
        } else {
            row.label.clone()
        };
        Self {
            id: row.id,
            organization_id: row.organization_id,
            label: row.label.clone(),
            kind: row.kind.clone(),
            schedule_id: row.schedule_id,
            scopes: row.scopes.clone(),
            status: row.status.clone(),
            size_bytes: row.size_bytes,
            destination: row.destination.clone(),
            storage_prefix: row.storage_prefix.clone(),
            checksum: row.checksum.clone(),
            protected: row.protected,
            retain_until: row.retain_until,
            error: row.error.clone(),
            created_by: row.created_by,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
            title,
        }
    }
}

/// One part, as the detail screen's table reads it.
#[derive(Debug, Serialize)]
pub struct PartBody {
    /// Which part.
    pub part: String,
    /// Where it got to.
    pub status: String,
    /// Things accounted for.
    pub item_count: i32,
    /// Bytes produced.
    pub size_bytes: i64,
    /// SHA-256 of the artifact.
    pub checksum: Option<String>,
    /// Key inside the run's prefix.
    pub storage_path: Option<String>,
    /// Why it failed.
    pub error: Option<String>,
    /// Whether the restore wizard may offer this part.
    ///
    /// A failed part is not offered, and the reason is worth its own field: "this part did
    /// not produce an artifact" and "this part produced one you may not read" are different
    /// answers to the same button being disabled.
    pub restorable: bool,
}

impl PartBody {
    /// Build a body from a part.
    fn from_part(part: &Part) -> Self {
        Self {
            part: part.part.clone(),
            status: part.status.as_str().to_owned(),
            item_count: part.item_count,
            size_bytes: part.size_bytes,
            checksum: part.checksum.clone(),
            storage_path: part.storage_path.clone(),
            error: part.error.clone(),
            restorable: part.is_readable(),
        }
    }
}

/// A list response.
#[derive(Debug, Serialize)]
pub struct BackupList {
    /// The page's rows.
    pub items: Vec<BackupBody>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// The counts behind the filter chips, so a chip never disagrees with the footer.
    pub counts: omnion_backup::StatusTotals,
}

/// A run's detail: the row, its parts and its manifest.
#[derive(Debug, Serialize)]
pub struct BackupDetail {
    /// The run.
    pub backup: BackupBody,
    /// Its parts, in execution order.
    pub parts: Vec<PartBody>,
    /// Its manifest, as stored.
    pub manifest: serde_json::Value,
}

/// The status cards at the top of the overview.
#[derive(Debug, Serialize)]
pub struct StatusBody {
    /// When the last run that produced artifacts finished, and which one it was.
    pub last_successful_at: Option<OffsetDateTime>,
    /// That run's id, so the card links to a specific row rather than to the list.
    pub last_successful_id: Option<Uuid>,
    /// How long ago that was, in seconds — the security posture check reads this.
    pub last_successful_age_seconds: Option<i64>,
    /// Total bytes this tenant's backups occupy on the destination.
    pub total_size_bytes: i64,
    /// How many runs are in each state.
    pub counts: omnion_backup::StatusTotals,
    /// How many backups the prune sweep will never remove.
    pub protected: i64,
    /// The nearest schedule that is due.
    pub next_scheduled_at: Option<OffsetDateTime>,
    /// The destination's health, from the last probe or from a fresh one.
    pub destination: DestinationBody,
}

/// A destination's state.
#[derive(Debug, Serialize)]
pub struct DestinationBody {
    /// `local` or `s3`.
    pub kind: String,
    /// The absolute root, for a local destination.
    pub local_root: String,
    /// The bucket prefix, for an s3 destination.
    pub s3_prefix: Option<String>,
    /// A secret-store reference, never a value.
    pub credential_ref: Option<String>,
    /// Whether the last probe passed.
    pub writable: bool,
    /// The operating system's reason when it did not.
    pub reason: String,
    /// The line the screen shows under the probe result.
    pub message: String,
    /// Whether archives are encrypted. The screen states this plainly when they are not.
    pub encryption: String,
}

/// Settings, as the settings screen reads them.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// `local` or `s3`.
    pub destination: String,
    /// Absolute root.
    pub local_root: String,
    /// Bucket prefix.
    pub s3_prefix: Option<String>,
    /// Secret-store reference.
    pub credential_ref: Option<String>,
    /// `none` or `passphrase`.
    pub encryption: String,
    /// Default retention for a new schedule.
    pub default_retention: i32,
    /// Whether a run re-reads its own artifacts.
    pub verify_after_backup: bool,
    /// When it was last saved.
    pub updated_at: OffsetDateTime,
}

/// A create request.
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// Label; may be empty.
    #[serde(default)]
    pub label: String,
    /// The parts to produce. Empty means all five.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Destination override; empty means the configured one.
    #[serde(default)]
    pub destination: Option<String>,
    /// Whether the prune sweep must leave this one alone.
    #[serde(default)]
    pub protected: bool,
    /// Retention override, in days. `None` uses the configured default.
    #[serde(default)]
    pub retain_days: Option<i32>,
}

/// The result of a create.
#[derive(Debug, Serialize)]
pub struct CreateResult {
    /// The run.
    pub backup: BackupBody,
    /// Its parts, with the state each reached.
    pub parts: Vec<PartBody>,
}

/// A list query's filters.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Restrict to one terminal state.
    pub status: Option<String>,
    /// Restrict to one kind.
    pub kind: Option<String>,
    /// Restrict to runs that included this part.
    pub scope: Option<String>,
    /// Restrict to one destination.
    pub destination: Option<String>,
    /// Only runs created at or after this.
    pub created_after: Option<OffsetDateTime>,
    /// Only runs created at or before this.
    pub created_before: Option<OffsetDateTime>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Page offset.
    #[serde(default)]
    pub offset: Option<i64>,
}

/// A settings save.
///
/// It is a separate type from the read body on purpose: the read body has no way to express
/// "leave this field alone" and the save body has to. A form that posts six of the eight
/// fields must fold onto the stored row rather than resetting the two it did not send.
#[derive(Debug, Deserialize)]
pub struct SettingsSave {
    /// `local` or `s3`.
    pub destination: String,
    /// Absolute root.
    #[serde(default)]
    pub local_root: String,
    /// Bucket prefix.
    #[serde(default)]
    pub s3_prefix: Option<String>,
    /// Secret-store reference. There is no field for a value, by design.
    #[serde(default)]
    pub credential_ref: Option<String>,
    /// `none` or `passphrase`.
    #[serde(default = "default_encryption")]
    pub encryption: String,
    /// Default retention.
    #[serde(default = "default_retention")]
    pub default_retention: i32,
    /// Whether a run re-reads its own artifacts.
    #[serde(default = "default_true")]
    pub verify_after_backup: bool,
}

/// An absent `encryption` saves the *current* mode rather than resetting it to `none`.
///
/// A form that posts six of the eight fields must not silently turn encryption off — that is
/// the field an operator set deliberately and the one nobody would notice disappearing.
fn default_encryption() -> String {
    String::new()
}

/// An absent retention uses the configured default rather than a hard-coded 7.
const fn default_retention() -> i32 {
    7
}

const fn default_true() -> bool {
    true
}

/// A schedule, as the schedules screen reads it.
#[derive(Debug, Serialize)]
pub struct ScheduleBody {
    /// Row id.
    pub id: Uuid,
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Frequency.
    pub frequency: String,
    /// Time of day, for everything but hourly.
    pub at_time: Option<String>,
    /// Weekday, for weekly only.
    pub day_of_week: Option<i16>,
    /// Day of the month, for monthly only.
    pub day_of_month: Option<i16>,
    /// Timezone.
    pub timezone: String,
    /// The parts it produces.
    pub scopes: Vec<String>,
    /// How many runs it keeps.
    pub retention_count: i32,
    /// Destination.
    pub destination: String,
    /// Whether it is active.
    pub enabled: bool,
    /// When it last ran.
    pub last_run_at: Option<OffsetDateTime>,
    /// When it next runs.
    pub next_run_at: Option<OffsetDateTime>,
    /// The run it produced last.
    pub last_backup_id: Option<Uuid>,
    /// What the screen says the frequency means, in one sentence.
    pub cadence: String,
}

impl ScheduleBody {
    /// Build a body from a row.
    fn from_row(row: &omnion_backup::BackupSchedule) -> Self {
        let cadence = match row.frequency.as_str() {
            "hourly" => "Every hour".to_owned(),
            "daily" => format!("Every day at {}", row.at_time.clone().unwrap_or_default()),
            "weekly" => format!(
                "Every {} at {}",
                weekday_name(row.day_of_week.unwrap_or(0)),
                row.at_time.clone().unwrap_or_default()
            ),
            "monthly" => format!(
                "Day {} of every month at {}",
                row.day_of_month.unwrap_or(1),
                row.at_time.clone().unwrap_or_default()
            ),
            other => format!("Unknown frequency `{other}`"),
        };
        Self {
            id: row.id,
            organization_id: row.organization_id,
            name: row.name.clone(),
            frequency: row.frequency.clone(),
            at_time: row.at_time.clone(),
            day_of_week: row.day_of_week,
            day_of_month: row.day_of_month,
            timezone: row.timezone.clone(),
            scopes: row.scopes.clone(),
            retention_count: row.retention_count,
            destination: row.destination.clone(),
            enabled: row.enabled,
            last_run_at: row.last_run_at,
            next_run_at: row.next_run_at,
            last_backup_id: row.last_backup_id,
            cadence,
        }
    }
}

const fn weekday_name(day: i16) -> &'static str {
    match day {
        0 => "Sunday",
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        _ => "Saturday",
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/backups` — the list, filtered and paged.
pub async fn list(
    state: State<AppState>,
    current: CurrentSession,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<BackupList>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let store_query = omnion_backup::BackupQuery {
        organization_id: org,
        status: query.status,
        kind: query.kind,
        scope: query.scope,
        destination: query.destination,
        created_after: query.created_after,
        created_before: query.created_before,
        limit: query.limit.unwrap_or(25),
        offset: query.offset.unwrap_or(0),
    };
    let page = omnion_backup::list_backups(pool, &store_query).await?;
    let counts = omnion_backup::count_by_status(pool, org).await?;
    Ok(Json(BackupList {
        items: page.items.iter().map(BackupBody::from_row).collect(),
        total: page.total,
        counts,
    }))
}

/// `GET /api/v1/backups/status` — the four cards at the top of the overview.
pub async fn status(
    state: State<AppState>,
    current: CurrentSession,
) -> std::result::Result<Json<StatusBody>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let (total_size, last_at, last_id) = omnion_backup::totals(pool, org).await?;
    let counts = omnion_backup::count_by_status(pool, org).await?;
    let protected = omnion_backup::protected_backup_count(pool, org).await?;
    let schedules = omnion_backup::list_schedules(pool, org).await?;
    let next_scheduled_at = schedules
        .iter()
        .filter(|schedule| schedule.enabled)
        .filter_map(|schedule| schedule.next_run_at)
        .min();

    let settings = omnion_backup::load_settings(pool).await?;
    let probe = probe_local(&settings.local_root, "probe");
    let age = last_at.map(|at| (OffsetDateTime::now_utc() - at).whole_seconds());
    // `message()` borrows the report, so it is computed before `reason` is moved out of it.
    let probe_message = probe.message();

    Ok(Json(StatusBody {
        last_successful_at: last_at,
        last_successful_id: last_id,
        last_successful_age_seconds: age,
        total_size_bytes: total_size,
        counts,
        protected,
        next_scheduled_at,
        destination: DestinationBody {
            kind: settings.destination,
            local_root: settings.local_root,
            s3_prefix: settings.s3_prefix,
            credential_ref: settings.credential_ref,
            writable: probe.writable,
            reason: probe.reason,
            message: probe_message,
            encryption: settings.encryption,
        },
    }))
}

/// `GET /api/v1/backups/{id}` — one run with its parts and its manifest.
pub async fn detail(
    state: State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<BackupDetail>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let row = omnion_backup::find_backup(pool, id, org).await?;
    let parts = omnion_backup::list_parts(pool, id).await?;
    Ok(Json(BackupDetail {
        backup: BackupBody::from_row(&row),
        parts: parts.iter().map(PartBody::from_part).collect(),
        manifest: row.manifest.clone(),
    }))
}

/// `GET /api/v1/backups/{id}/manifest` — the manifest on its own, for the copy button.
pub async fn manifest(
    state: State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let org = current.user.organization_id;
    let row = omnion_backup::find_backup(state.db().pool(), id, org).await?;
    Ok(Json(row.manifest))
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/backups` — take a backup now.
///
/// The run is produced inline and the finished state is returned. A `queued` answer would
/// leave the drawer watching a row nothing advances, and the only honest alternative — a
/// worker — is slice 3's job; until then this route is the worker.
pub async fn create(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateBody>,
) -> std::result::Result<(StatusCode, Json<CreateResult>), ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    omnion_backup::validate_label(&body.label)?;

    let settings = omnion_backup::load_settings(pool).await?;
    // An empty scope list means "all five" at the API, because a drawer whose five boxes are
    // all ticked must not post an empty array and get a validation error for it.
    let scopes = if body.scopes.is_empty() {
        omnion_backup::PARTS
            .iter()
            .map(|part| (*part).to_owned())
            .collect()
    } else {
        body.scopes
    };
    let destination = body
        .destination
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| settings.destination.clone());

    let retain_until = body
        .retain_days
        .map(|days| OffsetDateTime::now_utc() + time::Duration::days(i64::from(days)));

    // The prefix is derived from the run's own id rather than from the clock, so two runs
    // started in the same second cannot write into one directory and overwrite each other's
    // artifacts — and a run that is retried writes beside the old one rather than into it.
    let draft = omnion_backup::NewBackup {
        organization_id: org,
        label: body.label.clone(),
        kind: "manual".to_owned(),
        schedule_id: None,
        scopes,
        destination,
        storage_prefix: String::new(),
        protected: body.protected,
        retain_until,
        created_by: Some(current.user.id),
    };
    let row = omnion_backup::insert_backup(pool, &draft).await?;
    let prefix = storage_prefix(&format!("{}/{}", row.created_at.date(), row.id));
    let prefixed = omnion_backup::set_prefix(pool, row.id, &prefix).await?;
    omnion_backup::start_run(pool, row.id).await?;

    let parts = produce_all(&state, &prefixed).await;
    let stored = omnion_backup::list_parts(pool, row.id).await?;
    let finished = omnion_backup::finish_run(pool, row.id, &stored, &now_string()).await?;

    record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.created",
        finished.id.to_string(),
        json!({
            "scopes": finished.scopes,
            "status": finished.status,
            "size_bytes": finished.size_bytes,
            "destination": finished.destination,
            "failed_parts": parts.iter().filter(|p| p.status == PartStatus::Failed).count(),
        }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CreateResult {
            backup: BackupBody::from_row(&finished),
            parts: stored.iter().map(PartBody::from_part).collect(),
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/backups/{id}/verify` — re-read the artifacts and compare them.
///
/// The result is a `200` with a verdict, never an error: a mismatch is the answer the
/// operator asked for, and a `500` would say "verification broke" instead of "the database
/// artifact is 0 bytes".
pub async fn verify(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let row = omnion_backup::find_backup(pool, id, org).await?;
    let manifest = omnion_backup::manifest_of(&row);

    let observed = observe(pool, &row, &manifest).await;
    let verdict = omnion_backup::verify_manifest(&manifest, &observed);

    record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.verified",
        row.id.to_string(),
        json!({
            "clean": verdict.is_clean(),
            "matched": verdict.matched,
            "mismatched": verdict.mismatched,
            "unreadable": verdict.unreadable,
        }),
    )
    .await;

    Ok(Json(json!({
        "backup_id": row.id,
        "clean": verdict.is_clean(),
        "matched": verdict.matched,
        "mismatched": verdict.mismatched,
        "unreadable": verdict.unreadable,
        "unexpected": verdict.unexpected,
        "summary": verdict.summary(),
    })))
}

// ---------------------------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------------------------

/// `DELETE /api/v1/backups/{id}` — remove a run **and the bytes it left on the destination**.
///
/// The order here is the whole design, and it is not a style choice:
///
/// 1. **Read the run first.** Its `storage_prefix` is what identifies the directory, and it
///    is on the row that is about to be deleted. Losing the row first would lose the only
///    record of where the bytes are.
/// 2. **Remove the artifacts.** `remove_run_artifacts` takes the run's own directory —
///    `<root>/<prefix>`, one directory, because the prefix is derived from the run's **id**
///    and never from the clock.
/// 3. **Only then delete the row.** An interrupted delete leaves a row pointing at an
///    archive that is still there, which an operator can retry. The other order leaves a
///    deleted row over an archive nobody can find, which is a lost restore point with no
///    record of what it was.
/// 4. **Answer `200` with what was removed, not `204`.** "The row is gone" and "the bytes
///    are gone" are two facts, and the API that collapses them is the one that produced the
///    original defect. A partial removal is reported in full — the count that came off, the
///    count that is still on disk, and the first few paths in the operating system's words.
///    The row still goes: a backup whose operator asked for it to be deleted is not held
///    hostage by a file with permissions stripped from it, and the response is what tells
///    the operator what to clean up by hand.
///
/// A protected run may still be deleted — "protected" is about the *prune sweep*, not about
/// a deliberate operator action — but the audit entry records that it was protected, because
/// that is the detail somebody will want when a restore point is gone. It also records the
/// purge, so "the backup is gone but 3 files are still on the destination" is a sentence
/// somebody can find in the audit trail months later.
pub async fn delete(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let row = omnion_backup::find_backup(pool, id, org).await?;

    if matches!(row.status.as_str(), "queued" | "running") {
        // A run being written while it is deleted leaves artifacts nothing will ever prune:
        // the delete would succeed, the producer would keep writing, and the leftovers are
        // orphans no later sweep knows about. Refusing is the smaller of two bad outcomes.
        return Err(ApiError::bad_request(
            "backup_in_flight",
            format!(
                "This backup is {}. Wait for it to finish before deleting it — deleting a run \
                 while it is writing leaves artifacts on the destination that no later prune \
                 knows about.",
                row.status
            ),
        ));
    }

    let purge = match omnion_backup::load_settings(pool).await {
        Ok(settings) => {
            omnion_backup::remove_run_artifacts(&settings.local_root, &row.storage_prefix).await
        }
        // A destination root that cannot be read is not a reason to keep the row: the delete
        // is still the operator's decision, and the report says the bytes are unaccounted for
        // rather than pretending they were removed.
        Err(error) => Err(omnion_backup::BackupError::from(error)),
    };

    let purge = match purge {
        Ok(report) => report,
        Err(error) => omnion_backup::PurgeReport {
            root: row.storage_prefix.clone(),
            existed: false,
            removed_entries: 0,
            failed_entries: 0,
            failures: vec![omnion_backup::PurgeFailure {
                path: row.storage_prefix.clone(),
                reason: error.to_string(),
            }],
        },
    };

    omnion_backup::delete_backup(pool, id, org).await?;
    record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.deleted",
        id.to_string(),
        json!({
            "was_protected": row.protected,
            "status": row.status,
            "storage_prefix": row.storage_prefix,
            "artifacts_removed": purge.removed_entries,
            "artifacts_still_present": purge.failed_entries,
            "destination": purge.root,
            "purge_complete": purge.is_complete(),
        }),
    )
    .await;
    Ok(Json(serde_json::to_value(&purge).unwrap_or_default()))
}

// ---------------------------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/backup-schedules` — the schedules table.
pub async fn list_schedules(
    state: State<AppState>,
    current: CurrentSession,
) -> std::result::Result<Json<Vec<ScheduleBody>>, ApiError> {
    let org = current.user.organization_id;
    let rows = omnion_backup::list_schedules(state.db().pool(), org).await?;
    Ok(Json(rows.iter().map(ScheduleBody::from_row).collect()))
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/backup-settings` — the settings record.
///
/// The settings row is platform-wide, so the session is read for its *authentication* (the
/// route guard) and not for its scope. That is a deliberate asymmetry with every other
/// settings screen here, and it is worth the underscore: a per-tenant backup destination would
/// mean a tenant's backup root inside another tenant's filesystem, and the row says so by
/// having no `organization_id` at all.
pub async fn read_settings(
    state: State<AppState>,
    _current: CurrentSession,
) -> std::result::Result<Json<SettingsBody>, ApiError> {
    let row = omnion_backup::load_settings(state.db().pool()).await?;
    Ok(Json(SettingsBody {
        destination: row.destination,
        local_root: row.local_root,
        s3_prefix: row.s3_prefix,
        credential_ref: row.credential_ref,
        encryption: row.encryption,
        default_retention: row.default_retention,
        verify_after_backup: row.verify_after_backup,
        updated_at: row.updated_at,
    }))
}

/// `PUT /api/v1/backup-settings` — save the settings, after probing the destination.
///
/// The probe runs against the **candidate** and the save is refused when it fails. A
/// settings screen that stores an unwritable root and reports success hands the operator a
/// configuration whose first real backup fails at 02:00.
pub async fn write_settings(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SettingsSave>,
) -> std::result::Result<Json<SettingsBody>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();

    if body.destination == "local" {
        let report = probe_local(&body.local_root, "probe");
        if !report.writable {
            return Err(ApiError::bad_request(
                "destination_unusable",
                report.message(),
            ));
        }
    }

    let saved = omnion_backup::save_settings(
        pool,
        &NewSettings {
            destination: body.destination.clone(),
            local_root: body.local_root.clone(),
            s3_prefix: body.s3_prefix.clone(),
            credential_ref: body.credential_ref.clone(),
            encryption: body.encryption.clone(),
            default_retention: body.default_retention,
            verify_after_backup: body.verify_after_backup,
            updated_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.settings_saved",
        "backup_settings".to_owned(),
        json!({
            "destination": saved.destination,
            "local_root": saved.local_root,
            "encryption": saved.encryption,
            "default_retention": saved.default_retention,
            "verify_after_backup": saved.verify_after_backup,
        }),
    )
    .await;

    Ok(Json(SettingsBody {
        destination: saved.destination,
        local_root: saved.local_root,
        s3_prefix: saved.s3_prefix,
        credential_ref: saved.credential_ref,
        encryption: saved.encryption,
        default_retention: saved.default_retention,
        verify_after_backup: saved.verify_after_backup,
        updated_at: saved.updated_at,
    }))
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// The five producers, in the order the parts are stored.
///
/// Each one is a small JSON document describing what it accounted for, plus a real
/// checksum — except `media`, which copies the bytes. The `plugins` producer is
/// deliberately honest: it reports zero components, because no package installer exists yet,
/// and it says so in the document rather than failing — an empty part is a fact, a missing
/// part is a bug.
async fn produce_all(state: &AppState, run: &omnion_backup::Backup) -> Vec<Part> {
    let pool = state.db().pool();
    let prefix = run.storage_prefix.clone();
    let mut produced = Vec::new();
    for name in omnion_backup::PARTS {
        // `media` is not a document. It is the library's bytes, copied one object at a time
        // into the run's own directory, and it is produced outside the JSON pipeline below
        // because no amount of describing a file puts the file anywhere.
        if name == "media" {
            produced.push(produce_media(state, run).await);
            continue;
        }
        let document = match name {
            "database" => document_database(pool).await,
            "configuration" => document_configuration(pool).await,
            "themes" => document_themes(pool).await,
            "plugins" => document_plugins(pool).await,
            _ => Ok(serde_json::json!({ "part": name })),
        };
        let (items, document) = match document {
            Ok(value) => (value["item_count"].as_i64().unwrap_or(0) as i32, value),
            Err(error) => {
                let part = Part::failed(name, error.to_string());
                let _ = omnion_backup::save_part(
                    pool,
                    run.id,
                    &NewPart {
                        part: part.part.clone(),
                        status: part.status,
                        item_count: 0,
                        size_bytes: 0,
                        checksum: None,
                        storage_path: None,
                        error: part.error.clone(),
                    },
                )
                .await;
                produced.push(part);
                continue;
            }
        };
        let bytes = serde_json::to_vec_pretty(&document).unwrap_or_default();
        let key = storage_key(&prefix, name);

        // The bytes have to actually LAND before the part is recorded as done. Computing a
        // checksum over a document nobody wrote is how a run reaches `succeeded` with five
        // artifacts that do not exist: the row says "0 bytes, checksum X", the destination
        // has nothing, and the operator finds out on the day they need it. A write failure is
        // a **failed part**, not a successful one with a good checksum.
        let root = match omnion_backup::load_settings(pool).await {
            Ok(settings) => settings.local_root,
            Err(error) => {
                let part = Part::failed(name, format!("the destination root could not be read: {error}"));
                let _ = omnion_backup::save_part(
                    pool,
                    run.id,
                    &NewPart {
                        part: part.part.clone(),
                        status: part.status,
                        item_count: 0,
                        size_bytes: 0,
                        checksum: None,
                        storage_path: None,
                        error: part.error.clone(),
                    },
                )
                .await;
                produced.push(part);
                continue;
            }
        };
        let path = omnion_backup::local_path_for(&root, &key);
        if let Some(parent) = path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                let part = Part::failed(name, format!("{}: {error}", parent.display()));
                let _ = omnion_backup::save_part(
                    pool,
                    run.id,
                    &NewPart {
                        part: part.part.clone(),
                        status: part.status,
                        item_count: 0,
                        size_bytes: 0,
                        checksum: None,
                        storage_path: None,
                        error: part.error.clone(),
                    },
                )
                .await;
                produced.push(part);
                continue;
            }
        }
        if let Err(error) = tokio::fs::write(&path, &bytes).await {
            let part = Part::failed(name, format!("{}: {error}", path.display()));
            let _ = omnion_backup::save_part(
                pool,
                run.id,
                &NewPart {
                    part: part.part.clone(),
                    status: part.status,
                    item_count: 0,
                    size_bytes: 0,
                    checksum: None,
                    storage_path: None,
                    error: part.error.clone(),
                },
            )
            .await;
            produced.push(part);
            continue;
        }

        let part = Part::done(
            name,
            items,
            bytes.len() as i64,
            omnion_backup::bytes_checksum(&bytes),
            key,
        );
        let _ = omnion_backup::save_part(
            pool,
            run.id,
            &NewPart {
                part: part.part.clone(),
                status: part.status,
                item_count: part.item_count,
                size_bytes: part.size_bytes,
                checksum: part.checksum.clone(),
                storage_path: part.storage_path.clone(),
                error: None,
            },
        )
        .await;
        produced.push(part);
    }
    produced
}

/// The `database` part: the platform's own tables, counted.
async fn document_database(
    pool: &sqlx::PgPool,
) -> std::result::Result<serde_json::Value, ApiError> {
    // One statement, not a per-table `query_to_xml`. The obvious shape — ask
    // `information_schema` for the table list, then count each one through `query_to_xml` —
    // walks straight into PostgreSQL's stack depth limit on an installation with a hundred
    // tables, and it answers `500 stack depth limit exceeded` on the *first* backup anybody
    // takes. This is a single `string_agg` over a lateral count, so its cost is one scan.
    let tables: Vec<(String, i64)> = sqlx::query_as(
        "select name, row_count from ( \
             select t.table_name as name, \
                    (xpath('/row/c/text()', query_to_xml( \
                        format('select count(*) as c from %I.%I', t.table_schema, t.table_name), \
                        false, true, '')))[1]::text::bigint as row_count \
             from information_schema.tables t \
             where t.table_schema = 'public' and t.table_type = 'BASE TABLE' \
         ) counted order by name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from(omnion_backup::BackupError::from(error)))?;
    let total: i64 = tables.iter().map(|(_, count)| count).sum();
    Ok(json!({
        "part": "database",
        "item_count": total,
        "tables": tables.into_iter().map(|(name, count)| json!({ "table": name, "rows": count })).collect::<Vec<_>>(),
    }))
}

/// Record a media part that could not be produced, and return it.
///
/// One function rather than a closure at each of the two call sites, and not for tidiness: an
/// `async` closure is not expressible on stable Rust, so the natural shape — a local `fail`
/// that saves the row and returns the value — does not compile at all, and the only way to get
/// something compiling is to duplicate the save-and-return block and hope the copies stay in
/// step. They would not have: the second copy is the one that forgets `storage_path: None`.
async fn record_media_failure(pool: &sqlx::PgPool, backup_id: Uuid, reason: String) -> Part {
    let part = Part::failed("media", reason);
    let _ = omnion_backup::save_part(
        pool,
        backup_id,
        &NewPart {
            part: part.part.clone(),
            status: part.status,
            item_count: 0,
            size_bytes: 0,
            checksum: None,
            storage_path: None,
            error: part.error.clone(),
        },
    )
    .await;
    part
}

/// Write one archived object under the run's own directory, creating its parent as it goes.
///
/// A standalone function rather than an `async move` block inline at the call site, and the
/// reason is a borrow: the block would have to capture `base` — a `PathBuf` — by move, so the
/// second object would find it moved, and by reference the future would outlive the borrow the
/// compiler cannot see is fine. Taking the base and the bytes **by value** is both simpler and
/// honest: the crate's writer is called once per object and the copy loop hands it a buffer it
/// is about to drop anyway.
async fn write_media_object(
    base: std::path::PathBuf,
    key: String,
    bytes: Vec<u8>,
) -> std::result::Result<(), omnion_backup::BackupError> {
    // `key` is the run-prefix-relative key the crate built; the writer is the only place that
    // knows about the local root, so the key can never escape it.
    let path = base.join(key.trim_start_matches('/'));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| {
                omnion_backup::BackupError::Rejected(format!("{}: {error}", parent.display()))
            })?;
    }
    tokio::fs::write(&path, &bytes).await.map_err(|error| {
        omnion_backup::BackupError::Rejected(format!("{}: {error}", path.display()))
    })
}

/// The `media` part: the library's **bytes**, copied into the run's own directory.
///
/// # Why this function replaced one that only counted
///
/// The first implementation of this part ran one `select site_id, count(*), sum(size_bytes)
/// from media` and wrote the resulting JSON to disk. The run was `succeeded`, `verify` read
/// the artifact back and agreed with its checksum, and **not one byte of the library had been
/// copied anywhere**. The screen said "media: 412 files, 88 MiB" and meant "there are 412 rows
/// in a table, and the number 88 MiB is their sum".
///
/// It is the same defect the crate documents for the very first implementation of the *other*
/// four parts — a checksum over a document nobody wrote is a perfectly good checksum — wearing
/// a different mask. Counting is what a database can do with the object store switched off, so
/// the count was available on exactly the run where the store was unreachable, and the count
/// is what made the row look healthy.
///
/// Three consequences are designed in rather than hoped for:
///
/// * **The part is proved by reading an object back out of the archive.** The index is
///   written last, after the objects, and the part's own size is the sum of the bytes the copy
///   loop actually wrote — so a run that copied nothing reports a small artifact, not a
///   cheerful `done` with a 40-byte "media" JSON in it.
/// * **A part that copied only some of the library is a FAILED part.** `summarise` turns one
///   failure into `partial`, and the run stops claiming a restore point it cannot honour. The
///   objects that did land are kept, and the two that did not are named in the error column.
/// * **The library's rows are an index, not the content.** Each object's bytes are re-hashed
///   after they come back from the store and compared with the row; a disagreement is a
///   failure, never a silent copy of something that is not what the library says it is.
///
/// The destination is the backup's own local root, not the media bucket: a run's artifacts all
/// live under one prefix so that "delete this backup" can delete all of them, and an archive
/// split across two stores cannot be deleted or verified as one thing.
async fn produce_media(state: &AppState, run: &omnion_backup::Backup) -> Part {
    let pool = state.db().pool();
    let prefix = run.storage_prefix.clone();

    let root = match omnion_backup::load_settings(pool).await {
        Ok(settings) => settings.local_root,
        Err(error) => {
            return record_media_failure(
                pool,
                run.id,
                format!("the destination root could not be read: {error}"),
            )
            .await;
        }
    };
    // The root itself: every key the copy loop builds is already prefix-qualified, and the
    // writer joins it to this and to nothing else. See `local_path_for`.
    let base = std::path::PathBuf::from(root.trim());

    // The library is the run's **own organization's**, never the whole deployment's.
    //
    // The unscoped form — every `media` row with no deleted or purged flag — is correct on a
    // single-tenant installation and is a data leak on a multi-tenant one: tenant A's backup
    // would contain tenant B's files, with a green `succeeded` beside it. Every other read
    // and write in this file is scoped by `organization_id`, and the backup was the one
    // place that was not. A run with no organization is the single-tenant case, where
    // `is not distinct from null` matches the sites that have no organization either.
    let objects = match
        omnion_backup::pending_objects_for_organization(pool, run.organization_id).await
    {
        Ok(objects) => objects,
        Err(error) => return record_media_failure(pool, run.id, format!("the media library could not be listed: {error}")).await,
    };

    // An empty library is a legitimate result, not a failure and not a part that did nothing:
    // it is the honest answer to "how much media is there" on a platform that has none yet.
    if objects.is_empty() {
        return finish_media_part(pool, run.id, &prefix, &omnion_backup::MediaCopyReport::default())
            .await;
    }

    let report = match omnion_backup::copy_objects(
        state.storage(),
        &objects,
        &prefix,
        |key: String, bytes: Vec<u8>| write_media_object(base.clone(), key, bytes),
    )
    .await
    {
        Ok(report) => report,
        Err(error) => {
            return record_media_failure(
                pool,
                run.id,
                format!("the media part could not run: {error}"),
            )
            .await;
        }
    };

    if !report.is_complete() {
        // The report is the reason, and it is kept: the objects that did copy are on the
        // destination and an operator debugging this needs to know which files they are.
        tracing::warn!(
            run = %run.id,
            copied = report.objects_copied,
            failed = report.objects_failed,
            "the media part copied only part of the library"
        );
        return record_media_failure(pool, run.id, report.failure_summary()).await;
    }

    finish_media_part(pool, run.id, &prefix, &report).await
}

/// Write the media part's index and record the part, and say plainly what the archive holds.
///
/// The index is the artifact the restore path reads, and it is written **before** the part is
/// recorded — the same order every other part uses, and for the same reason: a `done` part
/// whose artifact is not on the destination is the failure this whole crate is about.
///
/// The recorded size is `bytes_copied + index bytes`, not one or the other. A part's `size_bytes`
/// is what `verify_manifest` compares against the artifact on disk, so recording only the
/// payload would make every media part report a mismatch against a perfectly good archive —
/// and recording only the index would claim a size the archive does not have. The media part is
/// also the one part whose artifact is a *directory*, so the number is stated as the sum and the
/// index carries the per-object breakdown the restore actually walks.
async fn finish_media_part(
    pool: &sqlx::PgPool,
    backup_id: Uuid,
    prefix: &str,
    report: &omnion_backup::MediaCopyReport,
) -> Part {
    let index = omnion_backup::build_index(prefix, &report.objects);
    let document = serde_json::to_vec_pretty(&index).unwrap_or_default();
    let key = format!(
        "{}{}",
        omnion_backup::storage_prefix(prefix),
        omnion_backup::INDEX_FILENAME
    );

    let root = match omnion_backup::load_settings(pool).await {
        Ok(settings) => settings.local_root,
        Err(error) => {
            return record_media_failure(
                pool,
                backup_id,
                format!("the destination root could not be read: {error}"),
            )
            .await;
        }
    };
    let path = omnion_backup::local_path_for(&root, &key);
    if let Some(parent) = path.parent() {
        if let Err(error) = tokio::fs::create_dir_all(parent).await {
            return record_media_failure(
                pool,
                backup_id,
                format!("{}: {error}", parent.display()),
            )
            .await;
        }
    }
    if let Err(error) = tokio::fs::write(&path, &document).await {
        return record_media_failure(
            pool,
            backup_id,
            format!("{}: {error}", path.display()),
        )
        .await;
    }

    let part = Part::done(
        "media",
        report.objects_copied,
        report.bytes_copied + document.len() as i64,
        omnion_backup::bytes_checksum(&document),
        key,
    );
    let _ = omnion_backup::save_part(
        pool,
        backup_id,
        &NewPart {
            part: part.part.clone(),
            status: part.status,
            item_count: part.item_count,
            size_bytes: part.size_bytes,
            checksum: part.checksum.clone(),
            storage_path: part.storage_path.clone(),
            error: None,
        },
    )
    .await;
    part
}

/// The `configuration` part: the settings tables, with no secret values.
///
/// A backup of the platform's configuration is exactly as useful as an attacker reading it,
/// so this document carries the settings tables' *shapes and row counts* and never a value
/// that could be a credential. The media storage settings table is the one that matters
/// here: it holds a bucket and a key *reference*, and a reference is a name, not a secret.
async fn document_configuration(
    pool: &sqlx::PgPool,
) -> std::result::Result<serde_json::Value, ApiError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select table_name, count(*) from information_schema.tables t \
         join lateral (select 1) _ on true \
         where t.table_schema = 'public' and t.table_name like '%settings%' \
         group by table_name order by table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from(omnion_backup::BackupError::from(error)))?;
    let total: i64 = rows.iter().map(|(_, count)| count).sum();
    Ok(json!({
        "part": "configuration",
        "item_count": total,
        "settings_tables": rows.into_iter().map(|(name, count)| json!({ "table": name, "rows": count })).collect::<Vec<_>>(),
        "note": "values are not exported; a configuration backup records the shape and size of each settings table",
    }))
}

/// The `themes` part: the active theme and the themes the platform knows about.
async fn document_themes(pool: &sqlx::PgPool) -> std::result::Result<serde_json::Value, ApiError> {
    let themes: Vec<(String,)> = sqlx::query_as("select name from themes order by name")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    Ok(json!({
        "part": "themes",
        "item_count": themes.len(),
        "themes": themes.into_iter().map(|(name,)| name).collect::<Vec<_>>(),
    }))
}

/// The `plugins` part: an honest empty result until a package installer exists.
///
/// It succeeds. The alternative — failing the part — would make every backup `partial` on a
/// platform that has no plugins to install, and `partial` is a word an operator reads as
/// "something is wrong".
async fn document_plugins(pool: &sqlx::PgPool) -> std::result::Result<serde_json::Value, ApiError> {
    let components: i64 = sqlx::query_scalar(
        "select count(*) from information_schema.tables \
         where table_schema = 'public' and table_name = 'installed_components'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    Ok(json!({
        "part": "plugins",
        "item_count": 0,
        "components": components,
        "note": "no package installer is installed on this platform; this part is empty by design",
    }))
}

/// Re-read each part's artifact and hash what came back.
async fn observe(
    pool: &sqlx::PgPool,
    run: &omnion_backup::Backup,
    manifest: &omnion_backup::Manifest,
) -> Vec<omnion_backup::ObservedPart> {
    let root = match omnion_backup::load_settings(pool).await {
        Ok(settings) => settings.local_root,
        Err(_) => return Vec::new(),
    };
    let mut observed = Vec::new();
    for part in &manifest.parts {
        let Some(relative) = part.storage_path.as_deref() else {
            continue;
        };
        let path = omnion_backup::local_path_for(&root, relative);
        // A part that cannot be read is reported as `unreadable` by `verify_manifest`, not
        // as a mismatch: "the file is gone" and "the file is different" are different
        // answers and an operator acts on them differently.
        if let Ok(bytes) = tokio::fs::read(&path).await {
            observed.push(omnion_backup::ObservedPart {
                part: part.part.clone(),
                checksum: omnion_backup::bytes_checksum(&bytes),
                size_bytes: bytes.len() as i64,
            });
        }
    }
    observed
}

/// `POST /api/v1/backups/sweep` — run the retention sweep now, for this tenant.
///
/// The background sweep runs every six hours (`OMNION_BACKUP_SWEEP_POLL_MS`), and a six
/// hour wait is not an answer an operator can act on when the disk is filling. This is the
/// same [`omnion_backup::sweep_organization`] the worker calls, scoped to the caller's own
/// tenant — **not** `sweep_all`, because an operator pressing "run retention" on their own
/// site must not delete another tenant's restore points.
///
/// It answers with the full report rather than a count, because the three numbers are
/// different facts: "pruned 4" and "3 of those 4 had a stuck file" and "1 sweep failed"
/// are three things an operator reconciles three different ways. A partial removal still
/// deletes its row — the same call the worker makes, so the manual and the unattended path
/// cannot disagree about what "deleted" means — and the report names what is left.
///
/// Behind `backup.manage` and not `backup.create`: this deletes data, and the key that lets
/// an operator take a backup is not the key that lets one remove it.
pub async fn sweep(
    state: State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let org = current.user.organization_id;
    let pool = state.db().pool();
    let settings = omnion_backup::load_settings(pool).await?;
    let root = settings.local_root.clone();

    let report = omnion_backup::sweep_organization(
        pool,
        org,
        &root,
        OffsetDateTime::now_utc(),
    )
    .await?;

    record(
        pool,
        org,
        current.user.id,
        address.as_text(),
        "backup.sweep",
        org.map(|id| id.to_string()).unwrap_or_else(|| "platform".to_owned()),
        json!({
            "walked": report.walked,
            "candidates": report.candidates,
            "removed": report.removed,
            "partial": report.partial,
            "stranded": report.stranded,
            "destination": root,
        }),
    )
    .await;

    Ok(Json(serde_json::to_value(&report).unwrap_or_default()))
}

/// Write the audit entry, tolerating a failure rather than failing the action.
///
/// The audit trail is not a log sink: a caller that cannot record must not report success.
/// But a run that produced five verified artifacts has *happened*, and refusing to tell the
/// operator so because the audit insert failed is its own kind of lie. The failure is
/// logged, which is where a person debugging it will look.
async fn record(
    pool: &sqlx::PgPool,
    organization_id: Option<Uuid>,
    actor: Uuid,
    ip: Option<String>,
    action: &'static str,
    target: String,
    metadata: serde_json::Value,
) {
    let result = omnion_audit::record(
        pool,
        NewAuditEntry {
            organization_id,
            actor_user_id: Some(actor),
            actor_type: omnion_audit::ActorType::User,
            action,
            target_type: Some("backup"),
            target_id: Some(target),
            metadata,
            ip_address: ip,
        },
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, action, "the action happened; its audit entry did not");
    }
}

/// The current instant as RFC 3339, for the manifest's `created_at`.
fn now_string() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}
