//! Retention's API (REQ-010, slice 4): the policies, the run log, the hold and the manual run.
//!
//! Five rules hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **Reading a policy is `media.read`; writing one is `media.settings.manage`.** The
//!   settings screen shows a reader what the site promises to keep, because "my file was
//!   deleted by a policy" is a question a person without write access must be able to ask.
//!   Changing the windows is the destructive one, and it is the key slice 3 already gave the
//!   storage screen.
//! * **A hold is set on a *file*, and the reason is the caller's.** The hold is the one switch
//!   in the library that stops every sweep, so it carries an audit entry naming the file and
//!   the reason, and the settings screen will not let anybody set one from the policy form
//!   alone — a policy-level hold would be invisible per file, which is the whole point of it.
//! * **A manual run is a run.** It writes a row in the same table and with the same fields as
//!   the daily pass, so the log does not have two shapes and the "when did this last run"
//!   question has one answer.
//! * **The route answers what it actually did, and refuses what it did not.** A `Run now`
//!   that removed 4 files, held 1 back and refused 2 has to say all three: the first number is
//!   what happened, and the other two are the two ways the same run could have been a failure.
//! * **A purge is never run from here.** `purge_eligible` in the crate decides what may go and
//!   this file only *asks*; a route that computed its own candidate list would be a second
//!   answer to "may this file go", and the two would disagree the moment one of them was
//!   edited.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_media::{
    NewRetentionPolicy, PurgeRefusal, RetentionPolicy, RetentionRun, RunTotals,
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

/// A retention policy, as the settings screen reads it.
#[derive(Debug, Serialize)]
pub struct PolicyBody {
    /// Row id.
    pub id: Uuid,
    /// Site the policy belongs to.
    pub site_id: Uuid,
    /// How it is named.
    pub name: String,
    /// The folder it governs; null means the whole site.
    pub folder_id: Option<Uuid>,
    /// The folder's path, so the screen can say *which* folder rather than print a uuid.
    pub folder_path: String,
    /// Superseded history window, in days.
    pub keep_versions_days: i32,
    /// Restore window, in days.
    pub trash_days: i32,
    /// Hard-delete window, in days, counted from the deletion.
    pub purge_after_days: i32,
    /// Whether a hold applies to everything this policy would otherwise touch.
    pub legal_hold: bool,
    /// Whether the worker acts on this row.
    pub enabled: bool,
    /// What the policy does to a file, in one sentence.
    pub behaviour: String,
    /// What the policy is scoped to, in words.
    pub scope: String,
}

impl PolicyBody {
    /// Describe a stored policy for the panel.
    fn build(policy: &RetentionPolicy, folder_path: Option<&str>) -> Self {
        Self {
            id: policy.id,
            site_id: policy.site_id,
            name: policy.name.clone(),
            folder_id: policy.folder_id,
            folder_path: folder_path.unwrap_or_default().to_string(),
            keep_versions_days: policy.keep_versions_days,
            trash_days: policy.trash_days,
            purge_after_days: policy.purge_after_days,
            legal_hold: policy.legal_hold,
            enabled: policy.enabled,
            behaviour: policy.describe(),
            scope: policy.scope(folder_path),
        }
    }
}

