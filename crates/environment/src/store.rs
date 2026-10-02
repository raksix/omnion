//! Database access for environments and their clone jobs (REQ-017).
//!
//! Everything a request about staging needs to *read* and everything the clone has to *write*
//! lives here, so `apps/api`'s route file is only tenancy, permission, status codes and audit.
//!
//! Two things this module is careful about, and both come from the same fact: a staging
//! environment is a **copy**, not a parallel installation. That means
//!
//!   * a clone must be idempotent — re-cloning an unchanged environment produces the same rows
//!     and no duplicates, because a duplicate row in staging shows up as a second "added" item
//!     in the Changes tab and reads to the operator as an edit they did not make; and
//!   * a clone must never reach production. Every copy statement filters on the source
//!     environment, and every statement that could plausibly be misread is written so that
//!     omitting the filter would copy *nothing* rather than *everything*. A query that forgets
//!     its `where environment_id = $1` and copies the whole table is a cross-environment leak;
//!     a query that forgets an inner join and copies nothing is a failed clone job that the
//!     progress bar reports honestly.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::clone::{Area, Progress};
use crate::error::EnvironmentError;
use crate::key;
use crate::model::{CloneStatus, EnvironmentStatus, EnvironmentType};

/// Column list every environment query selects.
const ENVIRONMENT_COLUMNS: &str = "id, organization_id, key, name, type, status, \
     cloned_from_environment_id, cloned_at, staging_host, created_by, created_at, updated_at";

/// Column list every clone-job query selects.
const CLONE_JOB_COLUMNS: &str = "id, environment_id, status, areas, items_total, items_done, \
     area_counts, exclude_archived, error, started_at, finished_at, created_by, created_at";

/// A stored environment, as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EnvironmentRow {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// The slug-like key, unique per organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// `production` or `staging`.
    pub r#type: String,
    /// `active`, `cloning`, `error` or `archived`.
    pub status: String,
    /// The environment this one was cloned from, when there was one.
    pub cloned_from_environment_id: Option<Uuid>,
    /// When the last clone finished.
    pub cloned_at: Option<OffsetDateTime>,
    /// The host staging content is served from.
    pub staging_host: Option<String>,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl EnvironmentRow {
    /// The environment's type.
    ///
    /// An unreadable value is `Staging` rather than `Production`: the fallback is the answer that
    /// *refuses* things, and defaulting to production would hand an environment whose type the
    /// database could not vouch for the one identity that may be cloned into.
    #[must_use]
    pub fn kind(&self) -> EnvironmentType {
        EnvironmentType::parse(&self.r#type).unwrap_or(EnvironmentType::Staging)
    }

    /// The environment's status.
    #[must_use]
    pub fn state(&self) -> EnvironmentStatus {
        EnvironmentStatus::parse(&self.status).unwrap_or(EnvironmentStatus::Error)
    }

    /// May this environment be written to right now?
    #[must_use]
    pub fn accepts_writes(&self) -> bool {
        self.state().accepts_writes()
    }
}

/// A stored clone job.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CloneJobRow {
    /// Primary key.
    pub id: Uuid,
    /// The environment being filled.
    pub environment_id: Uuid,
    /// `pending`, `running`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// The areas, in copy order.
    pub areas: Vec<String>,
    /// Rows expected.
    pub items_total: i32,
    /// Rows copied.
    pub items_done: i32,
    /// Per-area counts as json, so the Overview tab needs no second query per area.
    pub area_counts: serde_json::Value,
    /// Whether archived pages were left behind.
    pub exclude_archived: bool,
    /// The failure, verbatim.
    pub error: Option<String>,
    /// When a worker claimed it.
    pub started_at: Option<OffsetDateTime>,
    /// When it reached a terminal state.
    pub finished_at: Option<OffsetDateTime>,
    /// Who asked.
    pub created_by: Option<Uuid>,
    /// When they asked.
    pub created_at: OffsetDateTime,
}

