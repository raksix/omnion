//! Database access for the deployment centre's read surface (REQ-024, slice 1).
//!
//! This is the layer between the six tables in `0211`/`0212` and the HTTP handlers, and it is
//! deliberately the only place that knows the SQL. Three rules shape it:
//!
//! * **A row is returned as an error, never as an empty value.** `load_release("9.9.9")` on a
//!   cache that has never seen it is `Err(NotFound)`, not `Ok(None)` and certainly not a
//!   synthesized `2.0.0` — a release detail screen that rendered a plausible version nobody
//!   published is the worst thing this feature can do.
//! * **The channel filter is part of the key, not a `where` clause that can be forgotten.** A
//!   list of releases for `stable` and a list for `nightly` are different sets, and a caller
//!   that omits the filter gets *the installation's channel*, which is the only safe default:
//!   offering a nightly list to a stable installation is how a downgrade starts.
//! * **Nothing here decides anything.** Version ordering, the channel rule and the three
//!   availability states are in `version.rs`; this module only loads and stores. The one
//!   exception is the `update.available` claim, which is a single `on conflict do nothing`
//!   whose affected-row count *is* the dedupe — a read-modify-write of the seen set in Rust
//!   would reintroduce the race the constraint-free design was avoiding.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::version::{Channel, Release, Version};

/// Column list every release query selects.
const RELEASE_COLUMNS: &str = "version, channel, released_at, notes_md, breaking, migrations, \
     core_min, artifact_checksum, checked_at";

/// One cached release, as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReleaseRow {
    /// The version string, exactly as the feed wrote it.
    pub version: String,
    /// The channel it was published on.
    pub channel: String,
    /// The feed's own timestamp string, unparsed.
    pub released_at: Option<String>,
    /// Rendered plain text.
    pub notes_md: String,
    /// Whether the notes declare breaking changes.
    pub breaking: bool,
    /// Migrations the release ships, in order.
    pub migrations: Vec<String>,
    /// The oldest core that can run it, when declared.
    pub core_min: Option<String>,
    /// The artifact digest.
    pub artifact_checksum: Option<String>,
    /// When this row was last refreshed from the feed.
    pub checked_at: OffsetDateTime,
}

impl ReleaseRow {
    /// The channel, with the same refusal-the-wrong-way default as everywhere else: an
    /// unreadable channel is treated as one that **admits nothing**, so a row whose channel the
    /// database cannot vouch for can never become the card's `Available` line.
    #[must_use]
    pub fn kind(&self) -> Option<Channel> {
        Channel::parse(&self.channel)
    }

    /// Build the crate's release type, or `None` when the row cannot be represented.
    ///
    /// A row whose `version` will not parse is skipped rather than repaired. The alternative —
    /// normalizing `1.10` to `1.10.0` here — invents a version the feed did not publish, and the
    /// card would then offer a release by a number its author never used.
    #[must_use]
    pub fn to_release(&self) -> Option<Release> {
        let version = Version::parse(&self.version).ok()?;
        Some(Release {
            version,
            channel: self.kind()?,
            notes: self.notes_md.clone(),
            breaking: self.breaking,
            migrations: self.migrations.clone(),
            core_min: self.core_min.as_deref().and_then(|raw| Version::parse(raw).ok()),
            artifact_checksum: self.artifact_checksum.clone(),
            released_at: self.released_at.clone(),
        })
    }
}

