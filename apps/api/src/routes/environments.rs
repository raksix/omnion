//! `/api/v1/environments` — the staging environments of an organization
//! (docs/requests/REQ-017, slices 1 and 2).
//!
//! Reading the list and one environment is `deployment.read`; creating one, re-cloning it and
//! cancelling a clone is `deployment.preview`; archiving one is `deployment.rollback`. Three
//! keys rather than one, and the split is the point: *looking* at a staging environment, *filling*
//! it and *throwing it away* are three different amounts of trust, and a single key would let an
//! account that only wanted to check whether a change was ready also delete the copy it was
//! checking.
//!
//! Everything below the HTTP boundary is in `omnion_environment`: the key and host rules, the
//! area plan, the progress fold, the store and the copy itself. This file's jobs are tenancy,
//! permission, turning a field-level refusal into a `400` that names the field, and the audit
//! entry — plus one thing that can only be decided here, which is whether the caller is asking
//! about a staging environment at all (`environment_not_staging`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_environment::changes::{ChangeItem, ChangeSet};
use omnion_environment::clone::{self, Area};
use omnion_environment::error::EnvironmentError;
use omnion_environment::key;
use omnion_environment::model::{CloneStatus, EnvironmentStatus, EnvironmentType};
use omnion_environment::runner;
use omnion_environment::store::{self, CloneJobRow, EnvironmentRow, NewEnvironment};
use omnion_events::NewEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/environments`.
#[derive(Debug, Deserialize)]
pub struct CreateEnvironmentInput {
    /// Display name, 1–64 characters.
    pub name: String,
    /// The key, when the operator overrode the derived one.
    #[serde(default)]
    pub key: Option<String>,
    /// The host staging content is served from.
    #[serde(default)]
    pub staging_host: Option<String>,
    /// The areas the first clone copies. Empty is refused, not defaulted.
    #[serde(default)]
    pub areas: Vec<String>,
    /// Whether archived pages are left behind.
    #[serde(default)]
    pub exclude_archived: bool,
}

/// Query of `GET /api/v1/environments`.
#[derive(Debug, Default, Deserialize)]
pub struct EnvironmentListQuery {
    /// `production` or `staging`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// `active`, `cloning`, `error` or `archived`.
    #[serde(default)]
    pub status: Option<String>,
    /// Substring of the name or key.
    #[serde(default)]
    pub search: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Row offset.
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Body of `POST /api/v1/environments/{id}/clone`.
#[derive(Debug, Default, Deserialize)]
pub struct CloneInput {
    /// The areas to copy. Empty means "the areas of the last clone", which is what a retry means.
    #[serde(default)]
    pub areas: Vec<String>,
    /// Whether archived pages are left behind.
    #[serde(default)]
    pub exclude_archived: bool,
    /// The operator confirmed that staging changes are discarded.
    ///
    /// Required, not advisory. A re-clone empties the environment, and the rows it removes are
    /// edits the operator made in staging — the exact work this request exists to keep safe.
    /// Without the flag the request is refused with a message naming what will be lost.
    #[serde(default)]
    pub discard_confirmed: bool,
}

/// An environment as the list and detail screens see it.
#[derive(Debug, Serialize)]
pub struct EnvironmentBody {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// The key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// `production` or `staging`.
    pub r#type: String,
    /// `active`, `cloning`, `error` or `archived`.
    pub status: String,
    /// The environment this one was cloned from.
    pub cloned_from_environment_id: Option<Uuid>,
    /// When the last clone finished.
    pub cloned_at: Option<OffsetDateTime>,
    /// The staging host, when it has one.
    pub staging_host: Option<String>,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
    /// What the environment holds, for the list screen's "Content" column.
    pub content: ContentCountsBody,
    /// What the current or last clone job is doing.
    pub clone: Option<CloneJobBody>,
    /// Whether the panel should offer a re-clone. Computed here, not in the panel, so a button
    /// cannot appear on a row where the request would be refused.
    pub reclonable: bool,
}

/// Per-area content counts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContentCountsBody {
    /// Pages.
    pub pages: i64,
    /// Translations.
    pub translations: i64,
    /// Workflow definitions.
    pub workflows: i64,
    /// Settings rows.
    pub settings: i64,
    /// Revisions.
    pub revisions: i64,
    /// Everything a clone would carry.
    pub total: i64,
}

impl From<store::ContentCounts> for ContentCountsBody {
    fn from(counts: store::ContentCounts) -> Self {
        Self {
            total: counts.pages
                + counts.translations
                + counts.workflows
                + counts.settings
                + counts.revisions,
            pages: counts.pages,
            translations: counts.translations,
            workflows: counts.workflows,
            settings: counts.settings,
            revisions: counts.revisions,
        }
    }
}

/// A clone job as the Overview tab and the list row see it.
#[derive(Debug, Serialize)]
pub struct CloneJobBody {
    /// Primary key.
    pub id: Uuid,
    /// The environment it fills.
    pub environment_id: Uuid,
    /// `pending`, `running`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// The areas, in copy order.
    pub areas: Vec<AreaBody>,
    /// Rows copied.
    pub items_done: i32,
    /// Rows expected.
    pub items_total: i32,
    /// Completion, 0–100. Zero while the total is still unknown rather than a hopeful hundred.
    pub percent: u8,
    /// The one-line summary under the bar.
    pub summary: String,
    /// The error, verbatim.
    pub error: Option<String>,
    /// When a worker claimed it.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
    /// Who asked.
    pub created_by: Option<Uuid>,
    /// When they asked.
    pub created_at: OffsetDateTime,
    /// Whether a cancel button should be offered.
    pub cancellable: bool,
}

/// One area of a clone job, with the label the wizard and the Overview tab use.
#[derive(Debug, Serialize)]
pub struct AreaBody {
    /// The wire name.
    pub name: String,
    /// The label a person reads.
    pub label: String,
    /// Rows expected.
    pub total: u64,
    /// Rows copied.
    pub done: u64,
}

impl CloneJobBody {
    /// Project a stored job.
    #[must_use]
    pub fn build(job: &CloneJobRow) -> Self {
        let status = job.state();
        let progress = job.progress();
        let counts = progress.area_counts();
        let areas = job
            .areas
            .iter()
            .filter_map(|raw| Area::parse(raw))
            .map(|area| AreaBody {
                name: area.as_str().to_string(),
                label: area.label().to_string(),
                total: progress.total.get(&area).copied().unwrap_or(0),
                done: counts.get(&area).copied().unwrap_or(0),
            })
            .collect();
        Self {
            id: job.id,
            environment_id: job.environment_id,
            status: status.as_str().to_string(),
            areas,
            items_done: job.items_done,
            items_total: job.items_total,
            percent: progress.percent(),
            summary: progress.summary(),
            error: job.error.clone(),
            started_at: job.started_at,
            finished_at: job.finished_at,
            created_by: job.created_by,
            created_at: job.created_at,
            cancellable: status.is_open(),
        }
    }
}

/// Response of `GET /api/v1/environments`.
#[derive(Debug, Serialize)]
pub struct EnvironmentListResponse {
    /// The page.
    pub environments: Vec<EnvironmentBody>,
    /// How many match the filter in total.
    pub total: i64,
    /// The areas the wizard offers, with their labels and what they cost.
    pub areas: Vec<AreaOptionBody>,
    /// The organization's production environment, for the wizard's "clone from" line.
    pub source_key: String,
}

/// One checkbox in the create wizard.
#[derive(Debug, Serialize)]
pub struct AreaOptionBody {
    /// The wire name.
    pub name: String,
    /// The label.
    pub label: String,
    /// What it costs, in words rather than a fake number.
    pub weight: String,
    /// Whether ticking this actually copies anything, and why not when it does not.
    ///
    /// This is the field the whole change turns on. The wizard used to render six
    /// identically-shaped checkboxes over three areas that copy and three that return `0` from
    /// the runner, so a tick on "Site settings" was accepted, stored on the job, priced in the
    /// estimate and then quietly did nothing. The request's Definition of Done forbids dead
    /// controls, and this was a live one — so the answer travels with the option and the panel
    /// renders it, rather than being left to a comment in the runner.
    pub copies: bool,
    /// The reason, for the areas that copy nothing. `None` for the ones that do.
    pub note: Option<String>,
}

/// Response of `GET /api/v1/environments/{id}`.
#[derive(Debug, Serialize)]
pub struct EnvironmentDetailResponse {
    /// The environment.
    pub environment: EnvironmentBody,
    /// The job history, newest first.
    pub jobs: Vec<CloneJobBody>,
    /// The estimate for the next clone, from the production counts.
    pub estimate: String,
}

/// Response of `POST /api/v1/environments/{id}/clone-jobs/{job_id}/cancel`.
#[derive(Debug, Serialize)]
pub struct CancelCloneResponse {
    /// The job as it now stands.
    pub job: CloneJobBody,
    /// The environment's status after the cancel.
    pub environment_status: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/environments` — the list screen.