/// A policy as the caller describes it.
///
/// `folder_id` carries **three** states, not two, and `Option<Option<Uuid>>` cannot express
/// them: absent (leave the scope alone), `null` (the whole site) and an id (this folder). The
/// middle one is the edit an operator most often wants — "this rule was for one folder, now
/// it is for everything" — and the request type has to be able to say it.
///
/// So the field is a plain `Option<Uuid>` plus a *flag* read from the raw body, and the flag
/// is what `Option<Option<_>>` would have been. `#[serde(default)]` on the nested form was
/// tried first and is a trap: serde cannot tell "the field was absent" from "the field was
/// null" on a type whose `None` means both, so the explicit `null` — the one edit that
/// matters — silently arrived as "leave it alone" and the scope was never cleared. A
/// `fold` over a `Vec<(String, Value)>` is the only way to see what was actually sent.
#[derive(Debug, Deserialize)]
struct RawPolicy {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    keep_versions_days: Option<i32>,
    #[serde(default)]
    trash_days: Option<i32>,
    #[serde(default)]
    purge_after_days: Option<i32>,
    #[serde(default)]
    legal_hold: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// A policy as the caller describes it.
#[derive(Debug, Default)]
pub struct PolicyInput {
    /// How the policy is named.
    pub name: Option<String>,
    /// The folder it governs: `None` leaves the scope alone, `Some(None)` means the whole site.
    pub folder_id: Option<Option<Uuid>>,
    /// Superseded history window.
    pub keep_versions_days: Option<i32>,
    /// Restore window.
    pub trash_days: Option<i32>,
    /// Hard-delete window.
    pub purge_after_days: Option<i32>,
    /// Whether a hold applies to what this policy touches.
    pub legal_hold: Option<bool>,
    /// Whether the worker acts on it.
    pub enabled: Option<bool>,
}

impl PolicyInput {
    /// Read the request, keeping the one distinction a flat `Option` cannot carry.
    ///
    /// `folder_id: null` and an absent `folder_id` are the same value on every other field and
    /// opposite values on this one, so the raw body is the only place that difference exists.
    fn decode(body: &serde_json::Value) -> Result<Self, ApiError> {
        let raw: RawPolicy = serde_json::from_value(body.clone()).map_err(|error| {
            ApiError::bad_request("invalid_retention_setting", format!("the request is not a policy: {error}"))
                .with_details(json!({ "field": "body" }))
        })?;

        // The scope is read from the **key's presence**, not from a deserialised value, and
        // that is the second half of the trap: `Option<Value>` is *also* lossy here, because
        // serde's `Option` visitor maps a `null` to `None` whatever the inner type is. So
        // "the key was not sent" and "the key was sent as null" have to be told apart by
        // asking the object whether it has the key at all. There is no typed shape that
        // carries both, and the only honest place to look is the raw map.
        let folder_id = match body.get("folder_id") {
            // The caller did not touch the scope.
            None => None,
            // The caller widened the policy to the whole site.
            Some(serde_json::Value::Null) => Some(None),
            // The caller named a folder.
            Some(value) => {
                let parsed: Uuid = serde_json::from_value(value.clone()).map_err(|_| {
                    ApiError::bad_request(
                        "invalid_retention_setting",
                        "folder_id must be a folder id, or null for the whole site",
                    )
                    .with_details(json!({ "field": "folder_id" }))
                })?;
                Some(Some(parsed))
            }
        };
        Ok(Self {
            name: raw.name,
            folder_id,
            keep_versions_days: raw.keep_versions_days,
            trash_days: raw.trash_days,
            purge_after_days: raw.purge_after_days,
            legal_hold: raw.legal_hold,
            enabled: raw.enabled,
        })
    }