/// Load every cached release for a channel, newest first.
///
/// Ordering happens in SQL as well as in Rust: the SQL keeps the screen's page stable between
/// two requests on a tie, and `Version::parse` in Rust is the authority for *which* is newer.
pub async fn list_releases(
    pool: &PgPool,
    channel: Channel,
    limit: i64,
) -> Result<Vec<ReleaseRow>, StoreError> {
    let sql = format!(
        "select {RELEASE_COLUMNS} from releases_cache where channel = $1 \
         order by checked_at desc, version desc limit $2"
    );
    let rows = sqlx::query_as::<_, ReleaseRow>(&sql)
        .bind(channel.as_str())
        .bind(limit.clamp(1, 200))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Load one cached release by version, on any channel.
///
/// Channel is part of the argument rather than a filter because a version string can appear on
/// more than one channel, and `releases_cache`'s primary key is the version alone — so a
/// lookup that ignored the channel could return the nightly's notes for a stable install.
pub async fn load_release(
    pool: &PgPool,
    channel: Channel,
    version: &str,
) -> Result<ReleaseRow, StoreError> {
    let sql = format!(
        "select {RELEASE_COLUMNS} from releases_cache where version = $1 and channel = $2"
    );
    let row = sqlx::query_as::<_, ReleaseRow>(&sql)
        .bind(version)
        .bind(channel.as_str())
        .fetch_optional(pool)
        .await?;
    row.ok_or(StoreError::NotFound)
}

/// The newest timestamp in the cache, or `None` when the cache is empty.
///
/// This is the `{time}` in the stale banner. `max(checked_at)` rather than "now minus
/// something": the banner's whole claim is that what you see is what was known *then*, so the
/// number has to come from the rows, not from the clock.
pub async fn newest_checked_at(pool: &PgPool) -> Result<Option<OffsetDateTime>, StoreError> {
    let row: (Option<OffsetDateTime>,) =
        sqlx::query_as("select max(checked_at) from releases_cache")
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}

/// Replace what a check found, per channel.
///
/// A **delete-then-insert** rather than an upsert, and the reason is what the table is for: the
/// cache exists so `/deployment` renders while the feed is unreachable, so a release the feed
/// no longer lists must *disappear* from the screen. An upsert would leave a withdrawn release
/// sitting on the card for ever, and a withdrawn release is exactly the one an operator must not
/// deploy to. Releases the feed omits on one run are also stale rows from a truncated response,
/// which the parser's `rejected` list reports — so a successful parse with a short list is a
/// short list, and the operator sees the shrink.
pub async fn replace_channel(
    pool: &PgPool,
    channel: Channel,
    releases: &[Release],
    released_at: &Option<String>,
) -> Result<usize, StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from releases_cache where channel = $1")
        .bind(channel.as_str())
        .execute(&mut *tx)
        .await?;

    let mut written = 0usize;
    for release in releases {
        let row = sqlx::query(
            "insert into releases_cache (version, channel, released_at, notes_md, breaking, \
             migrations, core_min, artifact_checksum) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (version) do update set \
               channel = excluded.channel, \
               released_at = excluded.released_at, \
               notes_md = excluded.notes_md, \
               breaking = excluded.breaking, \
               migrations = excluded.migrations, \
               core_min = excluded.core_min, \
               artifact_checksum = excluded.artifact_checksum, \
               checked_at = now()",
        )
        .bind(release.version.to_string())
        .bind(release.channel.as_str())
        .bind(release.released_at.as_deref().or(released_at.as_deref()))
        .bind(&release.notes)
        .bind(release.breaking)
        .bind(&release.migrations)
        .bind(release.core_min.as_ref().map(ToString::to_string))
        .bind(release.artifact_checksum.as_deref())
        .execute(&mut *tx)
        .await?;
        written += row.rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(written)
}

/// The single update-check row, created on first read.
///
/// Created on read rather than on boot for the reason the row exists: an installation that has
/// never checked has a real, answerable question ("has anything been looked at?") and the
/// answer is a row with `last_status = null`, which is *not* the same as a row saying the last
/// check failed. `insert … on conflict do nothing` then read is two statements rather than one
/// race between two workers both inserting.
pub async fn load_check(pool: &PgPool) -> Result<UpdateCheck, StoreError> {
    sqlx::query(
        "insert into deployment_update_check (id, channel) values (1, 'stable') \
         on conflict (id) do nothing",
    )
    .execute(pool)
    .await?;
    let row = sqlx::query_as::<_, CheckRow>(
        "select channel, last_run_at, last_finished_at, last_status, last_error, last_seen, \
         last_announced from deployment_update_check where id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(UpdateCheck {
        channel: Channel::parse(&row.channel).unwrap_or(Channel::Stable),
        last_run_at: row.last_run_at,
        last_finished_at: row.last_finished_at,
        last_status: row.last_status,
        last_error: row.last_error,
        last_seen: row.last_seen,
        last_announced: row.last_announced,
    })
}

/// The row as the table holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CheckRow {
    /// The channel this installation follows.
    pub channel: String,
    /// When the last check started.
    pub last_run_at: Option<OffsetDateTime>,
    /// When it finished.
    pub last_finished_at: Option<OffsetDateTime>,
    /// `completed`, `failed`, or `None` when nothing has run.
    pub last_status: Option<String>,
    /// The failure reason, verbatim.
    pub last_error: Option<String>,
    /// How many releases the last successful read carried.
    pub last_seen: Option<i32>,
    /// What the last successful run announced.
    pub last_announced: Vec<String>,
}

/// What the checks screen renders, without the run in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// The channel this installation follows.
    pub channel: Channel,
    /// When the last check started.
    pub last_run_at: Option<OffsetDateTime>,
    /// When it finished.
    pub last_finished_at: Option<OffsetDateTime>,
    /// `completed`, `failed`, or `None`.
    pub last_status: Option<String>,
    /// The failure reason, verbatim.
    pub last_error: Option<String>,
    /// How many releases the last successful read carried.
    pub last_seen: Option<i32>,
    /// What the last successful run announced.
    pub last_announced: Vec<String>,
}

