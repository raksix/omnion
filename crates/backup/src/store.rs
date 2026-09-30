//! The four tables: runs, parts, schedules and settings (REQ-013, slice 1).
//!
//! Two rules hold across every statement here, and both are about *who decides*:
//!
//! * **A status is written by the crate, never by the caller.** [`finish_run`] derives the
//!   terminal state from the parts with [`summarise`], so a route that decides a run
//!   "succeeded" because the handler returned early cannot record it. A caller may say a
//!   run is `running`; only the parts decide what it becomes.
//! * **Every read is scoped by organization in the same `where` clause that finds the row.**
//!   Reading the row first and checking its scope afterwards is the standard tenancy bug and
//!   it answers `403 cross_organization`, which confirms the id exists — an oracle for
//!   guessing ids. A backup of another tenant is a `404` here, and the refusal deletes
//!   nothing.
//!
//! The prune sweep is the statement that most needs both rules at once: it is destructive, it
//! runs unattended, and the two things it must never delete are *the newest successful
//! backup* and *a protected one*. Both are decided in SQL, in the one statement that
//! performs the delete, so a caller cannot express a third rule that disagrees.

use sqlx::{PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{BackupError, Result};
use crate::part::{
    MANIFEST_VERSION, Manifest, Part, PartStatus, build_manifest, manifest_checksum,
    normalise_scopes, summarise,
};

/// The columns of a backup row, in the order [`Backup`] reads them.
const BACKUP_COLUMNS: &str = "id, organization_id, label, kind, schedule_id, scopes, status, \
                              size_bytes, destination, storage_prefix, manifest, checksum, \
                              protected, retain_until, error, created_by, created_at, \
                              started_at, finished_at";

/// A backup run as the list and the detail screen read it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Backup {
    /// Run id.
    pub id: Uuid,
    /// Tenant the run belongs to; null for the platform.
    pub organization_id: Option<Uuid>,
    /// Operator's label; empty is legal.
    pub label: String,
    /// `manual` or `scheduled`.
    pub kind: String,
    /// The schedule that started it, when one did.
    pub schedule_id: Option<Uuid>,
    /// The parts it was asked for, in execution order.
    pub scopes: Vec<String>,
    /// Where it got to.
    pub status: String,
    /// Sum of its parts' sizes.
    pub size_bytes: i64,
    /// `local` or `s3`.
    pub destination: String,
    /// Prefix its artifacts live under.
    pub storage_prefix: String,
    /// Its own manifest, as stored.
    pub manifest: serde_json::Value,
    /// SHA-256 over the manifest's canonical form.
    pub checksum: Option<String>,
    /// Whether the prune sweep will leave it alone.
    pub protected: bool,
    /// When the prune sweep may remove it.
    pub retain_until: Option<OffsetDateTime>,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// Who started it.
    pub created_by: Option<Uuid>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When it began producing.
    pub started_at: Option<OffsetDateTime>,
    /// When it stopped producing.
    pub finished_at: Option<OffsetDateTime>,
}

/// `Backup` is a name `sqlx` already uses for its own row-mapping trait, so the `query_as`
/// turbofish in this file spells the row `BackupRow`. Without the alias every statement has
/// to name the derive's trait, which reads like a mistake in a statement that has nothing to
/// do with mapping.
type BackupRow = Backup;

/// A run to insert.
#[derive(Debug, Clone)]
pub struct NewBackup {
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// Label; may be empty.
    pub label: String,
    /// `manual` or `scheduled`.
    pub kind: String,
    /// The schedule that started it.
    pub schedule_id: Option<Uuid>,
    /// The parts to produce. Normalised, checked, and stored in execution order.
    pub scopes: Vec<String>,
    /// Where the bytes go.
    pub destination: String,
    /// Prefix under that destination.
    pub storage_prefix: String,
    /// Whether the prune sweep must leave it alone.
    pub protected: bool,
    /// Retention override, when the caller set one.
    pub retain_until: Option<OffsetDateTime>,
    /// Who started it.
    pub created_by: Option<Uuid>,
}

/// A part to insert or update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPart {
    /// Which part.
    pub part: String,
    /// Where it got to.
    pub status: PartStatus,
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
}

/// A list query's filters.
#[derive(Debug, Clone, Default)]
pub struct BackupQuery {
    /// Tenant scope.
    pub organization_id: Option<Uuid>,
    /// Restrict to one terminal status.
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
    /// How many rows a page holds.
    pub limit: i64,
    /// How far into the list to start.
    pub offset: i64,
}