impl CloneJobRow {
    /// The job's status.
    #[must_use]
    pub fn state(&self) -> CloneStatus {
        CloneStatus::parse(&self.status).unwrap_or(CloneStatus::Failed)
    }

    /// Rebuild the progress tally from the row.
    ///
    /// The stored `area_counts` is the source and the two integer columns are re-derived from it,
    /// not the other way round. A runner that advanced the integers and forgot the json would
    /// produce a bar that moves and a list of per-area counts that do not, and the Overview tab
    /// shows both — so one of them has to be authoritative and it has to be the one written last.
    #[must_use]
    pub fn progress(&self) -> Progress {
        let mut progress = Progress::new();
        if let Some(map) = self.area_counts.as_object() {
            for (name, value) in map {
                let Some(area) = Area::parse(name) else {
                    continue;
                };
                let Some(count) = value.as_u64() else {
                    continue;
                };
                progress.expect(area, count);
                progress.advance(area, count);
            }
        }
        // Areas the runner has not discovered yet still need to be on the bar, or the total
        // silently shrinks as the job finishes and the percentage jumps backwards.
        for area in self.areas.iter().filter_map(|raw| Area::parse(raw)) {
            progress.expect(area, progress.done.get(&area).copied().unwrap_or(0));
        }
        if self.state() == CloneStatus::Failed {
            // The failing area is whatever area is short of its own total, and when nothing is
            // short the job failed before counting anything — the Overview tab says "stopped at"
            // in that case rather than naming an area that had not started.
            let short = self
                .areas
                .iter()
                .filter_map(|raw| Area::parse(raw))
                .find(|area| {
                    let done = progress.done.get(area).copied().unwrap_or(0);
                    let total = progress.total.get(area).copied().unwrap_or(0);
                    done < total
                });
            if let Some(area) = short {
                progress.fail(area);
            }
        }
        progress
    }
}

/// The filter `list` applies.
#[derive(Debug, Clone, Default)]
pub struct EnvironmentFilter {
    /// Restrict to one type.
    pub kind: Option<EnvironmentType>,
    /// Restrict to one status.
    pub status: Option<EnvironmentStatus>,
    /// Case-insensitive substring of the name or key.
    pub search: Option<String>,
    /// Page size.
    pub limit: i64,
    /// Row offset.
    pub offset: i64,
}

impl EnvironmentFilter {
    /// The cap the list route applies when the caller sends no size.
    pub const DEFAULT_LIMIT: i64 = 50;
    /// The largest page the list route will return.
    pub const MAX_LIMIT: i64 = 200;
}

/// A page of environments plus the count that matches the filter, not the page.
#[derive(Debug, Clone)]
pub struct EnvironmentPage {
    /// The rows in this page.
    pub environments: Vec<EnvironmentRow>,
    /// How many rows match in total.
    pub total: i64,
}

/// A new staging environment, before the database has an opinion about it.
#[derive(Debug, Clone)]
pub struct NewEnvironment {
    /// The organization it belongs to.
    pub organization_id: Uuid,
    /// The key, already validated by [`crate::key::check_key`].
    pub key: String,
    /// The display name.
    pub name: String,
    /// The environment it is a copy of.
    pub cloned_from_environment_id: Uuid,
    /// The host staging content is served from.
    pub staging_host: Option<String>,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// The areas the first clone will copy.
    pub areas: Vec<Area>,
    /// Whether the first clone leaves archived pages behind.
    pub exclude_archived: bool,
}