    /// Reduce the request into the crate's type, where validation happens.
    fn into_new(self) -> NewRetentionPolicy {
        NewRetentionPolicy {
            name: self.name,
            folder_id: self.folder_id,
            keep_versions_days: self.keep_versions_days,
            trash_days: self.trash_days,
            purge_after_days: self.purge_after_days,
            legal_hold: self.legal_hold,
            enabled: self.enabled,
        }
    }
}

/// The policy list of a site, with the numbers the screen cannot compute on its own.
#[derive(Debug, Serialize)]
pub struct PolicyList {
    /// Site the policies belong to.
    pub site_id: Uuid,
    /// The policies, site-wide rule first.
    pub policies: Vec<PolicyBody>,
    /// Trashed files whose restore window has closed, under the site-wide rule.
    pub past_restore: i64,
    /// The bytes those files occupy.
    pub past_restore_bytes: i64,
    /// The last finished run of any kind, or null when this site has never been swept.
    pub last_run: Option<RunBody>,
    /// One sentence about the site-wide rule, for the header.
    pub summary: String,
}

/// One run of the log, as the screen reads it.
#[derive(Debug, Serialize)]
pub struct RunBody {
    /// Run id.
    pub id: Uuid,
    /// `daily`, `manual`, `versions`, `trash` or `purge`.
    pub kind: String,
    /// Superseded versions removed.
    pub versions_removed: i64,
    /// Bytes those removals reclaimed.
    pub versions_bytes: i64,
    /// Files purged.
    pub purged: i64,
    /// Bytes those purges reclaimed.
    pub purged_bytes: i64,
    /// Files the sweep would not purge because something still points at them.
    pub refused: i64,
    /// Files the hold removed from the eligible set.
    pub held_back: i64,
    /// What stopped the run; empty when nothing did.
    pub error: String,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished; null while it is running.
    pub finished_at: Option<OffsetDateTime>,
    /// One sentence — a number without a word is not an answer an operator can act on.
    pub summary: String,
    /// Total bytes reclaimed by this run.
    pub bytes_reclaimed: i64,
}

impl RunBody {
    /// Describe a run for the panel.
    fn build(run: &RetentionRun) -> Self {
        Self {
            id: run.id,
            kind: run.kind.clone(),
            versions_removed: run.versions_removed,
            versions_bytes: run.versions_bytes,
            purged: run.purged,
            purged_bytes: run.purged_bytes,
            refused: run.refused,
            held_back: run.held_back,
            error: run.error.clone(),
            started_at: run.started_at,
            finished_at: run.finished_at,
            summary: run.summary(),
            bytes_reclaimed: run.bytes_reclaimed(),
        }
    }
}

/// The run log of a site.
#[derive(Debug, Serialize)]
pub struct RunList {
    /// Site the log belongs to.
    pub site_id: Uuid,
    /// The most recent runs, newest first.
    pub runs: Vec<RunBody>,
}

/// The answer to "run retention now".
#[derive(Debug, Serialize)]
pub struct RunResponse {
    /// The run row that was written, so the operator can open it in the log.
    pub run_id: Uuid,
    /// The run as the log will read it.
    pub run: RunBody,
    /// Files still in the trash, and their bytes.
    pub remaining_files: i64,
    /// The bytes those files occupy.
    pub remaining_bytes: i64,
    /// What the sweep would not touch, with the records holding each file.
    pub refused: Vec<RefusalBody>,
}

/// One "this file is still referenced" line, naming the record.
#[derive(Debug, Serialize)]
pub struct RefusalBody {
    /// The file that would have been purged.
    pub media_id: Uuid,
    /// Its name.
    pub filename: String,
    /// What still points at it.
    pub resource_kind: String,
    /// The referent's id.
    pub resource_id: String,
    /// Which field of it points here.
    pub field: String,
    /// The whole thing as one line.
    pub describe: String,
}

impl From<&PurgeRefusal> for RefusalBody {
    fn from(row: &PurgeRefusal) -> Self {
        Self {
            media_id: row.media_id,
            filename: row.filename.clone(),
            resource_kind: row.resource_kind.clone(),
            resource_id: row.resource_id.clone(),
            field: row.field.clone(),
            describe: row.describe(),
        }
    }
}

/// A hold or a release of one, with the reason the audit log keeps.
#[derive(Debug, Deserialize)]
pub struct HoldInput {
    /// Whether the file is being held or released.
    pub hold: bool,
    /// Why. Required in both directions: a hold nobody can explain is a file nobody will ever
    /// be allowed to delete, and a release nobody can explain is a file somebody bypassed the
    /// rules for.
    pub reason: String,
}

/// The site a retention route acts on.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// Site whose library is addressed.
    pub site_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Handlers — the policies
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/retention` — a site's policies and the numbers around them.
pub async fn read(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<PolicyList>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();

    let policies = omnion_media::list_policies(pool, site.id).await?;
    let paths: std::collections::HashMap<Uuid, String> =
        omnion_media::policy_scope_paths(pool, site.id).await?.into_iter().collect();

    // The header's numbers come from the *site-wide* rule, because a site has exactly one
    // fallback. A folder with its own policy is counted against that policy's window, and the
    // site-wide number is deliberately not a total of the library: it is "how many files would
    // the default policy have released by now", which is the question the header answers.
    let site_row = omnion_media::site_policy(pool, site.id).await?;
    let window = omnion_media::Window {
        keep_versions_days: site_row.keep_versions_days,
        trash_days: site_row.trash_days,
        purge_after_days: site_row.purge_after_days,
    };
    let (past_restore, past_restore_bytes) =
        omnion_media::past_restore_window(pool, site.id, window).await?;
    let last = omnion_media::last_run(pool, site.id).await?;

    Ok(Json(PolicyList {
        site_id: site.id,
        summary: site_row.describe(),
        past_restore,
        past_restore_bytes,
        last_run: last.as_ref().map(RunBody::build),
        policies: policies
            .iter()
            .map(|policy| {
                let path = policy.folder_id.and_then(|id| paths.get(&id).cloned());
                PolicyBody::build(policy, path.as_deref())
            })
            .collect(),
    }))
}

/// `POST /api/v1/media/retention` — create a policy.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Json(body): Json<serde_json::Value>,
) -> std::result::Result<Json<PolicyBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let policy = omnion_media::create_policy(
        state.db().pool(),
        site.id,
        omnion_media::validate_retention(site.id, PolicyInput::decode(&body)?.into_new())?,
    )
    .await?;
    let path = scope_path(&state, policy.folder_id).await?;

    audit(
        &state,
        current.user.id,
        &site,
        address.as_text().as_deref().unwrap_or_default(),
        "media.retention_policy_created",
        "media_retention_policy",
        policy.id,
        json!({
            "site_id": site.id,
            "name": policy.name,
            "keep_versions_days": policy.keep_versions_days,
            "trash_days": policy.trash_days,
            "purge_after_days": policy.purge_after_days,
            "legal_hold": policy.legal_hold,
            "scope": policy.scope(path.as_deref()),
        }),
    )
    .await?;

    Ok(Json(PolicyBody::build(&policy, path.as_deref())))
}

/// `PUT /api/v1/media/retention/{id}` — change a policy.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Path(policy_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> std::result::Result<Json<PolicyBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let (after, changes) = omnion_media::update_policy(
        state.db().pool(),
        site.id,
        policy_id,
        PolicyInput::decode(&body)?.into_new(),
    )
    .await?;
    let path = scope_path(&state, after.folder_id).await?;

    // The audit entry names the fields that moved. Retention is the one library feature where
    // "the policy changed" answers none of the three questions anybody asks afterwards: what
    // did it say before, what does it say now, and who changed it.
    audit(
        &state,
        current.user.id,
        &site,
        address.as_text().as_deref().unwrap_or_default(),
        "media.retention_policy_changed",
        "media_retention_policy",
        after.id,
        json!({
            "site_id": site.id,
            "name": after.name,
            "changed": changes.fields,
            "keep_versions_days": after.keep_versions_days,
            "trash_days": after.trash_days,
            "purge_after_days": after.purge_after_days,
            "legal_hold": after.legal_hold,
            "enabled": after.enabled,
        }),
    )
    .await?;

    Ok(Json(PolicyBody::build(&after, path.as_deref())))
}

/// `DELETE /api/v1/media/retention/{id}` — remove a policy.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Path(policy_id): Path<Uuid>,
) -> std::result::Result<StatusCode, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let policy = omnion_media::find_policy(state.db().pool(), site.id, policy_id)
        .await?
        .ok_or_else(policy_not_found)?;
    omnion_media::delete_policy(state.db().pool(), site.id, policy_id).await?;

    audit(
        &state,
        current.user.id,
        &site,
        address.as_text().as_deref().unwrap_or_default(),
        "media.retention_policy_deleted",
        "media_retention_policy",
        policy.id,
        json!({ "site_id": site.id, "name": policy.name }),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Handlers — the hold
// ---------------------------------------------------------------------------------------------

/// `PUT /api/v1/media/files/{id}/hold` — put a file under a legal hold, or take it off one.
///
/// The hold is the one switch in the library that stops *every* sweep, so it is also the one
/// switch a person will be asked to justify. The reason is required in both directions and
/// lands in the audit log with the file named.
pub async fn set_file_hold(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
    Json(input): Json<HoldInput>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let file = omnion_media::find_file_any_state(pool, file_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "media_not_found", "no such file")
        })?;
    let site = site_in_scope(&state, &current, file.site_id).await?;

    let reason = input.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "hold_reason_required",
            "say why — a legal hold with no reason is a file nobody will ever be allowed to \
             delete, and a release with no reason is a file somebody bypassed the rules for",
        )
        .with_details(json!({ "field": "reason" })));
    }

    // `set_hold` only writes when the value actually moves, so a second identical press is a
    // no-op rather than a second audit entry — a log with three identical rows reads as three
    // people and tells an operator nothing about who decided what.
    let changed = omnion_media::set_hold(pool, site.id, file_id, input.hold).await?;
    if !changed {
        return Ok(Json(json!({
            "media_id": file_id,
            "legal_hold": input.hold,
            "changed": false,
        })));
    }

    bus::emit(
        pool,
        NewEvent::new(if input.hold { "media.hold_placed" } else { "media.hold_released" })
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": file_id,
                "site_id": site.id,
                "filename": file.filename,
                "reason": reason,
            })),
    )
    .await?;

    audit(
        &state,
        current.user.id,
        &site,
        address.as_text().as_deref().unwrap_or_default(),
        if input.hold { "media.hold_placed" } else { "media.hold_released" },
        "media",
        file_id,
        json!({ "site_id": site.id, "filename": file.filename, "reason": reason }),
    )
    .await?;

    Ok(Json(json!({ "media_id": file_id, "legal_hold": input.hold, "changed": true })))
}

// ---------------------------------------------------------------------------------------------
// Handlers — the run
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/media/retention/run` — sweep a site now, and answer what it actually did.
pub async fn run_now(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<RunResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let outcome = run_once(&state, site.id, Some(current.user.id)).await?;

    Ok(Json(RunResponse {
        run_id: outcome.run_id,
        run: outcome.run,
        remaining_files: outcome.remaining_files,
        remaining_bytes: outcome.remaining_bytes,
        refused: outcome.refused,
    }))
}

/// `GET /api/v1/media/retention/runs` — the run log of a site.
pub async fn runs(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<RunList>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let runs = omnion_media::list_retention_runs(state.db().pool(), site.id, 25).await?;
    Ok(Json(RunList {
        site_id: site.id,
        runs: runs.iter().map(RunBody::build).collect(),
    }))
}

/// `POST /api/v1/media/retention/repair` — drop reference rows whose referent is gone.
///
/// The other direction from the purge refusal. A page that was deleted leaves a reference
/// behind that will refuse a purge for ever, so "cannot purge: still referenced" is a sentence
/// an operator meets and cannot act on. What the repair scan removes is a *lie about a record
/// that no longer exists* — never a reference to a kind this scan cannot verify, because a
/// module that arrives tomorrow must not find its usage rows already deleted.
pub async fn repair(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let removed = omnion_media::repair_references(state.db().pool(), 1000).await?;

    if removed > 0 {
        audit(
            &state,
            current.user.id,
            &site,
            address.as_text().as_deref().unwrap_or_default(),
            "media.references_repaired",
            "site",
            site.id,
            json!({ "site_id": site.id, "references_removed": removed }),
        )
        .await?;
    }

    Ok(Json(json!({
        "site_id": site.id,
        "references_removed": removed,
        "summary": if removed == 0 {
            "No reference rows point at a record that no longer exists.".to_string()
        } else {
            format!("{removed} reference row(s) pointed at a deleted page and were removed.")
        },
    })))
}

// ---------------------------------------------------------------------------------------------
// The sweep itself
// ---------------------------------------------------------------------------------------------

/// What one pass of the worker did. Shared by the route and the daily worker, so the two
/// cannot drift into two different answers about the same site.
pub struct RunOutcome {
    /// The run row that was written.
    pub run_id: Uuid,
    /// The run, as the log reads it.
    pub run: RunBody,
    /// Files still in the trash.
    pub remaining_files: i64,
    /// The bytes they occupy.
    pub remaining_bytes: i64,
    /// What the sweep would not touch.
    pub refused: Vec<RefusalBody>,
}

/// Sweep one site: superseded versions, then the trash, then the run log.
///
/// Bounded by [`omnion_media::SWEEP_BATCH`] on both halves, so a site with a million rows does
/// not hold a statement open for a working day; the next tick continues where this one stopped,
/// and the run log makes that visible rather than pretending the pass was complete.
pub async fn run_once(
    state: &AppState,
    site_id: Uuid,
    actor: Option<Uuid>,
) -> std::result::Result<RunOutcome, ApiError> {
    let pool = state.db().pool();
    let site = site_of(state, site_id).await?;

    let window_row = omnion_media::site_policy(pool, site_id).await?;
    let window = omnion_media::Window {
        keep_versions_days: window_row.keep_versions_days,
        trash_days: window_row.trash_days,
        purge_after_days: window_row.purge_after_days,
    };

    let run_id = omnion_media::begin_retention_run(pool, Some(site_id), Some(window_row.id), "manual", actor)
        .await?;
    let mut totals = RunTotals::default();

    let versions =
        omnion_media::sweep_versions(pool, site_id, window, omnion_media::RETENTION_SWEEP_BATCH)
            .await?;
    let purge =
        omnion_media::purge_eligible(pool, site_id, window, omnion_media::RETENTION_SWEEP_BATCH)
            .await?;
    totals.absorb(&versions, &purge);

    // The refusals are asked of the crate rather than recomputed here, and this is the whole
    // point of the split: the query that decides "may this file go" lives in exactly one
    // place, so a hand-emptied trash and a nightly sweep cannot disagree about the same row.
    let (refusals, refused_count, held) =
        omnion_media::purge_candidates(pool, site_id, window, omnion_media::RETENTION_SWEEP_BATCH)
            .await?;
    totals.refused = refused_count;
    totals.held_back += held + purge.held_back;

    // Bytes first, then the rows are already gone — the crate deleted them inside
    // `purge_eligible`, so this loop is the *storage* half. A store that refuses is a warning
    // and not an error: the row is gone either way, and an operator who retries cannot find
    // the key to retry with, so failing the whole run would be a lie about what happened.
    for key in versions.storage_keys.iter().chain(purge.storage_keys.iter()) {
        if let Err(error) = state.storage().delete(key).await {
            tracing::warn!(error = %error, key, "a retention-removed object could not be deleted");
        }
    }

    let mut error = String::new();
    if totals.versions_removed > 0 || totals.purged > 0 {
        if let Err(failure) = bus::emit(
            pool,
            NewEvent::new("media.retention_applied")
                .organization(site.organization_id)
                .site(site_id)
                .actor(actor)
                .payload(json!({
                    "site_id": site_id,
                    "run_id": run_id,
                    "versions_removed": totals.versions_removed,
                    "versions_bytes": totals.versions_bytes,
                    "purged": totals.purged,
                    "purged_bytes": totals.purged_bytes,
                    "refused": totals.refused,
                    "held_back": totals.held_back,
                })),
        )
        .await
        {
            // The work is done and the rows are written; a bus that refused to take the
            // announcement is recorded in the run rather than returned as a failure, because
            // an operator who sees "the run failed" will re-run a sweep that already happened.
            error = format!("the work was done but the event could not be published: {failure}");
        }
    }
    omnion_media::finish_retention_run(pool, run_id, &totals, &error).await?;

    let run = omnion_media::list_retention_runs(pool, site_id, 1)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "retention_run_unreadable",
                "the retention run could not be read back",
            )
        })?;
    let (remaining_files, remaining_bytes) = omnion_media::trash_summary(pool, site_id).await?;

    Ok(RunOutcome {
        run_id,
        run: RunBody::build(&run),
        remaining_files,
        remaining_bytes,
        refused: refusals.iter().map(RefusalBody::from).collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The path of a folder, for the screen's scope column.
async fn scope_path(state: &AppState, folder_id: Option<Uuid>) -> std::result::Result<Option<String>, ApiError> {
    let Some(folder_id) = folder_id else {
        return Ok(None);
    };
    let folder = omnion_media::find_folder(state.db().pool(), folder_id).await?;
    Ok(folder.map(|row| row.path))
}

/// Write one audit entry, with the site already resolved.
#[allow(clippy::too_many_arguments)]
async fn audit(
    state: &AppState,
    user_id: Uuid,
    site: &omnion_identity::Site,
    ip: &str,
    action: &'static str,
    target_kind: &'static str,
    target_id: Uuid,
    metadata: serde_json::Value,
) -> std::result::Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(user_id, action)
            .target(target_kind, target_id.to_string())
            .metadata(metadata)
            .ip_address(if ip.is_empty() { None } else { Some(ip.to_string()) })
            .organization(site.organization_id),
    )
    .await?;
    Ok(())
}