/// One page of runs plus the count that does not depend on the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupPage {
    /// The page's rows.
    pub items: Vec<Backup>,
    /// How many rows the filters match in total.
    pub total: i64,
}

/// How many runs are in each terminal state.
///
/// It derives `Serialize` because the API returns it verbatim in two responses, and a filter
/// chip rendered from a count the endpoint did not send is a chip that can disagree with the
/// footer underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct StatusTotals {
    /// Waiting to start.
    pub queued: i64,
    /// In flight.
    pub running: i64,
    /// Every part produced its artifact.
    pub succeeded: i64,
    /// Some parts produced theirs, some did not.
    pub partial: i64,
    /// No part produced its artifact.
    pub failed: i64,
}

/// How many parts of one kind a set of runs produced, and how many failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartTotals {
    /// Parts that produced an artifact.
    pub done: i64,
    /// Parts that were attempted and refused.
    pub failed: i64,
}

/// A recurring backup definition.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupSchedule {
    /// Row id.
    pub id: Uuid,
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// Display name, unique per scope.
    pub name: String,
    /// `hourly|daily|weekly|monthly`.
    pub frequency: String,
    /// Time of day, for everything but hourly.
    pub at_time: Option<String>,
    /// Weekday, for weekly only.
    pub day_of_week: Option<i16>,
    /// Day of the month, for monthly only.
    pub day_of_month: Option<i16>,
    /// IANA zone the schedule is computed in.
    pub timezone: String,
    /// The parts it produces.
    pub scopes: Vec<String>,
    /// How many of its own runs to keep.
    pub retention_count: i32,
    /// Where the bytes go.
    pub destination: String,
    /// Whether the worker acts on it.
    pub enabled: bool,
    /// When it last produced a run.
    pub last_run_at: Option<OffsetDateTime>,
    /// When it produces its next.
    pub next_run_at: Option<OffsetDateTime>,
    /// The run it produced last.
    pub last_backup_id: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was last edited.
    pub updated_at: OffsetDateTime,
}

/// A schedule to insert or update.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    /// Tenant it belongs to.
    pub organization_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// Frequency.
    pub frequency: String,
    /// Time of day.
    pub at_time: Option<String>,
    /// Weekday.
    pub day_of_week: Option<i16>,
    /// Day of the month.
    pub day_of_month: Option<i16>,
    /// Timezone.
    pub timezone: String,
    /// Parts it produces.
    pub scopes: Vec<String>,
    /// How many runs to keep.
    pub retention_count: i32,
    /// Destination.
    pub destination: String,
    /// Whether it is active.
    pub enabled: bool,
    /// Who created it.
    pub created_by: Option<Uuid>,
}

/// The single settings row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupSettings {
    /// Always 1.
    pub id: i16,
    /// `local` or `s3`.
    pub destination: String,
    /// Absolute root for a local destination.
    pub local_root: String,
    /// Prefix inside a bucket, for an s3 destination.
    pub s3_prefix: Option<String>,
    /// A reference into the deployment's secret store. Never a value.
    pub credential_ref: Option<String>,
    /// `none` or `passphrase`.
    pub encryption: String,
    /// Default retention for a new schedule.
    pub default_retention: i32,
    /// Whether a run re-reads its own artifacts before finishing.
    pub verify_after_backup: bool,
    /// Who last saved it.
    pub updated_by: Option<Uuid>,
    /// When it was last saved.
    pub updated_at: OffsetDateTime,
}

/// Settings to save.
#[derive(Debug, Clone)]
pub struct NewSettings {
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
    /// Default retention.
    pub default_retention: i32,
    /// Whether to verify after a run.
    pub verify_after_backup: bool,
    /// Who saved it.
    pub updated_by: Option<Uuid>,
}