/// Read one environment, scoped to its organization.
///
/// Scoped in the `where` clause rather than fetched and compared: a fetch-then-compare returns
/// the row to a caller that is about to be refused, and the refusal then has to be built from a
/// row it should never have held.
pub async fn find(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<EnvironmentRow, EnvironmentError> {
    let sql = format!(
        "select {ENVIRONMENT_COLUMNS} from environments \
         where id = $1 and organization_id = $2"
    );
    sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?
        .ok_or(EnvironmentError::NotFound)
}

/// Read one environment by id with no organization filter.
///
/// **For the internal runner only.** Every route-facing read goes through [`find`], which
/// carries the caller's organization in the `where` clause. This one exists because the clone
/// worker starts from a job row and has no caller to scope by — it is a background task acting
/// on a row it was handed, and adding an organization argument it would have to invent would be
/// the same mistake as making it optional. It is not exported to any route.
pub async fn find_any(pool: &PgPool, id: Uuid) -> Result<EnvironmentRow, EnvironmentError> {
    let sql = format!("select {ENVIRONMENT_COLUMNS} from environments where id = $1");
    sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?
        .ok_or(EnvironmentError::NotFound)
}

/// Read one environment by its key, scoped to its organization.
pub async fn find_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<EnvironmentRow>, EnvironmentError> {
    let sql = format!(
        "select {ENVIRONMENT_COLUMNS} from environments \
         where organization_id = $1 and key = $2"
    );
    let row = sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(organization_id)
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    Ok(row)
}

/// The organization's production environment.
///
/// This is the clone's source and the promotion's target, so it is read by the routes that both
/// need; a staging environment cloned from a staging environment is refused by
/// [`EnvironmentType::can_be_clone_source`] before this is ever consulted.
pub async fn production(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<EnvironmentRow, EnvironmentError> {
    let sql = format!(
        "select {ENVIRONMENT_COLUMNS} from environments \
         where organization_id = $1 and type = 'production'"
    );
    sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(organization_id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?
        .ok_or(EnvironmentError::NotFound)
}

/// A page of the organization's environments.
pub async fn list(
    pool: &PgPool,
    organization_id: Uuid,
    filter: &EnvironmentFilter,
) -> Result<EnvironmentPage, EnvironmentError> {
    let limit = if filter.limit <= 0 {
        EnvironmentFilter::DEFAULT_LIMIT
    } else {
        filter.limit.min(EnvironmentFilter::MAX_LIMIT)
    };
    let offset = filter.offset.max(0);
    let search = filter.search.as_deref().map(str::to_lowercase);

    // The `where` is written so that a null filter is *no* filter rather than *nothing*: the
    // panel sends only the chips that are set, and a filter that matched on null would make the
    // unticked chips look like a search for the literal string "null".
    let total: i64 = sqlx::query_scalar(
        "select count(*) from environments \
         where organization_id = $1 \
           and ($2::text is null or type = $2) \
           and ($3::text is null or status = $3) \
           and ($4::text is null or lower(name) like '%' || $4 || '%' or lower(key) like '%' || $4 || '%')",
    )
    .bind(organization_id)
    .bind(filter.kind.map(EnvironmentType::as_str))
    .bind(filter.status.map(EnvironmentStatus::as_str))
    .bind(search.as_deref())
    .fetch_one(pool)
    .await
    .map_err(store_error)?;

    let sql = format!(
        "select {ENVIRONMENT_COLUMNS} from environments \
         where organization_id = $1 \
           and ($2::text is null or type = $2) \
           and ($3::text is null or status = $3) \
           and ($4::text is null or lower(name) like '%' || $4 || '%' or lower(key) like '%' || $4 || '%') \
         order by (type = 'production') desc, created_at asc, id asc \
         limit $5 offset $6"
    );
    let environments = sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(organization_id)
        .bind(filter.kind.map(EnvironmentType::as_str))
        .bind(filter.status.map(EnvironmentStatus::as_str))
        .bind(search.as_deref())
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(store_error)?;

    Ok(EnvironmentPage {
        environments,
        total,
    })
}

/// Per-area content counts of one environment, as the list screen's "Content" column shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentCounts {
    /// Pages in the environment.
    pub pages: i64,
    /// Translations in the environment.
    pub translations: i64,
    /// Workflow definitions in the environment.
    pub workflows: i64,
    /// Settings rows in the environment.
    pub settings: i64,
    /// Revisions reachable through the environment's pages.
    pub revisions: i64,
}

/// Count what an environment holds.
///
/// One statement rather than five so the list screen's row is a single round trip and so the
/// counts cannot describe two different moments: five queries read five instants, and a clone
/// running between two of them yields a row whose page count and translation count belong to
/// different points in the copy.
pub async fn content_counts(
    pool: &PgPool,
    environment_id: Uuid,
) -> Result<ContentCounts, EnvironmentError> {
    let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "select
            (select count(*) from pages where environment_id = $1),
            (select count(*) from translations where environment_id = $1),
            (select count(*) from workflows where environment_id = $1),
            (select count(*) from organization_settings where environment_id = $1),
            (select count(*) from page_revisions r
               join pages p on p.id = r.page_id where p.environment_id = $1)",
    )
    .bind(environment_id)
    .fetch_one(pool)
    .await
    .map_err(store_error)?;
    Ok(ContentCounts {
        pages: row.0,
        translations: row.1,
        workflows: row.2,
        settings: row.3,
        revisions: row.4,
    })
}

/// Create a staging environment in `cloning` and open its first clone job.
///
/// Both rows are written in one transaction because a staging environment with no job is an
/// environment the operator is told is cloning and that nothing will ever fill — a state the
/// request's own list screen has no status for, and which the next clone request would then
/// refuse with `clone_already_running` for a job that does not exist.
pub async fn create_staging(
    pool: &PgPool,
    new: &NewEnvironment,
) -> Result<(EnvironmentRow, CloneJobRow), EnvironmentError> {
    if let Some(existing) = find_by_key(pool, new.organization_id, &new.key).await? {
        return Err(EnvironmentError::KeyTaken {
            key: existing.key,
        });
    }
    if let Some(host) = new.staging_host.as_deref() {
        let sql = format!(
            "select {ENVIRONMENT_COLUMNS} from environments where staging_host = $1 limit 1"
        );
        let taken: Option<EnvironmentRow> = sqlx::query_as(&sql)
            .bind(host)
            .fetch_optional(pool)
            .await
            .map_err(store_error)?;
        if let Some(other) = taken {
            return Err(EnvironmentError::HostTaken {
                host: other.staging_host.unwrap_or_else(|| host.to_string()),
            });
        }
    }

    let mut tx = pool.begin().await.map_err(store_error)?;

    let environment_sql = format!(
        "insert into environments \
           (organization_id, key, name, type, status, cloned_from_environment_id, staging_host, created_by) \
         values ($1, $2, $3, 'staging', 'cloning', $4, $5, $6) returning {ENVIRONMENT_COLUMNS}"
    );
    let environment = sqlx::query_as::<_, EnvironmentRow>(&environment_sql)
        .bind(new.organization_id)
        .bind(&new.key)
        .bind(&new.name)
        .bind(new.cloned_from_environment_id)
        .bind(new.staging_host.as_deref())
        .bind(new.created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_error)?;

    let job = insert_job(
        &mut tx,
        pool,
        environment.id,
        &new.areas,
        new.exclude_archived,
        new.created_by,
    )
    .await?;

    tx.commit().await.map_err(store_error)?;
    Ok((environment, job))
}

/// Open a clone job for an existing environment.
///
/// The `clone_already_running` refusal is raised by the database's partial unique index on
/// `environment_clone_jobs`, not by a read-then-write here: a check in this function is correct
/// until two clone requests arrive together, and then both of them are told they may proceed and
/// one of them silently overwrites the other's work.
pub async fn open_clone_job(
    pool: &PgPool,
    environment: &EnvironmentRow,
    areas: &[Area],
    exclude_archived: bool,
    created_by: Option<Uuid>,
) -> Result<CloneJobRow, EnvironmentError> {
    let mut tx = pool.begin().await.map_err(store_error)?;
    let job = insert_job(&mut tx, pool, environment.id, areas, exclude_archived, created_by).await;
    match job {
        Ok(job) => {
            set_status(&mut tx, environment.id, EnvironmentStatus::Cloning).await?;
            tx.commit().await.map_err(store_error)?;
            Ok(job)
        }
        Err(err) => Err(err),
    }
}

/// Insert one job row inside a caller's transaction.
///
/// The `clone_already_running` refusal comes from the database's partial unique index, and the
/// *lookup* of the running job happens in a **new** transaction, not this one. That is not a
/// style choice: in PostgreSQL a failed statement aborts the whole transaction, so the obvious
/// version — insert, catch the error, read the open job in the same transaction — answers
/// "current transaction is aborted" and turns a `409` into a `500`. The database already knows
/// which job is open; the only reason to ask is to name it, and a name is not worth losing the
/// refusal over.
async fn insert_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pool: &PgPool,
    environment_id: Uuid,
    areas: &[Area],
    exclude_archived: bool,
    created_by: Option<Uuid>,
) -> Result<CloneJobRow, EnvironmentError> {
    let names: Vec<String> = areas.iter().map(|area| area.as_str().to_string()).collect();
    let sql = format!(
        "insert into environment_clone_jobs \
           (environment_id, status, areas, exclude_archived, created_by) \
         values ($1, 'pending', $2, $3, $4) returning {CLONE_JOB_COLUMNS}"
    );
    let result = sqlx::query_as::<_, CloneJobRow>(&sql)
        .bind(environment_id)
        .bind(&names)
        .bind(exclude_archived)
        .bind(created_by)
        .fetch_one(&mut **tx)
        .await;

    match result {
        Ok(job) => Ok(job),
        Err(sqlx::Error::Database(err)) if is_open_clone_violation(err.as_ref()) => {
            // Report the *running* job rather than a bare "already running", so the detail
            // drawer can show the operator which clone they are being refused by and the
            // cancel button has an id to act on. Read on the pool, because `tx` is finished.
            let open = open_job(pool, environment_id).await?;
            Err(EnvironmentError::CloneAlreadyRunning {
                job_id: open.map_or_else(|| "unknown".to_string(), |row| row.id.to_string()),
            })
        }
        Err(err) => Err(store_error(err)),
    }
}