pub async fn list_environments(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<EnvironmentListQuery>,
) -> Result<Json<EnvironmentListResponse>, ApiError> {
    let organization_id = organization_of(&current)?;

    // An unreadable filter chip is *no* filter rather than a `400`: a chip the panel is still
    // transitioning should show the unfiltered table, not an error banner over a screen that
    // works. The same rule the CDN purge list runs on, for the same reason.
    let filter = store::EnvironmentFilter {
        kind: query.r#type.as_deref().and_then(EnvironmentType::parse),
        status: query.status.as_deref().and_then(EnvironmentStatus::parse),
        search: query.search.clone().filter(|raw| !raw.trim().is_empty()),
        limit: query.limit.unwrap_or(store::EnvironmentFilter::DEFAULT_LIMIT),
        offset: query.offset.unwrap_or(0),
    };

    let pool = state.db().pool();
    let page = store::list(pool, organization_id, &filter).await?;
    let production = store::production(pool, organization_id).await?;

    let mut environments = Vec::with_capacity(page.environments.len());
    for row in &page.environments {
        environments.push(environment_body(pool, row).await?);
    }

    Ok(Json(EnvironmentListResponse {
        environments,
        total: page.total,
        areas: Area::ALL
            .iter()
            .map(|area| AreaOptionBody {
                name: area.as_str().to_string(),
                label: area.label().to_string(),
                weight: area.weight().to_string(),
                copies: area.copies(),
                note: area.note().map(str::to_string),
            })
            .collect(),
        source_key: production.key,
    }))
}

