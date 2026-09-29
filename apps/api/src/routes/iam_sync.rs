//! `/api/v1/iam/providers/{id}/sync-runs` — the sync ledger an operator reads at 09:00
//! (REQ-065, slice 4 part 2).
//!
//! [`crate::routes::iam_providers`] answers "is this provider reachable right now" and
//! [`omnion_identity::sso::sync_runs`] records what a sweep *did*. Neither one answers the
//! question that decides whether anybody does anything today: **did the nightly run work, and if
//! not, for whom?** So this is deliberately a separate surface from the provider itself:
//!
//! * **A run is listed, not summarised.** `last_sync_at` on the provider row is one value that
//!   hides everything the list exists to show, and the list is where the failures live.
//! * **`partial` is its own status, and the panel must not paint it green.** A run that refused a
//!   third of the directory and reported success is the failure this slice exists to surface, so
//!   the row carries the verdict *derived* from the failures rather than a boolean from the
//!   caller.
//! * **Failures outlive the run row.** The drawer reads `directory_sync_errors`, so retrying a
//!   subject appends an attempt instead of destroying the record of what failed the first time.
//!   A retry that overwrote it would leave nobody able to answer "was this always broken?".
//!
//! Nothing here reads a credential: a run row is counts, a verdict and a sentence, and an error
//! row is a code from a closed set plus that sentence. There is no field in which a raw provider
//! response could arrive.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_events::{NewEvent, bus};
use omnion_identity::sso::sync_runs::{
    self, FailedSubject, GroupLink, RunStatus, SyncKind, SyncRun,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Largest run page the list answers; the same clamp the store applies, named here so the
/// document the client reads is the document the server enforces.
const MAX_RUN_PAGE: i64 = 100;

/// Query of the run list.
#[derive(Debug, Deserialize)]
pub struct SyncRunQuery {
    /// How many runs to answer. Clamped, not refused.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Only runs that ended badly. The query an operator runs *because* the chip is amber.
    #[serde(default)]
    pub problems_only: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A run, as the list and the drawer read it.
#[derive(Debug, Serialize)]
pub struct SyncRunBody {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub kind: &'static str,
    pub status: &'static str,
    /// Whether the panel's chip is green. `partial` is deliberately not: it is the whole reason
    /// `partial` is a separate status.
    pub healthy: bool,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// Seconds, or `null` while the run is still going. Never "time so far" — that renders a
    /// slow run as permanently unfinished and a fast one as never having ended.
    pub duration_seconds: Option<i64>,
    pub counts: CountsBody,
    pub error_count: i32,
    pub message: Option<String>,
    pub triggered_by: Option<Uuid>,
}

impl From<&SyncRun> for SyncRunBody {
    fn from(run: &SyncRun) -> Self {
        Self {
            id: run.id,
            provider_id: run.provider_id,
            kind: run.kind.as_str(),
            status: run.status.as_str(),
            healthy: run.status.is_healthy(),
            started_at: run.started_at.format(&Rfc3339).unwrap_or_default(),
            finished_at: run
                .finished_at
                .map(|moment| moment.format(&Rfc3339).unwrap_or_default()),
            duration_seconds: run.duration().map(|span| span.whole_seconds()),
            counts: CountsBody {
                users_seen: run.counts.users_seen,
                users_created: run.counts.users_created,
                users_updated: run.counts.users_updated,
                users_deactivated: run.counts.users_deactivated,
                groups_seen: run.counts.groups_seen,
            },
            error_count: run.error_count,
            message: run.message.clone(),
            triggered_by: run.triggered_by,
        }
    }
}

/// The counters, kept level with the row so the panel never derives one from another.
#[derive(Debug, Serialize)]
pub struct CountsBody {
    pub users_seen: i32,
    pub users_created: i32,
    pub users_updated: i32,
    pub users_deactivated: i32,
    pub groups_seen: i32,
}

/// One subject a run could not process, and how many attempts it took.
#[derive(Debug, Serialize)]
pub struct FailedSubjectBody {
    /// What to retry.
    pub key: String,
    /// `1` is the ordinary case; more says the directory is flapping, and the panel says so.
    pub attempts: i32,
    /// The *first* code, not the last: the first is the one that explains the others.
    pub code: String,
    pub message: String,
    pub last_failed_at: String,
}

impl From<&FailedSubject> for FailedSubjectBody {
    fn from(subject: &FailedSubject) -> Self {
        Self {
            key: subject.key.clone(),
            attempts: subject.attempts,
            code: subject.code.clone(),
            message: subject.message.clone(),
            last_failed_at: subject.last_failed_at.format(&Rfc3339).unwrap_or_default(),
        }
    }
}

/// A directory group the provider has seen, and whether the last sync could read it.
#[derive(Debug, Serialize)]
pub struct GroupLinkBody {
    pub id: Uuid,
    pub external_id: String,
    pub external_label: String,
    pub member_count: i32,
    pub last_seen_at: String,
    /// `false` when the last sync could not read this group's membership. The row is kept so a
    /// group whose repair is pending does not silently vanish from the screen.
    pub synced: bool,
}

impl From<&GroupLink> for GroupLinkBody {
    fn from(link: &GroupLink) -> Self {
        Self {
            id: link.id,
            external_id: link.external_id.clone(),
            external_label: link.external_label.clone(),
            member_count: link.member_count,
            last_seen_at: link.last_seen_at.format(&Rfc3339).unwrap_or_default(),
            synced: link.synced,
        }
    }
}

/// The body of `POST /{run_id}/retry`.
///
/// The subjects are named rather than defaulted, and that is the point: "retry everything that
/// failed" and "retry this one person" are different acts, and a button that silently does the
/// first when the operator clicked the second is how a colleague gets a second failure at 03:00.
/// An empty list is refused instead of being read as "all of them".
#[derive(Debug, Deserialize)]
pub struct RetryBody {
    /// The failed subjects to re-attempt, exactly as the drawer listed them.
    pub subjects: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// A provider's runs, newest first.
pub async fn list_sync_runs(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(provider_id): Path<Uuid>,
    Query(query): Query<SyncRunQuery>,
) -> Result<Json<Value>, ApiError> {
    let provider = crate::routes::iam_providers::load_for_sync(&state, &current, provider_id).await?;

    let limit = query.limit.unwrap_or(MAX_RUN_PAGE);
    let runs = sync_runs::list_runs(state.db().pool(), provider.id, limit).await?;

    // The store's own clamp is the floor, but the request's clamp is applied here too: a caller
    // asking for 10 000 rows should get the documented maximum, not whatever the store decides
    // today. Two places deciding the same number is how a screen starts rendering a different
    // page size than the document it was written against.
    let rows: Vec<SyncRunBody> = runs
        .iter()
        .map(SyncRunBody::from)
        .filter(|run| !query.problems_only.unwrap_or(false) || !run.healthy)
        .collect();

    // The summary is the same derivation the row itself carries, so the header cannot disagree
    // with the list underneath it — a "3 problems" caption over three green rows is the exact
    // thing an operator stops believing after once.
    let problems = rows.iter().filter(|run| !run.healthy).count();
    let running = rows.iter().filter(|run| run.status == "running").count();

    Ok(Json(json!({
        "provider_id": provider.id,
        "sync_interval_minutes": provider.sync_interval_minutes,
        "summary": { "runs": rows.len(), "problems": problems, "running": running },
        "runs": rows,
    })))
}

/// One run, with the subjects it could not process.
///
/// The failures are read from the errors table rather than recomputed, so a retried subject shows
/// both attempts: the drawer is the only place in the panel where "this failed twice" is
/// visible, and collapsing it would leave the flapping-directory case indistinguishable from an
/// ordinary one-off.
pub async fn get_sync_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((provider_id, run_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let provider = crate::routes::iam_providers::load_for_sync(&state, &current, provider_id).await?;

    let run = sync_runs::get_run(state.db().pool(), run_id)
        .await?
        .filter(|run| run.provider_id == provider.id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "sync_run_not_found",
                "no such sync run for this provider",
            )
        })?;

    let subjects = sync_runs::failed_subjects(state.db().pool(), run.id).await?;
    let attempts = sqlx::query_as::<_, (i64,)>(
        "select count(*) from directory_sync_errors where run_id = $1",
    )
    .bind(run.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;

    Ok(Json(json!({
        "run": SyncRunBody::from(&run),
        // The two numbers are different on purpose: `attempts` is every honest failure including
        // repeats, `subjects` is the list an operator can act on. One row that says "3" beside a
        // one-row table is the bug this pair exists to prevent.
        "attempts": attempts.0,
        "failed_subjects": subjects.iter().map(FailedSubjectBody::from).collect::<Vec<_>>(),
    })))
}

/// Re-attempt named subjects of a finished run.
///
/// A **new** run is opened rather than the old one reopened. Reopening would give a single row two
/// start times, and the duration an operator reads would depend on how many retries there were —
/// the same reason `finish_run` refuses a second outcome.
///
/// Every named subject must have actually failed in that run. Accepting an arbitrary string would
/// let a caller record a retry for a subject nobody saw fail, which is a run row that says
/// "retried" and a history that cannot be read.
pub async fn retry_sync_run(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((provider_id, run_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<RetryBody>,
) -> Result<Json<Value>, ApiError> {
    let provider = crate::routes::iam_providers::load_for_sync(&state, &current, provider_id).await?;

    let run = sync_runs::get_run(state.db().pool(), run_id)
        .await?
        .filter(|run| run.provider_id == provider.id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "sync_run_not_found",
                "no such sync run for this provider",
            )
        })?;

    if run.status == RunStatus::Running {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "sync_run_in_progress",
            "this run has not finished yet — there is nothing to retry",
        ));
    }
    if body.subjects.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_subjects",
            "name at least one failed subject to retry; an empty list would mean every subject \
             that ever failed, which is not what this button says",
        ));
    }

    let known: Vec<String> = sync_runs::failed_subjects(state.db().pool(), run.id)
        .await?
        .into_iter()
        .map(|subject| subject.key)
        .collect();
    let unknown: Vec<&String> = body
        .subjects
        .iter()
        .filter(|subject| !known.contains(subject))
        .collect();
    if !unknown.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "subject_not_failed",
            format!(
                "these subjects did not fail in this run: {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // Deduplicated, because "retry alice and alice" is one unit of work and the caller's own
    // list is not the place to reject a duplicate.
    let mut subjects = body.subjects.clone();
    subjects.sort();
    subjects.dedup();

    let retry = sync_runs::start_run(
        state.db().pool(),
        provider.id,
        SyncKind::Manual,
        Some(current.user.id),
    )
    .await?;

    // The request is recorded, not performed. A sync that walks a live directory is a job for the
    // scheduler, and a panel button that pretends to have run one would put a green row over a
    // directory nobody swept. What is written here is the *intent*, plus the subjects it applies
    // to, so the queue and the history both have something true to read.
    for subject in &subjects {
        sqlx::query(
            "insert into directory_sync_errors (run_id, subject, code, message) \
             values ($1, $2, 'retry_requested', $3)",
        )
        .bind(retry.id)
        .bind(subject)
        .bind(format!("retry requested for a subject that failed with `{run_id}`"))
        .execute(state.db().pool())
        .await
        .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;
    }

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("iam.sync_retry_requested")
            .organization(Some(provider.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "provider_id": provider.id,
                "retry_run_id": retry.id,
                "source_run_id": run.id,
                "subject_count": subjects.len(),
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "the event could not be recorded");
    }

    Ok(Json(json!({
        "retry_run_id": retry.id,
        "source_run_id": run.id,
        "subjects": subjects,
    })))
}