impl UpdateCheck {
    /// Has a check ever completed?
    #[must_use]
    pub fn has_ever_run(&self) -> bool {
        self.last_status.is_some()
    }

    /// Is the cache stale — the condition the banner is for?
    ///
    /// `true` for a **failed** run and for one that has never run, and both are honest reasons
    /// to say "this is what was known when we last heard from the feed". A never-run instance
    /// showing a banner is unusual but true: it has no feed data, and silence is not data.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.last_status.as_deref() != Some("completed")
    }
}

/// Record a completed check.
pub async fn record_completed(
    pool: &PgPool,
    channel: Channel,
    seen: usize,
    announced: &[String],
) -> Result<(), StoreError> {
    sqlx::query(
        "update deployment_update_check set \
           last_run_at = now(), last_finished_at = now(), last_status = 'completed', \
           last_error = null, last_seen = $2, last_announced = $3, channel = $1 \
         where id = 1",
    )
    .bind(channel.as_str())
    .bind(seen as i32)
    .bind(announced)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed check.
///
/// `last_seen` is left alone on purpose: a failed read learned nothing, and overwriting the
/// last good count with a guess would make "the feed carried 14 releases" true of a run that
/// read nothing at all.
pub async fn record_failed(pool: &PgPool, reason: &str) -> Result<(), StoreError> {
    sqlx::query(
        "update deployment_update_check set \
           last_run_at = now(), last_finished_at = now(), last_status = 'failed', \
           last_error = $1 where id = 1",
    )
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim the (channel, version) pairs this check is announcing, and return the ones it won.
///
/// The dedupe, as one statement. `on conflict do nothing` plus the affected-row count is the
/// whole mechanism: two checks running at once both try to insert, one of them gets the rows
/// and the other's count is zero, so `update.available` is emitted once per pair by
/// construction rather than by a read that another process may invalidate between the read and
/// the write.
pub async fn claim_announced(
    pool: &PgPool,
    channel: Channel,
    versions: &[String],
) -> Result<Vec<String>, StoreError> {
    let mut won = Vec::new();
    for version in versions {
        let row = sqlx::query(
            "insert into deployment_seen_releases (channel, version) values ($1, $2) \
             on conflict (channel, version) do nothing",
        )
        .bind(channel.as_str())
        .bind(version)
        .execute(pool)
        .await?;
        if row.rows_affected() == 1 {
            won.push(version.clone());
        }
    }
    Ok(won)
}

/// Release a claim taken by [`claim_announced`], so the next check re-announces those versions.
///
/// The recovery path for a failed `update.available` emit, and it is a `delete` rather than a
/// flag because a "seen but not announced" row has no other meaning: the table exists only to
/// answer "has this been announced yet?", and after a failed emit the honest answer is no.
///
/// The rows are deleted by the same (channel, version) pairs that were claimed, never by
/// "everything this check touched" — a concurrent check that claimed other versions must keep
/// its claim, or a slow second worker turns a recovered announcement into a double one.
pub async fn release_announced(
    pool: &PgPool,
    channel: Channel,
    versions: &[String],
) -> Result<(), StoreError> {
    for version in versions {
        sqlx::query("delete from deployment_seen_releases where channel = $1 and version = $2")
            .bind(channel.as_str())
            .bind(version)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// How many (channel, version) pairs the instance has already announced.
pub async fn seen_count(pool: &PgPool) -> Result<i64, StoreError> {
    let row: (i64,) = sqlx::query_as("select count(*) from deployment_seen_releases")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// One environment's last probe, as the card's tooltip quotes it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HealthRow {
    /// The environment.
    pub environment: String,
    /// The version running there.
    pub version: String,
    /// `healthy`, `degraded` or `unreachable`.
    pub status: String,
    /// When the probe ran.
    pub checked_at: OffsetDateTime,
    /// The per-probe results, as JSON.
    pub details: serde_json::Value,
}

/// Load every environment's health row.
pub async fn list_health(pool: &PgPool) -> Result<Vec<HealthRow>, StoreError> {
    let rows = sqlx::query_as::<_, HealthRow>(
        "select environment, version, status, checked_at, details from environment_health \
         order by case status when 'unreachable' then 0 when 'degraded' then 1 else 2 end, \
         environment",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One deployment row, as the history screen reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeploymentRow {
    /// Primary key.
    pub id: Uuid,
    /// The environment.
    pub environment: String,
    /// `deploy`, `rollback` or `restart`.
    pub kind: String,
    /// Where it came from.
    pub from_version: Option<String>,
    /// Where it went.
    pub to_version: Option<String>,
    /// The status.
    pub status: String,
    /// The strategy.
    pub strategy: String,
    /// Who started it.
    pub started_by: Option<Uuid>,
    /// The reason, for a rollback.
    pub reason: Option<String>,
    /// The backup taken first, when there was one.
    pub backup_id: Option<Uuid>,
    /// The error, verbatim.
    pub error: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
    /// How long it took.
    pub duration_ms: Option<i32>,
}

/// Query of `GET /api/v1/deployment/history`.
///
/// **No `Default` derive**, on purpose. The derived one would fill `limit` with `0`, and a
/// zero limit is not "no limit" to SQL — it is a query that returns no rows. The only way to
/// build this without naming every field is [`HistoryFilter::with_defaults`], which puts the
/// page size in one place instead of two (`DEFAULT_LIMIT` and whatever the caller passes).
#[derive(Debug, Clone)]
pub struct HistoryFilter {
    /// Environment filter.
    pub environment: Option<String>,
    /// `deploy`, `rollback` or `restart`.
    pub kind: Option<String>,
    /// A job status.
    pub status: Option<String>,
    /// Only rows started at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// Page size.
    pub limit: i64,
    /// Row offset.
    pub offset: i64,
}

impl HistoryFilter {
    /// The default page size, in one place.
    pub const DEFAULT_LIMIT: i64 = 50;

    /// A filter with the defaults filled in.
    #[must_use]
    pub fn with_defaults() -> Self {
        HistoryFilter {
            environment: None,
            kind: None,
            status: None,
            since: None,
            limit: Self::DEFAULT_LIMIT,
            offset: 0,
        }
    }
}

/// Read the history page and the total that matches the filter.
///
/// One statement for the page and one for the count, and the count is `count(*) over ()` on the
/// same query rather than a second `count(*)` with the predicates written twice: a count whose
/// filter is a *copy* of the page's filter is a count that is right until one of them is edited.
pub async fn list_history(
    pool: &PgPool,
    filter: &HistoryFilter,
) -> Result<(Vec<DeploymentRow>, i64), StoreError> {
    let rows = sqlx::query_as::<_, HistoryRow>(
        "select id, environment, kind, from_version, to_version, status, strategy, \
         started_by, reason, backup_id, error, started_at, finished_at, duration_ms, \
         count(*) over () as total \
         from deployments \
         where ($1::text is null or environment = $1) \
           and ($2::text is null or kind = $2) \
           and ($3::text is null or status = $3) \
           and ($4::timestamptz is null or started_at >= $4) \
         order by started_at desc, id desc \
         limit $5 offset $6",
    )
    .bind(filter.environment.as_deref())
    .bind(filter.kind.as_deref())
    .bind(filter.status.as_deref())
    .bind(filter.since)
    .bind(filter.limit.clamp(1, 200))
    .bind(filter.offset.max(0))
    .fetch_all(pool)
    .await?;

    let total = rows.first().map_or(0, |row| row.total);
    let deployments = rows
        .into_iter()
        .map(|row| DeploymentRow {
            id: row.id,
            environment: row.environment,
            kind: row.kind,
            from_version: row.from_version,
            to_version: row.to_version,
            status: row.status,
            strategy: row.strategy,
            started_by: row.started_by,
            reason: row.reason,
            backup_id: row.backup_id,
            error: row.error,
            started_at: row.started_at,
            finished_at: row.finished_at,
            duration_ms: row.duration_ms,
        })
        .collect();
    Ok((deployments, total))
}

/// A history row plus the total that matched the filter.
#[derive(Debug, Clone, sqlx::FromRow)]
struct HistoryRow {
    id: Uuid,
    environment: String,
    kind: String,
    from_version: Option<String>,
    to_version: Option<String>,
    status: String,
    strategy: String,
    started_by: Option<Uuid>,
    reason: Option<String>,
    backup_id: Option<Uuid>,
    error: Option<String>,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    duration_ms: Option<i32>,
    total: i64,
}

/// One step of a deployment, as the history expansion reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StepRow {
    /// Position in the run.
    pub position: i32,
    /// The step's name.
    pub name: String,
    /// Its status.
    pub status: String,
    /// Its log output, so far.
    pub output: String,
    /// When it started.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
}

/// Load one deployment's steps, in run order.
pub async fn list_steps(pool: &PgPool, deployment_id: Uuid) -> Result<Vec<StepRow>, StoreError> {
    let rows = sqlx::query_as::<_, StepRow>(
        "select position, name, status, output, started_at, finished_at from deployment_steps \
         where deployment_id = $1 order by position",
    )
    .bind(deployment_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Load one deployment, or `NotFound`.
pub async fn load_deployment(pool: &PgPool, id: Uuid) -> Result<DeploymentRow, StoreError> {
    let row = sqlx::query_as::<_, DeploymentRow>(
        "select id, environment, kind, from_version, to_version, status, strategy, \
         started_by, reason, backup_id, error, started_at, finished_at, duration_ms \
         from deployments where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.ok_or(StoreError::NotFound)
}