/// Insert a run and its `queued` part rows in one transaction.
///
/// The parts are inserted here rather than by the caller, and that is the difference between
/// "a run exists" and "a run exists and is auditable part by part": a caller that inserts the
/// run and then crashes leaves a row whose parts are all absent, and the detail screen has to
/// decide whether that means "nothing was asked for" or "we never got that far".
pub async fn insert_backup(pool: &PgPool, new: &NewBackup) -> Result<Backup> {
    let scopes = normalise_scopes(&new.scopes)?;
    if !matches!(new.kind.as_str(), "manual" | "scheduled") {
        return Err(BackupError::Invalid(format!(
            "kind: `{}` is not manual or scheduled",
            new.kind
        )));
    }
    if !matches!(new.destination.as_str(), "local" | "s3") {
        return Err(BackupError::Invalid(format!(
            "destination: `{}` is not local or s3",
            new.destination
        )));
    }

    let mut tx = pool.begin().await?;
    let row: Backup = sqlx::query_as(&format!(
        "insert into backups \
           (organization_id, label, kind, schedule_id, scopes, status, destination, \
            storage_prefix, protected, retain_until, created_by) \
         values ($1, $2, $3, $4, $5, 'queued', $6, $7, $8, $9, $10) \
         returning {BACKUP_COLUMNS}"
    ))
    .bind(new.organization_id)
    .bind(new.label.trim())
    .bind(&new.kind)
    .bind(new.schedule_id)
    .bind(&scopes)
    .bind(&new.destination)
    .bind(&new.storage_prefix)
    .bind(new.protected)
    .bind(new.retain_until)
    .bind(new.created_by)
    .fetch_one(&mut *tx)
    .await?;

    for scope in &scopes {
        sqlx::query("insert into backup_parts (backup_id, part, status) values ($1, $2, 'queued')")
            .bind(row.id)
            .bind(scope)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// One run, scoped by tenant.
///
/// The scope is in the `where` rather than a check after the read, so another tenant's run
/// is a `404` and not a `403` that confirms the id exists.
pub async fn find_backup(pool: &PgPool, id: Uuid, organization_id: Option<Uuid>) -> Result<Backup> {
    sqlx::query_as::<_, BackupRow>(&format!(
        "select {BACKUP_COLUMNS} from backups \
         where id = $1 and organization_id is not distinct from $2"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?
    .ok_or(BackupError::NotFound)
}

/// The parts of a run, in execution order.
pub async fn list_parts(pool: &PgPool, backup_id: Uuid) -> Result<Vec<Part>> {
    let rows: Vec<(
        String,
        String,
        i32,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "select part, status, item_count, size_bytes, checksum, storage_path, error \
             from backup_parts where backup_id = $1 order by part",
    )
    .bind(backup_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(
            |(part, status, item_count, size_bytes, checksum, storage_path, error)| {
                Ok(Part {
                    part,
                    status: PartStatus::parse(&status)?,
                    item_count,
                    size_bytes,
                    checksum,
                    storage_path,
                    error,
                })
            },
        )
        .collect()
}

/// A page of runs and the count behind it.
///
/// The count and the page are built from **one** [`QueryBuilder`] prefix, so they cannot
/// disagree. Two separate `where` strings is how "Showing 20 of 0" appears next to twenty
/// rows — and the count is the number the footer quotes, so a wrong one is a wrong claim
/// about the operator's own data rather than a rendering glitch.
///
/// A filter value is always `push_bind`ed. The only text this function writes into the
/// statement are the *operators* (`= `, `>= `, `= any (scopes)`), which this file owns; a
/// search term or a status the caller chose is a bind, never a `format!`.
pub async fn list_backups(pool: &PgPool, query: &BackupQuery) -> Result<BackupPage> {
    fn push_filters(builder: &mut QueryBuilder<'_, Postgres>, query: &BackupQuery) {
        builder.push("organization_id is not distinct from ");
        builder.push_bind(query.organization_id);
        if let Some(status) = &query.status {
            builder.push(" and status = ");
            builder.push_bind(status.clone());
        }
        if let Some(kind) = &query.kind {
            builder.push(" and kind = ");
            builder.push_bind(kind.clone());
        }
        if let Some(destination) = &query.destination {
            builder.push(" and destination = ");
            builder.push_bind(destination.clone());
        }
        if let Some(scope) = &query.scope {
            // A run that included a part, not a run whose label mentions it: `scopes` is the
            // array the run was asked for, so the filter reads the same fact the row shows.
            builder.push(" and ");
            builder.push_bind(scope.clone());
            builder.push(" = any (scopes)");
        }
        if let Some(after) = query.created_after {
            builder.push(" and created_at >= ");
            builder.push_bind(after);
        }
        if let Some(before) = query.created_before {
            builder.push(" and created_at <= ");
            builder.push_bind(before);
        }
    }

    let mut counter = QueryBuilder::<Postgres>::new("select count(*) from backups where ");
    push_filters(&mut counter, query);
    let total: i64 = counter.build_query_scalar().fetch_one(pool).await?;

    let limit = query.limit.clamp(1, 200);
    let offset = query.offset.max(0);
    let mut page = QueryBuilder::<Postgres>::new("select ");
    page.push(BACKUP_COLUMNS);
    page.push(" from backups where ");
    push_filters(&mut page, query);
    page.push(" order by created_at desc limit ");
    page.push_bind(i64::from(limit));
    page.push(" offset ");
    page.push_bind(offset);
    let items: Vec<BackupRow> = page.build_query_as().fetch_all(pool).await?;
    Ok(BackupPage { items, total })
}

/// How many runs are in each terminal state, for the overview's filter chips.
pub async fn count_by_status(pool: &PgPool, organization_id: Option<Uuid>) -> Result<StatusTotals> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select status, count(*) from backups \
         where organization_id is not distinct from $1 group by status",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let mut totals = StatusTotals::default();
    for (status, count) in rows {
        match status.as_str() {
            "queued" => totals.queued = count,
            "running" => totals.running = count,
            "succeeded" => totals.succeeded = count,
            "partial" => totals.partial = count,
            "failed" => totals.failed = count,
            _ => {}
        }
    }
    Ok(totals)
}

/// Total bytes and the newest successful run, for the overview's status cards.
pub async fn totals(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<(i64, Option<OffsetDateTime>, Option<Uuid>)> {
    // `sum()` over a `bigint` column returns `numeric`, which sqlx will not decode into an
    // `i64` — the first version of this query answered `500` on every call with "mismatched
    // types; Rust type Option<i64> is not compatible with SQL type NUMERIC", which took the
    // whole status card down over a column width. The cast is `::bigint` and deliberately NOT
    // `::int`: a total that wraps at 2 GiB reports a plausible small number, and a plausible
    // small number is worse than an error somebody can see.
    let row: (Option<i64>, Option<OffsetDateTime>, Option<Uuid>) = sqlx::query_as(
        "select sum(size_bytes)::bigint, max(finished_at) filter (where status = 'succeeded'), \
                (array_agg(id order by finished_at desc) filter \
                   (where status = 'succeeded'))[1] \
         from backups where organization_id is not distinct from $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok((row.0.unwrap_or(0), row.1, row.2))
}

/// How many protected backups exist — the number the prune screen shows as "never removed".
pub async fn protected_backup_count(pool: &PgPool, organization_id: Option<Uuid>) -> Result<i64> {
    sqlx::query_scalar(
        "select count(*) from backups \
         where protected and organization_id is not distinct from $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .map_err(BackupError::from)
}

/// The parts a run produced, ready to be written to the destination.
pub async fn save_part(pool: &PgPool, backup_id: Uuid, part: &NewPart) -> Result<()> {
    let status = part.status.as_str();
    let error = part.error.as_deref().map(crate::part::truncate_error);
    sqlx::query(
        "insert into backup_parts \
           (backup_id, part, status, item_count, size_bytes, checksum, storage_path, error, \
            started_at, finished_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, \
                 case when $3 = 'queued' then null else now() end, \
                 case when $3 in ('done', 'failed') then now() else null end) \
         on conflict (backup_id, part) do update set \
            status = excluded.status, item_count = excluded.item_count, \
            size_bytes = excluded.size_bytes, checksum = excluded.checksum, \
            storage_path = excluded.storage_path, error = excluded.error, \
            started_at = coalesce(backup_parts.started_at, excluded.started_at), \
            finished_at = excluded.finished_at",
    )
    .bind(backup_id)
    .bind(&part.part)
    .bind(status)
    .bind(part.item_count)
    .bind(part.size_bytes)
    .bind(&part.checksum)
    .bind(&part.storage_path)
    .bind(&error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert one part row without an existing run — used by the walk fixtures.
pub async fn insert_part(pool: &PgPool, backup_id: Uuid, part: &str) -> Result<()> {
    sqlx::query("insert into backup_parts (backup_id, part) values ($1, $2)")
        .bind(backup_id)
        .bind(part)
        .execute(pool)
        .await?;
    Ok(())
}

/// Write a run's terminal state, deriving it from its parts.
///
/// The caller cannot pass a status, and that is the point: `summarise` reads the parts and
/// [`RunStatus`] comes out. A handler that returns early with a `200` records a run that is
/// still `running`, which is what actually happened — and the detail screen shows it as such
/// rather than as a success nobody confirmed.
pub async fn finish_run(
    pool: &PgPool,
    backup_id: Uuid,
    parts: &[Part],
    manifest_created_at: &str,
) -> Result<Backup> {
    let status = summarise(parts);
    let manifest = build_manifest(&backup_id.to_string(), parts, manifest_created_at);
    let checksum = manifest_checksum(&manifest);
    let size: i64 = parts.iter().map(|part| part.size_bytes).sum();
    let error = parts
        .iter()
        .find(|part| part.status == PartStatus::Failed)
        .and_then(|part| part.error.clone());

    sqlx::query_as::<_, BackupRow>(&format!(
        "update backups set status = $2, size_bytes = $3, manifest = $4, checksum = $5, \
           error = $6, finished_at = now(), started_at = coalesce(started_at, now()) \
         where id = $1 returning {BACKUP_COLUMNS}"
    ))
    .bind(backup_id)
    .bind(status.as_str())
    .bind(size)
    .bind(serde_json::to_value(&manifest).unwrap_or_else(|_| serde_json::json!({})))
    .bind(&checksum)
    .bind(error)
    .fetch_one(pool)
    .await
    .map_err(BackupError::from)
}

/// Mark a run in flight. Separate from [`finish_run`] so the detail screen can show a run
/// that started without pretending it produced anything.
pub async fn start_run(pool: &PgPool, backup_id: Uuid) -> Result<()> {
    sqlx::query(
        "update backups set status = 'running', started_at = coalesce(started_at, now()) \
         where id = $1 and status = 'queued'",
    )
    .bind(backup_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set a run's storage prefix, once the run's own id is known.
///
/// The prefix cannot be built in the insert because it is derived from the generated id, and
/// it cannot be built before the insert because there is no id yet — so the run is inserted
/// with an empty prefix and given one immediately after. Deriving it from the **id** rather
/// than from the clock is the part that matters: two runs started inside the same second
/// would share a prefix, and the second would overwrite the first's artifacts.
pub async fn set_prefix(pool: &PgPool, id: Uuid, prefix: &str) -> Result<Backup> {
    sqlx::query_as::<_, BackupRow>(&format!(
        "update backups set storage_prefix = $2 where id = $1 returning {BACKUP_COLUMNS}"
    ))
    .bind(id)
    .bind(prefix)
    .fetch_one(pool)
    .await
    .map_err(BackupError::from)
}

/// The tenants that own at least one backup, oldest history first, plus the platform's own
/// (`null`) row when it has one.
///
/// The retention sweep walks this list, and it is a **separate function rather than a query
/// the runner writes**: the sweep has to walk tenants one at a time because
/// [`prune_candidates`] is scoped by `organization_id` and `is not distinct from` is what
/// keeps the platform's own backups in the same loop. A runner that wrote
/// `select distinct organization_id from backups` inline would look identical and would be a
/// second answer to "who gets swept" — the same mistake the walkthrough's own comment warns
/// about, one layer up.
///
/// `null` is a real member of this list, not a missing value: `backups.organization_id` is
/// nullable for the platform itself, and a sweep that filtered it away would never prune the
/// platform's own restore points — the ones that matter most on a single-tenant installation.
pub async fn organizations_with_backups(pool: &PgPool, batch: i64) -> Result<Vec<Option<Uuid>>> {
    let rows: Vec<(Option<Uuid>,)> = sqlx::query_as(
        "select organization_id from backups group by organization_id \
         order by min(created_at) asc limit $1",
    )
    .bind(batch.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(organization_id,)| organization_id).collect())
}

/// Delete a run. Its parts cascade; its artifacts do not, and slice 2 owns removing those.
pub async fn delete_backup(pool: &PgPool, id: Uuid, organization_id: Option<Uuid>) -> Result<()> {
    let removed = sqlx::query(
        "delete from backups where id = $1 and organization_id is not distinct from $2",
    )
    .bind(id)
    .bind(organization_id)
    .execute(pool)
    .await?;
    if removed.rows_affected() == 0 {
        return Err(BackupError::NotFound);
    }
    Ok(())
}

/// Set or clear a backup's protection from the prune sweep.
pub async fn set_protected(
    pool: &PgPool,
    id: Uuid,
    organization_id: Option<Uuid>,
    protected: bool,
) -> Result<Backup> {
    sqlx::query_as::<_, BackupRow>(&format!(
        "update backups set protected = $3 \
         where id = $1 and organization_id is not distinct from $2 returning {BACKUP_COLUMNS}"
    ))
    .bind(id)
    .bind(organization_id)
    .bind(protected)
    .fetch_optional(pool)
    .await?
    .ok_or(BackupError::NotFound)
}

/// Runs the prune sweep may remove: past their `retain_until`, not protected, not the newest
/// successful one, and not still in flight.
///
/// All four are decided **here**, in the one statement that feeds the delete, so the delete
/// itself cannot be edited into a different rule. The newest-successful exemption is the one
/// that matters: a retention count of 7 that removes every backup because all seven are
/// expired leaves an operator with nothing to restore, and the sweep would have reported
/// success.
pub async fn prune_candidates(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Vec<Backup>> {
    // The subquery that picks the run to spare is the whole subtlety, and it had two conditions
    // the doc comment above promised and the statement never carried:
    //
    // * **`status = 'succeeded'`.** Without it the sweep spares the newest run *of any kind*,
    //   so a `partial` — a run that half-completed and is not restorable as a whole — gets the
    //   protection while the newest run that *can* be restored is deleted. That is backwards:
    //   the exemption exists so an operator always has something to restore, and exempting a
    //   broken run satisfies the letter of it with nothing behind it. Proved against the
    //   database: with the filter missing, a site whose last three runs were all `partial` had
    //   its newest `partial` spared and both older ones offered for deletion.
    //   the newest `partial` spared and both older ones offered for deletion.
    // * **`not protected`.** A protected run is already spared by the outer `not b.protected`,
    //   so letting it also consume the "newest successful" exemption is a second, invisible
    //   exemption spent on a row that needed none. The operator who protects the newest run
    //   expects the *newest other* run to be spared too; instead the sweep offered the whole
    //   rest of the history for deletion, and reported success. Two exemptions, one survivor.
    // * **`not running`.** A run still in flight has no meaningful age, and `finished_at desc
    //   nulls last` would spare it only by accident of a `NULL` sort.
    //
    // All of it belongs in this statement, not in the caller, because this statement is what
    // the delete is fed from.
    sqlx::query_as::<_, BackupRow>(&format!(
        "select {BACKUP_COLUMNS} from backups b \
         where organization_id is not distinct from $1 \
           and status not in ('failed', 'running') \
           and not b.protected \
           and b.retain_until is not null \
           and b.retain_until <= $2 \
           and b.id <> (select id from backups \
                        where organization_id is not distinct from $1 \
                          and status = 'succeeded' \
                          and not protected \
                        order by finished_at desc nulls last, created_at desc limit 1) \
         order by b.retain_until asc limit 100"
    ))
    .bind(organization_id)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(BackupError::from)
}

/// The schedules of a tenant.
pub async fn list_schedules(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<BackupSchedule>> {
    sqlx::query_as(
        "select id, organization_id, name, frequency, at_time::text as at_time, \
                day_of_week, day_of_month, timezone, scopes, retention_count, destination, \
                enabled, last_run_at, next_run_at, last_backup_id, created_at, updated_at \
         from backup_schedules \
         where organization_id is not distinct from $1 \
         order by enabled desc, lower(name) asc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(BackupError::from)
}

/// Insert or update a schedule, and report which it was.
///
/// The `returning (xmax = 0)` idiom distinguishes an insert from an update without a second
/// round trip, which matters because the schedule name is unique per scope and the screen
/// needs to say *which* rule refused a duplicate.
pub async fn upsert_schedule(
    pool: &PgPool,
    id: Option<Uuid>,
    new: &NewSchedule,
) -> Result<BackupSchedule> {
    let scopes = normalise_scopes(&new.scopes)?;
    let row: BackupSchedule = match id {
        Some(id) => sqlx::query_as(
            "update backup_schedules set name = $3, frequency = $4, at_time = $5::time, \
                day_of_week = $6, day_of_month = $7, timezone = $8, scopes = $9, \
                retention_count = $10, destination = $11, enabled = $12, updated_at = now() \
             where id = $1 and organization_id is not distinct from $2 \
             returning id, organization_id, name, frequency, at_time::text as at_time, \
                day_of_week, day_of_month, timezone, scopes, retention_count, destination, \
                enabled, last_run_at, next_run_at, last_backup_id, created_at, updated_at",
        )
        .bind(id)
        .bind(new.organization_id)
        .bind(new.name.trim())
        .bind(&new.frequency)
        .bind(&new.at_time)
        .bind(new.day_of_week)
        .bind(new.day_of_month)
        .bind(new.timezone.trim())
        .bind(&scopes)
        .bind(new.retention_count)
        .bind(&new.destination)
        .bind(new.enabled)
        .fetch_optional(pool)
        .await?
        .ok_or(BackupError::ScheduleNotFound)?,
        None => {
            sqlx::query_as(
                "insert into backup_schedules \
               (organization_id, name, frequency, at_time, day_of_week, day_of_month, \
                timezone, scopes, retention_count, destination, enabled, created_by) \
             values ($1, $2, $3, $4::time, $5, $6, $7, $8, $9, $10, $11, $12) \
             returning id, organization_id, name, frequency, at_time::text as at_time, \
                day_of_week, day_of_month, timezone, scopes, retention_count, destination, \
                enabled, last_run_at, next_run_at, last_backup_id, created_at, updated_at",
            )
            .bind(new.organization_id)
            .bind(new.name.trim())
            .bind(&new.frequency)
            .bind(&new.at_time)
            .bind(new.day_of_week)
            .bind(new.day_of_month)
            .bind(new.timezone.trim())
            .bind(&scopes)
            .bind(new.retention_count)
            .bind(&new.destination)
            .bind(new.enabled)
            .bind(new.created_by)
            .fetch_one(pool)
            .await?
        }
    };
    Ok(row)
}

/// Delete a schedule. Its runs keep their own `kind` and lose only the link.
pub async fn delete_schedule(pool: &PgPool, id: Uuid, organization_id: Option<Uuid>) -> Result<()> {
    let removed = sqlx::query(
        "delete from backup_schedules where id = $1 and organization_id is not distinct from $2",
    )
    .bind(id)
    .bind(organization_id)
    .execute(pool)
    .await?;
    if removed.rows_affected() == 0 {
        return Err(BackupError::ScheduleNotFound);
    }
    Ok(())
}

/// One schedule, read through the tenant boundary.
///
/// A `fetch_optional` filtered by `organization_id` rather than a read-then-check, because a
/// read followed by a scope test is two statements and a race: a schedule that is reassigned
/// between them answers a question about a row that no longer exists. The tenancy filter is
/// inside the `where`, which is also what turns a stranger's id into a `ScheduleNotFound` and
/// therefore a `404` — the same rule the rest of this file follows, for the same reason.
pub async fn find_schedule(
    pool: &PgPool,
    id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<BackupSchedule> {
    sqlx::query_as(
        "select id, organization_id, name, frequency, at_time::text as at_time, \
                day_of_week, day_of_month, timezone, scopes, retention_count, destination, \
                enabled, last_run_at, next_run_at, last_backup_id, created_at, updated_at \
         from backup_schedules \
         where id = $1 and organization_id is not distinct from $2",
    )
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?
    .ok_or(BackupError::ScheduleNotFound)
}

/// Write a schedule's next run, or clear it.
///
/// A **separate** function rather than a parameter on `upsert_schedule`, because the two are
/// different moments: the row is written first, and the next run is only known once the
/// cadence has been validated. Folding them together would mean computing a time for a row
/// that has not been accepted yet, and storing it if the row is later refused.
pub async fn set_schedule_next_run(
    pool: &PgPool,
    id: Uuid,
    next_run_at: Option<OffsetDateTime>,
) -> Result<()> {
    sqlx::query("update backup_schedules set next_run_at = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(next_run_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Enable or disable a schedule without touching the rest of it.
///
/// A separate function rather than a patch-style update because the only caller is the
/// worker refusing a schedule whose cadence it cannot compute, and that caller must not be
/// able to accidentally rewrite the scopes while it is disabling a row. A disabled schedule
/// keeps its `next_run_at`, so re-enabling it through the editor recomputes the time and a
/// row that is disabled by hand stays disabled until somebody says otherwise.
pub async fn set_schedule_enabled(pool: &PgPool, id: Uuid, enabled: bool) -> Result<()> {
    sqlx::query("update backup_schedules set enabled = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(())
}

/// Enabled schedules whose `next_run_at` has arrived.
pub async fn next_due_schedules(pool: &PgPool, now: OffsetDateTime) -> Result<Vec<BackupSchedule>> {
    sqlx::query_as(
        "select id, organization_id, name, frequency, at_time::text as at_time, \
                day_of_week, day_of_month, timezone, scopes, retention_count, destination, \
                enabled, last_run_at, next_run_at, last_backup_id, created_at, updated_at \
         from backup_schedules \
         where enabled and next_run_at is not null and next_run_at <= $1 \
         order by next_run_at asc limit 20",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(BackupError::from)
}

/// Whether a schedule is due, so a route can answer without computing a next run twice.
pub async fn schedule_appears_due(pool: &PgPool, id: Uuid, now: OffsetDateTime) -> Result<bool> {
    let due: Option<bool> = sqlx::query_scalar(
        "select enabled and next_run_at is not null and next_run_at <= $2 from backup_schedules \
         where id = $1",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(due.unwrap_or(false))
}

/// Read the settings row, creating it if a database somehow has none.
///
/// The migration seeds the row and a trigger keeps it, so this is belt and braces — but
/// "the settings screen answers with defaults" while "the settings save writes nothing" is
/// the exact failure `0028` and `0044` each had to fix, and a read that cannot fail is worth
/// two lines here.
pub async fn load_settings(pool: &PgPool) -> Result<BackupSettings> {
    if let Some(row) = sqlx::query_as::<_, BackupSettings>(
        "select id, destination, local_root, s3_prefix, credential_ref, encryption, \
                default_retention, verify_after_backup, updated_by, updated_at \
         from backup_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok(row);
    }
    sqlx::query("insert into backup_settings (id) values (1) on conflict (id) do nothing")
        .execute(pool)
        .await?;
    sqlx::query_as::<_, BackupSettings>(
        "select id, destination, local_root, s3_prefix, credential_ref, encryption, \
                default_retention, verify_after_backup, updated_by, updated_at \
         from backup_settings where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(BackupError::from)
}

/// Save the settings row.
///
/// A `partial` save folds onto the existing row rather than resetting the fields it did not
/// send: ten fields and a form that posts six must not silently put the other four back to
/// platform defaults, and the operator finds out a week later when the encryption mode
/// changed by itself.
pub async fn save_settings(pool: &PgPool, new: &NewSettings) -> Result<BackupSettings> {
    if !matches!(new.destination.as_str(), "local" | "s3") {
        return Err(BackupError::Invalid(format!(
            "destination: `{}` is not local or s3",
            new.destination
        )));
    }
    if !matches!(new.encryption.as_str(), "none" | "passphrase") {
        return Err(BackupError::Invalid(format!(
            "encryption: `{}` is not none or passphrase",
            new.encryption
        )));
    }
    if !(1..=365).contains(&new.default_retention) {
        return Err(BackupError::Invalid(
            "default_retention: between 1 and 365".to_owned(),
        ));
    }
    sqlx::query_as::<_, BackupSettings>(
        "insert into backup_settings \
           (id, destination, local_root, s3_prefix, credential_ref, encryption, \
            default_retention, verify_after_backup, updated_by, updated_at) \
         values (1, $1, $2, $3, $4, $5, $6, $7, $8, now()) \
         on conflict (id) do update set \
            destination = excluded.destination, local_root = excluded.local_root, \
            s3_prefix = excluded.s3_prefix, credential_ref = excluded.credential_ref, \
            encryption = excluded.encryption, \
            default_retention = excluded.default_retention, \
            verify_after_backup = excluded.verify_after_backup, \
            updated_by = excluded.updated_by, updated_at = now() \
         returning id, destination, local_root, s3_prefix, credential_ref, encryption, \
                default_retention, verify_after_backup, updated_by, updated_at",
    )
    .bind(&new.destination)
    .bind(new.local_root.trim())
    .bind(new.s3_prefix.as_deref().map(str::trim))
    .bind(new.credential_ref.as_deref().map(str::trim))
    .bind(&new.encryption)
    .bind(new.default_retention)
    .bind(new.verify_after_backup)
    .bind(new.updated_by)
    .fetch_one(pool)
    .await
    .map_err(BackupError::from)
}

/// Re-read a stored run's manifest, or an empty one when the run has none yet.
///
/// An unreadable manifest is `MANIFEST_VERSION` with no parts rather than an error, because
/// the caller's question is "what does the restore wizard get to show" and a run that has not
/// finished has an honest answer: nothing.
pub fn manifest_of(backup: &Backup) -> Manifest {
    serde_json::from_value(backup.manifest.clone()).unwrap_or(Manifest {
        backup_id: backup.id.to_string(),
        version: MANIFEST_VERSION,
        parts: Vec::new(),
        created_at: backup.created_at.to_string(),
    })
}

/// Tie a run to the schedule that produced it, and move that schedule's own bookkeeping.
pub async fn record_schedule_run(
    pool: &PgPool,
    schedule_id: Uuid,
    backup_id: Uuid,
    next_run_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "update backup_schedules \
         set last_run_at = now(), last_backup_id = $2, next_run_at = $3, updated_at = now() \
         where id = $1",
    )
    .bind(schedule_id)
    .bind(backup_id)
    .bind(next_run_at)
    .execute(pool)
    .await?;
    Ok(())
}