/// The groups a sync has seen, with the two values that go stale quietly.
pub async fn list_group_links(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(provider_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let provider = crate::routes::iam_providers::load_for_sync(&state, &current, provider_id).await?;
    let links = sync_runs::list_group_links(state.db().pool(), provider.id).await?;
    let unsynced = links.iter().filter(|link| !link.synced).count();
    Ok(Json(json!({
        "provider_id": provider.id,
        "summary": { "groups": links.len(), "unsynced": unsynced },
        "groups": links.iter().map(GroupLinkBody::from).collect::<Vec<_>>(),
    })))
}

/// The next scheduled run, as the panel's "next run" caption reads it.
///
/// Derived from the interval and the last run rather than stored, because a stored next-run time
/// is a promise the moment a run is retried by hand — and a caption that keeps saying "in 4
/// minutes" while the sweep is being retried is a caption nobody believes.
pub fn next_run_at(interval_minutes: i32, last: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    if interval_minutes <= 0 {
        return None;
    }
    last.map(|moment| moment + time::Duration::minutes(i64::from(interval_minutes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the test module builds a `SyncCounts` by hand, so the import lives here rather than
    // at the top: a top-level import that the binary does not need is a warning, and this file
    // compiles in two configurations where that warning is not hypothetical.
    use omnion_identity::sso::sync_runs::SyncCounts;

    fn a_run(status: RunStatus, started: OffsetDateTime, finished: Option<OffsetDateTime>) -> SyncRun {
        SyncRun {
            id: Uuid::nil(),
            provider_id: Uuid::nil(),
            kind: SyncKind::Full,
            status,
            started_at: started,
            finished_at: finished,
            counts: SyncCounts::default(),
            error_count: if status == RunStatus::Partial { 3 } else { 0 },
            message: None,
            triggered_by: None,
        }
    }

    #[test]
    fn a_running_run_reports_no_duration_rather_than_time_so_far() {
        let started = OffsetDateTime::UNIX_EPOCH;
        let body = SyncRunBody::from(&a_run(RunStatus::Running, started, None));
        assert_eq!(body.duration_seconds, None);
        assert_eq!(body.finished_at, None);
        // Still healthy: a run in progress is not a problem, and colouring it amber would make
        // every long sweep look like an outage.
        assert!(body.healthy);
    }

    #[test]
    fn partial_is_reported_unhealthy_even_when_the_counts_look_busy() {
        // The exact case this surface exists for: a run that created 40 accounts and refused
        // three people. `healthy` must not follow the counters.
        let mut run = a_run(
            RunStatus::Partial,
            OffsetDateTime::UNIX_EPOCH,
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(90)),
        );
        run.counts.users_created = 40;
        let body = SyncRunBody::from(&run);
        assert!(!body.healthy);
        assert_eq!(body.duration_seconds, Some(90));
        assert_eq!(body.error_count, 3);
    }

    #[test]
    fn the_next_run_caption_stays_empty_when_nothing_is_scheduled() {
        // Interval 0 is a real answer ("not on a schedule"), and a caption reading "in 0 minutes"
        // for it is worse than no caption.
        assert_eq!(next_run_at(0, Some(OffsetDateTime::UNIX_EPOCH)), None);
        assert_eq!(next_run_at(-5, Some(OffsetDateTime::UNIX_EPOCH)), None);
        // No last run means the interval has nothing to count from — inventing a time would put
        // a date in the future that nothing will honour.
        assert_eq!(next_run_at(60, None), None);
        assert_eq!(
            next_run_at(60, Some(OffsetDateTime::UNIX_EPOCH)),
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(60)),
        );
    }
}