/// `POST /api/v1/environments` — the wizard's last step.
pub async fn create_environment(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Json(input): axum::extract::Json<CreateEnvironmentInput>,
) -> Result<(StatusCode, Json<EnvironmentBody>), ApiError> {
    let organization_id = organization_of(&current)?;

    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(ApiError::bad_request(
            "environment_name_invalid",
            "The name must be between 1 and 64 characters.",
        ));
    }

    // Derive when absent, validate when present. Deriving is not the same as defaulting: a key
    // the operator did not choose has to come from the name they did choose, or two environments
    // called "Staging" would collide on a made-up suffix.
    let requested_key = input
        .key
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| key::derive_key(&name));
    let checked_key = key::check_key(&requested_key).map_err(|err| field_error(err, "key"))?;

    let staging_host = match input.staging_host.as_deref().map(str::trim) {
        Some(host) if !host.is_empty() => Some(key::check_staging_host(host).map_err(|err| field_error(err, "staging_host"))?),
        _ => None,
    };

    let areas = parse_areas(&input.areas)?;
    if areas.is_empty() {
        return Err(ApiError::bad_request(
            "clone_areas_required",
            clone::require_areas(&[]).unwrap_err(),
        ));
    }

    let pool = state.db().pool();
    let source = store::production(pool, organization_id).await?;

    let new = NewEnvironment {
        organization_id,
        key: checked_key,
        name,
        cloned_from_environment_id: source.id,
        staging_host,
        created_by: Some(current.user.id),
        areas,
        exclude_archived: input.exclude_archived,
    };
    let (environment, job) = store::create_staging(pool, &new).await?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "environment.created")
            .target("environment", environment.id.to_string())
            .metadata(json!({
                "key": environment.key,
                "name": environment.name,
                "cloned_from": source.key,
                "areas": job.areas,
                "staging_host": environment.staging_host,
            })),
    )
    .await?;
    omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("environment.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "environment_id": environment.id,
                "key": environment.key,
                "type": environment.r#type,
            })),
    )
    .await?;
    omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("environment.clone.started")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "environment_id": environment.id,
                "job_id": job.id,
                "areas": job.areas,
            })),
    )
    .await?;

    let body = environment_body(pool, &environment).await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// `GET /api/v1/environments/{id}` — the detail screen.