/// Read the open job of an environment, if it has one.
async fn open_job(
    pool: &PgPool,
    environment_id: Uuid,
) -> Result<Option<CloneJobRow>, EnvironmentError> {
    let sql = format!(
        "select {CLONE_JOB_COLUMNS} from environment_clone_jobs \
         where environment_id = $1 and status in ('pending','running') \
         order by created_at desc limit 1"
    );
    let row = sqlx::query_as::<_, CloneJobRow>(&sql)
        .bind(environment_id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    Ok(row)
}

/// Set an environment's status.
async fn set_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    environment_id: Uuid,
    status: EnvironmentStatus,
) -> Result<(), EnvironmentError> {
    sqlx::query("update environments set status = $2, updated_at = now() where id = $1")
        .bind(environment_id)
        .bind(status.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    Ok(())
}

/// Claim the oldest open job, for the clone runner.
///
/// `for update skip locked` in one statement: re-reading the claimed ids afterwards races the
/// update that just claimed them, and two API processes would drain the same job.
pub async fn claim_next_job(pool: &PgPool) -> Result<Option<CloneJobRow>, EnvironmentError> {
    let sql = format!(
        "update environment_clone_jobs set status = 'running', started_at = now() \
         where id = ( \
             select id from environment_clone_jobs \
             where status = 'pending' order by created_at asc, id asc \
             for update skip locked limit 1 \
         ) returning {CLONE_JOB_COLUMNS}"
    );
    let row = sqlx::query_as::<_, CloneJobRow>(&sql)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    Ok(row)
}

/// Record the progress of a running job.
///
/// Written per area rather than per batch because the panel's progress bar is polled by the
/// operator watching it: a job that only reports at the end is a job whose bar sits at 0% for
/// two minutes and then jumps, which is indistinguishable from a hang.
pub async fn record_progress(
    pool: &PgPool,
    job_id: Uuid,
    progress: &Progress,
) -> Result<(), EnvironmentError> {
    let counts: BTreeMap<&str, u64> = progress
        .area_counts()
        .into_iter()
        .map(|(area, count)| (area.as_str(), count))
        .collect();
    sqlx::query(
        "update environment_clone_jobs \
         set items_done = $2, items_total = $3, area_counts = $4 \
         where id = $1",
    )
    .bind(job_id)
    .bind(i32::try_from(progress.items_done()).unwrap_or(i32::MAX))
    .bind(i32::try_from(progress.items_total()).unwrap_or(i32::MAX))
    .bind(serde_json::to_value(&counts).unwrap_or_else(|_| serde_json::json!({})))
    .execute(pool)
    .await
    .map_err(store_error)?;
    Ok(())
}

/// Finish a job and move its environment to the status the outcome implies.
///
/// The environment's status follows the job's rather than being set by the caller: the caller's
/// two arguments can disagree, and the disagreement is what leaves a panel showing "active"
/// above a job that failed.
pub async fn finish_job(
    pool: &PgPool,
    job: &CloneJobRow,
    outcome: CloneStatus,
    error: Option<&str>,
) -> Result<(), EnvironmentError> {
    let mut tx = pool.begin().await.map_err(store_error)?;

    sqlx::query(
        "update environment_clone_jobs \
         set status = $2, error = $3, finished_at = now() where id = $1",
    )
    .bind(job.id)
    .bind(outcome.as_str())
    .bind(error)
    .execute(&mut *tx)
    .await
    .map_err(store_error)?;

    let environment_status = match outcome {
        // A cancelled clone leaves the environment `error`, not `active`: the copy is partial
        // and the operator has to be told. Marking it active is the one answer that would make
        // a half-copied environment look like a finished one.
        CloneStatus::Done => EnvironmentStatus::Active,
        CloneStatus::Failed | CloneStatus::Cancelled => EnvironmentStatus::Error,
        CloneStatus::Pending | CloneStatus::Running => EnvironmentStatus::Cloning,
    };
    set_status(&mut tx, job.environment_id, environment_status).await?;

    if matches!(outcome, CloneStatus::Done) {
        sqlx::query("update environments set cloned_at = now() where id = $1")
            .bind(job.environment_id)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
    }

    tx.commit().await.map_err(store_error)?;
    Ok(())
}

/// Cancel a job, refusing one that has already finished.
pub async fn cancel_job(
    pool: &PgPool,
    job: &CloneJobRow,
) -> Result<CloneJobRow, EnvironmentError> {
    let sql = format!(
        "update environment_clone_jobs \
         set status = 'cancelled', finished_at = now() \
         where id = $1 and status in ('pending','running') returning {CLONE_JOB_COLUMNS}"
    );
    let row = sqlx::query_as::<_, CloneJobRow>(&sql)
        .bind(job.id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    let Some(row) = row else {
        // The `where` matched nothing because the job finished between the read and the write.
        // Returning the current state (rather than a refusal) is what lets the panel show a
        // "done" job the operator tried to cancel as done, instead of as an error.
        return read_job(pool, job.id).await;
    };
    let _ = set_status_standalone(pool, job.environment_id, EnvironmentStatus::Error).await;
    Ok(row)
}

/// Read the environment a **staging host** addresses, across organizations.
///
/// This is the one read in this module that is deliberately *not* scoped by organization, and
/// it is scoped by something else instead: the host. A visitor reaches staging by typing its
/// address, long before they have a session or an organization, so the public read surface has
/// nothing to scope by — and pretending otherwise would be a different lie, because the answer
/// is a single row (or none) chosen by an exact host match, and it is a host the installation
/// itself published. The header this produces is the *only* thing the caller may learn from it:
/// whether this address is staging at all, so a search engine is told not to index it. The
/// environment's own name, key, counts and clone history never travel with it.
///
/// The lookup answers `Ok(None)` for an unknown host rather than an error, because "no staging
/// environment owns this address" is the ordinary case for every production request.
pub async fn find_staging_by_host(
    pool: &PgPool,
    host: &str,
) -> Result<Option<EnvironmentRow>, EnvironmentError> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Ok(None);
    }
    let sql = format!(
        "select {ENVIRONMENT_COLUMNS} from environments \
         where staging_host = $1 and status <> 'archived' limit 1"
    );
    let row = sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(&host)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    Ok(row)
}