/// The `404` a missing policy answers.
///
/// `404` and not `403` for the same reason the grant delete learned it: a policy belonging to
/// another tenant must not be distinguishable from one that does not exist, or the id becomes
/// an oracle.
fn policy_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "retention_policy_not_found",
        "no such retention policy on this site",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states of `folder_id` must stay three states.
    ///
    /// The trap is serde, not the type. `Option<Option<Uuid>>` *looks* like it carries "absent"
    /// and "null" separately, and it does not: `#[serde(default)]` cannot tell a missing key
    /// from a key whose value is `null`, both arrive as `None`, and the explicit `null` — the
    /// one edit an operator most often wants, "this rule was for one folder, now it is for
    /// everything" — silently does nothing. The screen then reports "saved" over an unchanged
    /// scope, which is the worst shape a save can have. This walk is the reason the raw body
    /// is inspected rather than the typed struct trusted.
    #[test]
    fn a_null_folder_id_means_the_whole_site_and_not_absent() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"name":"Campaign","folder_id":null}"#).expect("valid body");
        assert_eq!(
            PolicyInput::decode(&body).expect("valid").folder_id,
            Some(None),
            "an explicit null clears the scope"
        );

        let body: serde_json::Value = serde_json::from_str(r#"{"name":"Campaign"}"#).expect("valid body");
        assert_eq!(
            PolicyInput::decode(&body).expect("valid").folder_id,
            None,
            "an absent field leaves the scope alone"
        );

        let body: serde_json::Value = serde_json::from_str(
            r#"{"name":"Campaign","folder_id":"0d3f0000-0000-0000-0000-000000000001"}"#,
        )
        .expect("valid body");
        assert_eq!(
            PolicyInput::decode(&body).expect("valid").folder_id,
            Some(Some(Uuid::parse_str("0d3f0000-0000-0000-0000-000000000001").expect("a uuid"))),
            "a uuid names a folder"
        );
    }

    /// A `folder_id` that is neither a uuid nor null names the field rather than failing as
    /// a decode error, because the settings form renders the message under that input.
    #[test]
    fn a_folder_id_that_is_not_a_uuid_names_its_field() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"folder_id":"campaign"}"#).expect("valid json");
        let error = PolicyInput::decode(&body).expect_err("a slug is not a folder id");
        assert_eq!(error.code(), "invalid_retention_setting");
        // `ApiError` has no `Display`; the message is reached through its own accessor rather
        // than through a `Debug` print, because a `{:?}` in an assertion message prints the
        // whole struct and the failure diff becomes unreadable.
        assert!(
            error.message().contains("folder_id"),
            "the refusal has to name the field: {:?}",
            error
        );
    }

    /// An absent window is "leave it alone" on every field, not "set it to zero".
    #[test]
    fn an_absent_window_does_not_become_a_zero() {
        let body: serde_json::Value = serde_json::from_str(r#"{"name":"Campaign"}"#).expect("valid body");
        let new = PolicyInput::decode(&body).expect("valid").into_new();
        assert_eq!(new.keep_versions_days, None);
        assert_eq!(new.trash_days, None);
        assert_eq!(new.purge_after_days, None);
        assert_eq!(new.legal_hold, None);
        assert_eq!(new.enabled, None);
    }

    /// A refusal names the record, both through the crate's own description and through the
    /// wire body — the screen renders the second and the audit log writes the first, and two
    /// spellings of the same refusal is how an operator ends up with two stories.
    #[test]
    fn a_refusal_reaches_the_wire_in_one_spelling() {
        let row = PurgeRefusal {
            media_id: Uuid::nil(),
            filename: "hero.png".to_string(),
            resource_kind: "page".to_string(),
            resource_id: "0d3f".to_string(),
            field: "hero_image_id".to_string(),
        };
        let body = RefusalBody::from(&row);
        assert_eq!(body.describe, row.describe());
        assert_eq!(body.describe, "page 0d3f (hero_image_id)");
    }

    /// The run log is serialised from a run, and a run that removed nothing must not read as
    /// a broken worker: the summary has to carry the reason it removed nothing.
    #[test]
    fn a_run_body_never_reports_a_bare_zero() {
        let run = RetentionRun {
            id: Uuid::nil(),
            site_id: None,
            policy_id: None,
            kind: "daily".to_string(),
            versions_removed: 0,
            versions_bytes: 0,
            purged: 0,
            purged_bytes: 0,
            refused: 2,
            held_back: 0,
            error: String::new(),
            actor_user_id: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
        };
        let body = RunBody::build(&run);
        assert!(body.summary.contains("still referenced"), "{}", body.summary);
        assert_eq!(body.bytes_reclaimed, 0);
        // `finished_at` has to survive into the wire, because "the last run is still going"
        // and "the last run finished and found nothing" are different states on a screen.
        assert!(body.finished_at.is_some());
    }
}