pub async fn get_environment(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<EnvironmentDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let environment = store::find(pool, organization_id, id).await?;

    let jobs = store::list_jobs(pool, id, 20).await?;
    let body = environment_body(pool, &environment).await?;
    let source_counts = match environment.cloned_from_environment_id {
        Some(source) => store::content_counts(pool, source).await?,
        None => store::ContentCounts::default(),
    };
    let rows = source_counts.pages
        + source_counts.translations
        + source_counts.workflows
        + source_counts.settings
        + source_counts.revisions;

    Ok(Json(EnvironmentDetailResponse {
        environment: body,
        jobs: jobs.iter().map(CloneJobBody::build).collect(),
        estimate: runner::estimate_note(rows),
    }))
}

/// `POST /api/v1/environments/{id}/clone` — re-clone from production.
pub async fn start_clone(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    axum::extract::Json(input): axum::extract::Json<CloneInput>,
) -> Result<(StatusCode, Json<CloneJobBody>), ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let environment = store::find(pool, organization_id, id).await?;

    if environment.kind() != EnvironmentType::Staging {
        return Err(EnvironmentError::NotStaging {
            key: environment.key,
        }
        .into());
    }
    if environment.state() == EnvironmentStatus::Archived {
        return Err(EnvironmentError::Archived {
            key: environment.key,
        }
        .into());
    }
    // The source check is repeated here even though the source id is on the row: a row whose
    // `cloned_from_environment_id` points at a staging environment would otherwise be copied
    // staging→staging, which is the nesting the request refuses by name.
    if let Some(source_id) = environment.cloned_from_environment_id {
        let source = store::find(pool, organization_id, source_id).await?;
        if !source.kind().can_be_clone_source() {
            return Err(EnvironmentError::StagingNestingRefused {
                source_key: source.key,
            }
            .into());
        }
    }

    // Empty means "the areas of the last clone", which is what a retry means — an operator
    // pressing "retry" on a failed clone wants the same job again, not an empty one that copies
    // nothing and reports success.
    let areas = if input.areas.is_empty() {
        let previous = store::list_jobs(pool, id, 1).await?;
        previous
            .first()
            .map(|job| job.areas.iter().filter_map(|raw| Area::parse(raw)).collect())
            .unwrap_or_else(|| Area::ALL.to_vec())
    } else {
        parse_areas(&input.areas)?
    };
    if areas.is_empty() {
        return Err(ApiError::bad_request(
            "clone_areas_required",
            clone::require_areas(&[]).unwrap_err(),
        ));
    }

    // The count of what a clone would empty, so the operator can be told what they are about to
    // lose before the request is accepted. Refusing without this is refusing on trust.
    let discard = store::content_counts(pool, id).await?;
    let discard_rows = discard.pages + discard.translations + discard.workflows + discard.settings;
    if discard_rows > 0 && !input.discard_confirmed {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "clone_discard_unconfirmed",
            format!(
                "Re-cloning empties this environment first: {discard_rows} staging rows that are \
                 not in production would be discarded. Confirm to continue."
            ),
        )
        .with_details(json!({
            "discarded_pages": discard.pages,
            "discarded_translations": discard.translations,
            "discarded_workflows": discard.workflows,
            "discarded_settings": discard.settings,
            "requires": "discard_confirmed",
        })));
    }

    let job = store::open_clone_job(
        pool,
        &environment,
        &areas,
        input.exclude_archived,
        Some(current.user.id),
    )
    .await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "environment.clone.requested")
            .target("environment", environment.id.to_string())
            .metadata(json!({
                "job_id": job.id,
                "areas": job.areas,
                "discarded_rows": discard_rows,
                "exclude_archived": input.exclude_archived,
            })),
    )
    .await?;
    omnion_events::bus::emit(
        pool,
        NewEvent::new("environment.clone.started")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "environment_id": environment.id,
                "job_id": job.id,
                "areas": job.areas,
            })),
    )
    .await?;

    Ok((StatusCode::ACCEPTED, Json(CloneJobBody::build(&job))))
}