/// Set an environment's status outside a transaction.
async fn set_status_standalone(
    pool: &PgPool,
    environment_id: Uuid,
    status: EnvironmentStatus,
) -> Result<(), EnvironmentError> {
    sqlx::query("update environments set status = $2, updated_at = now() where id = $1")
        .bind(environment_id)
        .bind(status.as_str())
        .execute(pool)
        .await
        .map_err(store_error)?;
    Ok(())
}

/// Read one clone job by id.
pub async fn read_job(pool: &PgPool, id: Uuid) -> Result<CloneJobRow, EnvironmentError> {
    let sql = format!("select {CLONE_JOB_COLUMNS} from environment_clone_jobs where id = $1");
    sqlx::query_as::<_, CloneJobRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?
        .ok_or(EnvironmentError::NotFound)
}

/// The job history of one environment, newest first.
pub async fn list_jobs(
    pool: &PgPool,
    environment_id: Uuid,
    limit: i64,
) -> Result<Vec<CloneJobRow>, EnvironmentError> {
    let sql = format!(
        "select {CLONE_JOB_COLUMNS} from environment_clone_jobs \
         where environment_id = $1 order by created_at desc, id desc limit $2"
    );
    let rows = sqlx::query_as::<_, CloneJobRow>(&sql)
        .bind(environment_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(pool)
        .await
        .map_err(store_error)?;
    Ok(rows)
}

/// Archive an environment: content kept, host released, writes refused.
///
/// The host is set to null rather than the row being deleted, because "archive (content kept,
/// host released)" is the request's own words: a deleted staging environment takes its pages
/// with it through the cascade, and the operator who archived to free a hostname discovers the
/// content went with it.
pub async fn archive(
    pool: &PgPool,
    environment: &EnvironmentRow,
) -> Result<EnvironmentRow, EnvironmentError> {
    let sql = format!(
        "update environments \
         set status = 'archived', staging_host = null, updated_at = now() \
         where id = $1 returning {ENVIRONMENT_COLUMNS}"
    );
    let row = sqlx::query_as::<_, EnvironmentRow>(&sql)
        .bind(environment.id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    row.ok_or(EnvironmentError::NotFound)
}

/// Derive a staging key from a name that has none yet.
///
/// Returns an empty string rather than a placeholder: the wizard shows the derived key as *the
/// value the field will get*, and a `staging-1` style filler that the operator has to overwrite
/// is indistinguishable from a real suggestion. An empty field is obviously empty.
#[must_use]
pub fn suggest_key(name: &str) -> String {
    key::derive_key(name)
}

/// `true` when the database refused a second open clone for one environment.
fn is_open_clone_violation(error: &(dyn sqlx::error::DatabaseError + 'static)) -> bool {
    // The partial unique index has its own name, so the refusal is matched by that name rather
    // than by the generic unique-violation code: the same code covers the environment key and the
    // staging host, and reporting "already running" for a key collision sends the operator to
    // the wrong screen.
    error
        .constraint()
        .is_some_and(|name| name == "environment_clone_jobs_single_open")
}

fn store_error(error: sqlx::Error) -> EnvironmentError {
    EnvironmentError::Store {
        message: error.to_string(),
    }
}