/// `GET /api/v1/environments/{id}/clone-jobs` — the job history and the live progress.
pub async fn list_clone_jobs(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<CloneJobBody>>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    // The read of the environment is the tenancy check: an environment of another organization
    // answers `404` here, before any job row is touched.
    store::find(pool, organization_id, id).await?;
    let jobs = store::list_jobs(pool, id, 20).await?;
    Ok(Json(jobs.iter().map(CloneJobBody::build).collect()))
}

/// `POST /api/v1/environments/{id}/clone-jobs/{job_id}/cancel` — stop a running clone.
pub async fn cancel_clone(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<CancelCloneResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let environment = store::find(pool, organization_id, id).await?;

    let job = store::read_job(pool, job_id).await?;
    if job.environment_id != environment.id {
        // A job that belongs to another environment is a `404`, not a `403`: the caller is
        // allowed to act in this organization, and the only fact that is wrong is that this job
        // is not here.
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            EnvironmentError::NotFound.code(),
            "That clone job does not exist in this environment.",
        ));
    }

    let cancelled = store::cancel_job(pool, &job).await?;
    let refreshed = store::find(pool, organization_id, id).await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "environment.clone.cancelled")
            .target("environment", environment.id.to_string())
            .metadata(json!({
                "job_id": job.id,
                "status": cancelled.status,
                "items_done": cancelled.items_done,
            })),
    )
    .await?;

    Ok(Json(CancelCloneResponse {
        job: CloneJobBody::build(&cancelled),
        environment_status: refreshed.status,
    }))
}

/// `DELETE /api/v1/environments/{id}` — archive: content kept, host released.
pub async fn archive_environment(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<EnvironmentBody>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    let environment = store::find(pool, organization_id, id).await?;

    if environment.kind() != EnvironmentType::Staging {
        // Archiving production would take the tenant's live content out of service. The route
        // says so by name rather than by `400 bad request`.
        return Err(EnvironmentError::NotStaging {
            key: environment.key,
        }
        .into());
    }

    let archived = store::archive(pool, &environment).await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "environment.archived")
            .target("environment", environment.id.to_string())
            .metadata(json!({
                "key": environment.key,
                "released_host": environment.staging_host,
                "content_kept": true,
            })),
    )
    .await?;
    omnion_events::bus::emit(
        pool,
        NewEvent::new("environment.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "environment_id": environment.id,
                "key": environment.key,
            })),
    )
    .await?;

    let body = environment_body(pool, &archived).await?;
    Ok(Json(body))
}

/// `GET /api/v1/environments/{id}/changes` — what this staging environment holds that production
/// does not.
///
/// Read-only in slice 2. The route exists and answers a real change set because the Changes tab
/// is useless without it, and because slice 3's promotion freezes exactly what this returns — so
/// the comparison has to be one function with one definition of "changed" rather than two that can
/// disagree.
pub async fn list_changes(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ChangeSetBody>, ApiError> {
    let organization_id = organization_of(&current)?;
    let pool = state.db().pool();
    // Tenancy first, and it is the read of the environment that does it: an environment of
    // another organization answers `404` here, before a single page row is compared.
    let environment = store::find(pool, organization_id, id).await?;

    // The reference is the environment this one was *cloned from*, not "the organization's
    // production environment". The two agree on the first clone and diverge on every later one:
    // a re-clone from a staging environment would otherwise be compared against production and
    // the tab would show a diff of the wrong thing. `staging_source_refused` in the model is the
    // rule that keeps a staging source out, so the field is set for every staging environment.
    let production_id = environment.cloned_from_environment_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "environment_no_clone_source",
            "This environment has no clone source, so there is nothing to compare it against yet.",
        )
    })?;
    // The source has to still exist and still be in this organization. A deleted source leaves the
    // change set unanswerable rather than empty, and reporting "no changes" there would be the
    // most dangerous answer the screen can give.
    store::find(pool, organization_id, production_id).await?;

    let change_set =
        omnion_environment::changes::diff_against_production(pool, id, production_id).await?;

    Ok(Json(ChangeSetBody::build(
        &environment,
        &change_set,
    )))
}

/// The change set, in the shape the Changes tab reads.
///
/// `ChangeItem` is re-exported rather than re-declared: the panel must not be able to drift from
/// the server's idea of what a row is, and slice 3 freezes this exact list.
#[derive(Debug, Serialize)]
pub struct ChangeSetBody {
    /// The staging environment the items belong to.
    pub environment_id: Uuid,
    /// The environment's key, so the tab's header can name it without a second request.
    pub environment_key: String,
    /// The environment it is compared against — the one it was cloned from.
    pub production_id: Uuid,
    /// One row per changed page.
    pub items: Vec<ChangeItem>,
    /// Count of `added` items, so the tab header does not recount client-side.
    pub added: i64,
    /// Count of `updated` items.
    pub updated: i64,
    /// Count of `deleted` items.
    pub deleted: i64,
    /// `true` when nothing differs. The tab says so in words rather than showing an empty table
    /// with no explanation, because an empty change set after a clone is the expected state.
    pub empty: bool,
}

impl ChangeSetBody {
    /// Flatten the store's change set into the wire shape.
    pub fn build(
        environment: &store::EnvironmentRow,
        change_set: &ChangeSet,
    ) -> ChangeSetBody {
        ChangeSetBody {
            environment_id: change_set.environment_id,
            environment_key: environment.key.clone(),
            production_id: change_set.production_id,
            items: change_set.items.clone(),
            added: change_set.added,
            updated: change_set.updated,
            deleted: change_set.deleted,
            empty: change_set.is_empty(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Build the body of one environment, with its counts and its current job.
///
/// The job picked is the newest one, open or finished, because the list screen's progress bar
/// shows the running clone and the Overview tab shows the last outcome — and "newest" is the one
/// row that answers both.
async fn environment_body(
    pool: &sqlx::PgPool,
    environment: &EnvironmentRow,
) -> Result<EnvironmentBody, ApiError> {
    let content = store::content_counts(pool, environment.id).await?;
    let jobs = store::list_jobs(pool, environment.id, 1).await?;
    let kind = environment.kind();
    let state = environment.state();

    Ok(EnvironmentBody {
        id: environment.id,
        organization_id: environment.organization_id,
        key: environment.key.clone(),
        name: environment.name.clone(),
        r#type: kind.as_str().to_string(),
        status: state.as_str().to_string(),
        cloned_from_environment_id: environment.cloned_from_environment_id,
        cloned_at: environment.cloned_at,
        staging_host: environment.staging_host.clone(),
        created_by: environment.created_by,
        created_at: environment.created_at,
        updated_at: environment.updated_at,
        content: ContentCountsBody::from(content),
        clone: jobs.first().map(CloneJobBody::build),
        reclonable: kind == EnvironmentType::Staging && state != EnvironmentStatus::Archived,
    })
}

/// The organization the caller acts in.
///
/// A platform-level account (no primary organization) has no organization to answer for, and
/// the environment surface is organization-scoped end to end — there is no "all organizations"
/// listing of somebody's staging copies, because a staging environment is a tenant's own content
/// and a platform operator has no business guessing which tenant's staging to show.
pub fn organization_of(current: &CurrentSession) -> Result<Uuid, ApiError> {
    current.user.organization_id.ok_or_else(|| {
        ApiError::forbidden(
            "organization_required",
            "Environments belong to an organization; this account has none.",
        )
    })
}

/// Turn wire area names into areas, refusing an unknown one by name.
fn parse_areas(raw: &[String]) -> Result<Vec<Area>, ApiError> {
    let mut areas = Vec::with_capacity(raw.len());
    for name in raw {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let area = Area::parse(trimmed).ok_or_else(|| {
            let legal: Vec<&str> = Area::ALL.iter().map(|area| area.as_str()).collect();
            ApiError::bad_request(
                "clone_area_unknown",
                format!(
                    "“{trimmed}” is not something a clone copies. Choose from: {}.",
                    legal.join(", ")
                ),
            )
            .with_details(json!({ "areas": legal }))
        })?;
        areas.push(area);
    }
    Ok(clone::plan_areas(&areas, false))
}

/// A key or host refusal as a `400` that names the field it is about.
fn field_error(error: EnvironmentError, field: &str) -> ApiError {
    ApiError::bad_request("environment_field_invalid", error.to_string())
        .with_details(json!({ "field": field, "reason": error.code() }))
}

impl ApiError {
    /// The message an environment refusal carries, without moving the error.
    fn message_of(error: &EnvironmentError) -> String {
        error.to_string()
    }
}

impl From<EnvironmentError> for ApiError {
    /// Environments (docs/requests/REQ-017): a refusal the caller can act on is a `4xx` that
    /// carries the error's own `code`, because the panel switches on that name — a staging key
    /// taken sends the operator back to step 1 of the wizard and an environment that is not
    /// staging sends them to the list, and one shared "invalid environment" string would send
    /// both to the same place.
    fn from(error: EnvironmentError) -> Self {
        match error {
            EnvironmentError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "environment_not_found",
                "That environment does not exist in this organization.",
            ),
            EnvironmentError::KeyTaken { .. }
            | EnvironmentError::HostTaken { .. }
            | EnvironmentError::InvalidKey { .. }
            | EnvironmentError::StagingNestingRefused { .. }
            | EnvironmentError::NotStaging { .. }
            | EnvironmentError::Archived { .. }
            | EnvironmentError::CloneAlreadyRunning { .. }
            | EnvironmentError::PromotionNotPending { .. }
            | EnvironmentError::PromotionAlreadyRunning { .. } => {
                Self::new(StatusCode::CONFLICT, error.code(), Self::message_of(&error))
            }
            EnvironmentError::PromotionConflict { ref items } => {
                // The message is built before the borrow of `items` is taken, so the conflict
                // list is still whole when it goes into the details.
                let message = Self::message_of(&error);
                Self::new(StatusCode::CONFLICT, "promotion_conflict", message)
                    .with_details(json!({ "items": items }))
            }
            EnvironmentError::SelfApprovalRefused => {
                Self::new(StatusCode::FORBIDDEN, error.code(), Self::message_of(&error))
            }
            EnvironmentError::Store { message } => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("the environment store failed: {message}"),
            ),
        }
    }
}

/// Read the clone status a job ended in, for the runner's log line and its tests.
#[must_use]
pub fn ended_in(status: &str) -> CloneStatus {
    CloneStatus::parse(status).unwrap_or(CloneStatus::Failed)
}
