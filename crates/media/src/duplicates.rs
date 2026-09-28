//! Where a file is used, and which files are the same file twice (REQ-010, slice 3).
//!
//! Two questions live here, and they are the same question asked from opposite ends:
//!
//! * **"where is this file used?"** — the reference rows. Slice 2 shipped a "used in" tab that
//!   has nothing to answer it, and a duplicate merge has nothing to repoint without them.
//!   A reference names a *record and a field* (`page`, `<uuid>`, `hero_image_id`) and never a
//!   copy of the value, which is what makes the repoint two columns of an update rather than a
//!   search of every column of every table.
//! * **"what does this site store twice?"** — the duplicate groups. A group is a projection of
//!   `media` over its own checksum, never a stored table: a replace changes the checksum, a
//!   delete removes a row, a restore brings one back, and a stored group would need a trigger on
//!   all three to stay true.
//!
//! Six rules hold across this module, and each is a place the obvious shortcut is wrong:
//!
//! * **A group of one is not a duplicate.** The `having count(*) > 1` lives in the view, and it
//!   is the difference between a storage report and a second file browser with a warning painted
//!   on it.
//! * **A trashed copy is not a duplicate.** It is already on its way out and the retention
//!   window has already started, so counting it makes the report claim space the trash screen is
//!   about to return.
//! * **The report never picks the keeper.** [`merge_group`] refuses a `keep` that is not in the
//!   group, because a merge that chose for itself would break a live page and the operator would
//!   find out from a 404 rather than from the report.
//! * **A merge trashes copies; it never deletes.** The bytes stay, the retention countdown is
//!   already running, and `Restore` on the trashed copy reverses the whole operation. Every other
//!   destructive action in the library has that property and this one would be the odd exception
//!   if it did not.
//! * **"Reclaimable" excludes the keeper's own copy.** It is the size a purge will return, not
//!   the size of the group. A report that shows freed bytes at merge time teaches the operator to
//!   trust a number that is a retention window old.
//! * **The merge is one transaction.** Repointing and then failing to trash would leave a
//!   library claiming two rows are one file while the pages pointing at the second still resolve
//!   to bytes that are about to be reclaimed.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::browser::FILE_COLUMNS;

/// Longest site id list a cross-site report will read at once.
///
/// A cross-site scan is the platform owner's question ("what does this installation store
/// twice?") and it walks every `media` row in the deployment. Bounded because the answer has to
/// arrive in one response: a scan with no bound is a request that can make the API hold every
/// row of every site in memory, which is the shape of a denial of service dressed as a feature.
pub const MAX_CROSS_SITE_SITES: usize = 200;

/// One place a file is used.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Reference {
    /// Row id.
    pub id: Uuid,
    /// The file being pointed at.
    pub media_id: Uuid,
    /// Kind of referring record (`page`, `theme`, `form`, …).
    pub resource_kind: String,
    /// The referent's own id, as text.
    pub resource_id: String,
    /// Which field points at the file.
    pub field: String,
    /// When the reference was recorded.
    pub created_at: OffsetDateTime,
}

/// What a record may name when it points at a file.
#[derive(Debug, Clone)]
pub struct NewReference {
    /// The file being pointed at.
    pub media_id: Uuid,
    /// Kind of referring record.
    pub resource_kind: String,
    /// The referent's own id, as text.
    pub resource_id: String,
    /// Which field points at the file; empty when the record *is* the file.
    pub field: String,
}

/// One row of the duplicate report.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DuplicateGroup {
    /// Site the group belongs to (null in a cross-site report).
    pub site_id: Option<Uuid>,
    /// The checksum every member shares.
    pub checksum: String,
    /// How many live files share it.
    pub file_count: i32,
    /// Bytes held by all of them.
    pub total_bytes: i64,
    /// Bytes a purge of the copies would return — the group minus one keeper.
    pub reclaimable_bytes: i64,
    /// Earliest upload in the group.
    pub first_seen: OffsetDateTime,
    /// Latest upload in the group.
    pub last_seen: OffsetDateTime,
}

/// One file inside a group, as the expanded row of the report shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateMember {
    /// The file.
    pub file: crate::model::MediaFile,
    /// How many records point at it.
    pub reference_count: i64,
}

/// What a merge changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The file that survived.
    pub kept: Uuid,
    /// The copies that were moved to the trash, in the order they were taken.
    pub trashed: Vec<Uuid>,
    /// How many reference rows now point at the keeper.
    pub references_moved: i64,
    /// How many reference rows pointed at *several* copies and were rewritten once each.
    ///
    /// A page may point at the same file twice (a hero on the list card and on the detail page).
    /// The unique index means one of those rows survives the merge and the rest have to be
    /// dropped, and the number is reported so the panel can say "2 links moved" rather than
    /// "4 links moved" — which is what a per-row counter would claim and what the merge did not do.
    pub references_collapsed: i64,
}

/// A site named by a cross-site report.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SiteLabel {
    /// Site id.
    pub id: Uuid,
    /// Site name, for the cross-site report's "Site" column.
    pub name: String,
}

// ---------------------------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------------------------

/// Record that a file is used somewhere.
///
/// Idempotent on purpose: a page that publishes twice and a `page.published` event that arrives
/// twice both re-record the same references, and a caller that had to remember whether it had
/// already done so would eventually be wrong — a duplicate row for one hero is a "used in" list
/// that lists the same page twice.
pub async fn record_reference(pool: &PgPool, new: &NewReference) -> Result<()> {
    let kind = normalize(&new.resource_kind, "resource_kind")?;
    let resource = normalize(&new.resource_id, "resource_id")?;
    sqlx::query(
        "insert into media_references (media_id, resource_kind, resource_id, field) \
         values ($1, $2, $3, $4) \
         on conflict (media_id, resource_kind, resource_id, field) do nothing",
    )
    .bind(new.media_id)
    .bind(kind)
    .bind(resource)
    .bind(new.field.trim())
    .execute(pool)
    .await?;
    Ok(())
}

/// Every place a file is used, oldest first — the "used in" tab, in a reading order.
pub async fn list_references(pool: &PgPool, media_id: Uuid) -> Result<Vec<Reference>> {
    Ok(sqlx::query_as::<_, Reference>(
        "select id, media_id, resource_kind, resource_id, field, created_at \
         from media_references where media_id = $1 \
         order by created_at, resource_kind, resource_id",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?)
}

/// How many records point at one file.
///
/// `count(distinct (resource_kind, resource_id))` rather than `count(*)`: a page that points at
/// the same image in two fields is *one* usage, and a "used in 4 places" line for one page is
/// the kind of number that makes somebody delete a page.
pub async fn count_references(pool: &PgPool, media_id: Uuid) -> Result<i64> {
    Ok(
        sqlx::query_scalar::<_, i64>(
            "select count(distinct (resource_kind, resource_id)) \
             from media_references where media_id = $1",
        )
        .bind(media_id)
        .fetch_one(pool)
        .await?,
    )
}

/// How many share links over these files are still live.
///
/// The merge reads this *before* it runs, because after it the copies are trashed rows and the
/// links over them have been revoked by the same transaction — so a count taken afterwards is
/// always zero, and a merge that reported "0 links closed" would be reporting the effect of its
/// own write rather than the thing it is warning about.
pub async fn count_live_shares(pool: &PgPool, media_ids: &[Uuid]) -> Result<i64> {
    if media_ids.is_empty() {
        return Ok(0);
    }
    let count: i64 =
        sqlx::query_scalar(
            "select count(*) from media_shares \
              where media_id = any($1) and revoked_at is null",
        )
        .bind(media_ids)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Remove a file's references. Used when a file is purged, when a version is restored and by the
/// repair scan in slice 4.
///
/// `restrict` rather than `cascade` at the call sites that matter: a reference is a claim about
/// another record, and deleting it quietly because a media row went away would turn "this page
/// points at a file that does not exist" into "this page does not point at a file", which reads as
/// a broken editor rather than as a broken page.
pub async fn clear_references(pool: &PgPool, media_id: Uuid) -> Result<u64> {
    let result = sqlx::query("delete from media_references where media_id = $1")
        .bind(media_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Repoint every reference of `from` at `to`, returning `(moved, collapsed)`.
///
/// The unique index `(media_id, resource_kind, resource_id, field)` is what makes this correct
/// rather than merely likely: a page that referenced two copies of the same file has one row per
/// copy, and a plain `update … set media_id = to` would move the first row onto the keeper and
/// then be *refused* on the second — a whole merge rolled back with a `duplicate key` message
/// that names an index and not a cause.
///
/// So the rewrite is written against the keeper's own rows rather than against the index: rows
/// whose referent the keeper already has are simply deleted, and the rest are moved. Nothing
/// can collide afterwards, by construction rather than by luck. The number of deleted rows is
/// reported, because "2 links moved" and "2 links moved, 1 duplicate link collapsed" are
/// different facts about the same merge and only one of them is visible in the pages afterwards.
pub async fn repoint_references(pool: &PgPool, from: Uuid, to: Uuid) -> Result<(i64, i64)> {
    let mut transaction = pool.begin().await?;
    let (moved, collapsed) = repoint_in(&mut transaction, from, to).await?;
    transaction.commit().await?;
    Ok((moved, collapsed))
}

/// Move one file's references onto another, inside an open transaction.
///
/// One statement, so the keeper's rows are read and rewritten atomically against each other.
/// Two concurrent merges that target the same keeper each take their own `for update` locks on
/// their own copies; the loser of the keeper-side race finds the referent already present and
/// collapses rather than colliding, because the `not exists` is evaluated against a snapshot
/// the index then confirms.
async fn repoint_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    from: Uuid,
    to: Uuid,
) -> Result<(i64, i64)> {
    // `query_as::<_, (i64, i64)>` — the statement returns TWO columns. Declaring the row as
    // `((i64, i64),)` asks sqlx for one *composite* column, and PostgreSQL's answer is a plain
    // int8, so the failure reads "Rust type (i64,i64) (as RECORD) is not compatible with INT8"
    // and names the type rather than the mistake.
    let (moved, collapsed): (i64, i64) = sqlx::query_as(
        "with keeper_side as ( \
             select resource_kind, resource_id, field \
             from media_references where media_id = $2 \
         ), dropped as ( \
             delete from media_references r \
             where r.media_id = $1 \
               and exists (select 1 from keeper_side k \
                            where k.resource_kind = r.resource_kind \
                              and k.resource_id = r.resource_id \
                              and k.field = r.field) \
             returning 1 \
         ), rewritten as ( \
             update media_references r set media_id = $2 \
             where r.media_id = $1 \
               and not exists (select 1 from keeper_side k \
                               where k.resource_kind = r.resource_kind \
                                 and k.resource_id = r.resource_id \
                                 and k.field = r.field) \
             returning 1 \
         ) \
         select (select count(*) from rewritten), (select count(*) from dropped)",
    )
    .bind(from)
    .bind(to)
    .fetch_one(&mut **transaction)
    .await?;
    Ok((moved, collapsed))
}

// ---------------------------------------------------------------------------------------------
// The duplicate report
// ---------------------------------------------------------------------------------------------

/// The duplicate groups of one site, largest reclaimable first.
///
/// The ordering is by *waste*, not by group size: a group of two 10 MB files and a group of two
/// 2 kB files are both "a duplicate", and only one of them is worth an operator's attention. A
/// report ordered by file count puts forty kilobyte pairs above a gigabyte.
pub async fn duplicate_groups(pool: &PgPool, site_id: Uuid) -> Result<Vec<DuplicateGroup>> {
    let sql = "select site_id, checksum, file_count, total_bytes, reclaimable_bytes, \
                      first_seen, last_seen \
               from media_duplicate_groups where site_id = $1 \
               order by reclaimable_bytes desc, checksum";
    Ok(sqlx::query_as::<_, DuplicateGroup>(sql)
        .bind(site_id)
        .fetch_all(pool)
        .await?)
}

/// One checksum held more than once anywhere in the named sites.
///
/// **No `FromRow` derive**, unlike every other row type in this crate: the `sites` field is
/// assembled in Rust from a second query, so a derive would demand that `Vec<CrossSiteCopy>`
/// implement `Decode` — and the error it produces ("CrossSiteCopy: Decode is not satisfied")
/// names a struct that is not in the statement at all. The database half is [`CrossSiteRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSiteGroup {
    /// The checksum every copy shares.
    pub checksum: String,
    /// How many live files hold it, across every named site.
    pub file_count: i32,
    /// How many different sites hold it — the number that says "this is not one tenant's mess".
    pub site_count: i32,
    /// Bytes held by all of them.
    pub total_bytes: i64,
    /// Earliest upload.
    pub first_seen: OffsetDateTime,
    /// Latest upload.
    pub last_seen: OffsetDateTime,
    /// The sites that hold it, with their names.
    pub sites: Vec<CrossSiteCopy>,
}

/// The database half of a cross-site group: the columns, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct CrossSiteRow {
    checksum: String,
    file_count: i32,
    site_count: i32,
    total_bytes: i64,
    first_seen: OffsetDateTime,
    last_seen: OffsetDateTime,
}

impl CrossSiteRow {
    /// Attach the located copies, producing the type the route reads.
    fn with_sites(self, sites: Vec<CrossSiteCopy>) -> CrossSiteGroup {
        CrossSiteGroup {
            checksum: self.checksum,
            file_count: self.file_count,
            site_count: self.site_count,
            total_bytes: self.total_bytes,
            first_seen: self.first_seen,
            last_seen: self.last_seen,
            sites,
        }
    }
}

/// Where one copy of a cross-site group lives.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CrossSiteCopy {
    /// The checksum this site holds a copy of — the join key back to its group.
    pub checksum: String,
    /// Site id.
    pub site_id: Uuid,
    /// Site name, for the report's "Site" column.
    pub site_name: String,
    /// How many live copies that site holds.
    pub file_count: i32,
    /// Bytes that site holds.
    pub site_bytes: i64,
}

/// The installation-wide report: which bytes this deployment holds more than once.
///
/// This is *not* the per-site report with the site filter lifted, and the difference is the
/// whole point of the mode. Per-site, the interesting case is "two copies in one library": a
/// merge decides which of them a page resolves to. Across sites, the interesting case is "these
/// two tenants hold the same file" — which is a *platform owner's* storage question with no
/// single-click answer, because a merge repoints rows inside one site and cannot decide which
/// tenant keeps the file. So the grouping is by checksum alone, and `reclaimable` is absent
/// rather than zero: there is nothing to offer a button for.
pub async fn duplicate_groups_across(
    pool: &PgPool,
    site_ids: &[Uuid],
) -> Result<Vec<CrossSiteGroup>> {
    if site_ids.is_empty() {
        return Ok(Vec::new());
    }
    if site_ids.len() > MAX_CROSS_SITE_SITES {
        return Err(MediaError::TooManySites {
            limit: MAX_CROSS_SITE_SITES,
            requested: site_ids.len(),
        });
    }
    // Grouped here rather than read from the view and filtered by an `exists`: the view counts
    // every site in the installation, so filtering it afterwards would report `file_count = 9`
    // for a checksum the caller named two sites for — a total that includes the copies they were
    // not asking about. The view stays the *documented* shape; this is the scoped version of it.
    let rows: Vec<CrossSiteRow> = sqlx::query_as(
        "select checksum, count(*)::integer as file_count, \
                count(distinct site_id)::integer as site_count, \
                sum(size_bytes)::bigint as total_bytes, \
                min(created_at) as first_seen, max(created_at) as last_seen \
         from media \
         where site_id = any($1) and deleted_at is null and checksum <> '' \
         group by checksum \
         having count(*) > 1 \
         order by total_bytes desc, checksum",
    )
    .bind(site_ids)
    .fetch_all(pool)
    .await?;

    let wanted: Vec<String> = rows.iter().map(|row| row.checksum.clone()).collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let copies: Vec<CrossSiteCopy> = sqlx::query_as(
        "select m.checksum, m.site_id, s.name as site_name, count(*)::integer as file_count, \
                sum(m.size_bytes)::bigint as site_bytes \
         from media m join sites s on s.id = m.site_id \
         where m.checksum = any($2) and m.site_id = any($1) and m.deleted_at is null \
         group by m.checksum, m.site_id, s.name \
         order by s.name",
    )
    .bind(site_ids)
    .bind(&wanted)
    .fetch_all(pool)
    .await?;

    // Grouped in Rust rather than by a second statement per group: the copy list is small
    // (one row per site holding the bytes) and a query per group would make an owner's report
    // over a hundred checksums a hundred round trips.
    Ok(rows
        .into_iter()
        .map(|row| {
            let sites = copies
                .iter()
                .filter(|copy| copy.checksum == row.checksum)
                .cloned()
                .collect();
            row.with_sites(sites)
        })
        .collect())
}

/// The names of the sites a cross-site report covers, for its "Site" column.
pub async fn site_labels(pool: &PgPool, site_ids: &[Uuid]) -> Result<Vec<SiteLabel>> {
    if site_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, SiteLabel>(
        "select id, name from sites where id = any($1) order by name",
    )
    .bind(site_ids)
    .fetch_all(pool)
    .await?)
}

/// The live files of one duplicate group, oldest first.
///
/// The keeper is *not* chosen here: the order is the upload order, and the caller picks. A report
/// that pre-sorts a "suggested keeper" teaches the operator to accept the first row without
/// checking which one their pages use.
pub async fn group_members(
    pool: &PgPool,
    site_id: Uuid,
    checksum: &str,
) -> Result<Vec<DuplicateMember>> {
    let sql = format!(
        "select {FILE_COLUMNS} from media \
         where site_id = $1 and checksum = $2 and deleted_at is null \
         order by created_at, id"
    );
    let files: Vec<crate::model::MediaFile> = sqlx::query_as(&sql)
        .bind(site_id)
        .bind(checksum)
        .fetch_all(pool)
        .await?;

    let mut members = Vec::with_capacity(files.len());
    for file in files {
        members.push(DuplicateMember {
            reference_count: count_references(pool, file.id).await?,
            file,
        });
    }
    Ok(members)
}

/// Total reclaimable bytes across a site's groups — the report's headline number.
pub async fn reclaimable_total(pool: &PgPool, site_id: Uuid) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "select sum(reclaimable_bytes)::bigint from media_duplicate_groups where site_id = $1",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await?
    .unwrap_or(0))
}

// ---------------------------------------------------------------------------------------------
// The merge
// ---------------------------------------------------------------------------------------------

/// Merge a duplicate group down to one file.
///
/// The caller names the keeper. Everything else here exists to make that one decision safe:
///
/// 1. The keeper and every copy are read in the same transaction, so a file trashed by somebody
///    else between the report and the click cannot be merged silently.
/// 2. A keeper that is not in the group is refused. Merging "the group" when the id belongs to
///    another file would repoint references onto a file that has different bytes.
/// 3. References are repointed *before* the copies are trashed, so a failure in between leaves
///    every page still resolving — the copies are recoverable from the trash, which is the
///    correct order for a destructive step: fix what points at the file, then stop serving the
///    copy.
/// 4. The copies are trashed, never deleted, and the `deleted_by` is the operator — so the trash
///    screen shows who merged them and a restore reverses the whole thing.
///
/// The whole thing is one transaction. A merge that repointed and then failed would leave pages
/// pointing at a file the library has just agreed is a duplicate of a different one.
pub async fn merge_group(
    pool: &PgPool,
    site_id: Uuid,
    checksum: &str,
    keep: Uuid,
    by: Uuid,
) -> Result<MergeOutcome> {
    if checksum.trim().is_empty() {
        return Err(MediaError::MergeRefused {
            reason: "no checksum was named".to_owned(),
        });
    }

    let mut transaction = pool.begin().await?;

    let sql = format!(
        "select {FILE_COLUMNS} from media \
         where site_id = $1 and checksum = $2 and deleted_at is null \
         order by created_at, id for update"
    );
    let members: Vec<crate::model::MediaFile> = sqlx::query_as(&sql)
        .bind(site_id)
        .bind(checksum)
        .fetch_all(&mut *transaction)
        .await?;

    if members.len() < 2 {
        // Either the group is gone or it was never a group. Both are "there is nothing to merge",
        // and answering 404 rather than 409 keeps the route from being a checksum oracle.
        return Err(MediaError::MergeRefused {
            reason: "this group no longer holds two live files".to_owned(),
        });
    }
    if !members.iter().any(|member| member.id == keep) {
        return Err(MediaError::MergeRefused {
            reason: "the file to keep is not one of the files in this group".to_owned(),
        });
    }

    let copies: Vec<Uuid> = members
        .iter()
        .map(|member| member.id)
        .filter(|id| *id != keep)
        .collect();

    // A share over a copy must stop reaching the file *before* it is trashed, and a share over
    // the keeper keeps working — the keeper is the file the pages still resolve to.
    let closed = sqlx::query(
        "update media_shares set revoked_at = now(), \
                revoked_reason = 'the file was merged into another copy of the same bytes' \
         where media_id = any($1) and revoked_at is null",
    )
    .bind(&copies)
    .execute(&mut *transaction)
    .await?
    .rows_affected() as i64;

    let mut references_moved = 0i64;
    let mut references_collapsed = 0i64;
    for copy in &copies {
        let (moved, collapsed) = repoint_in(&mut transaction, *copy, keep).await?;
        references_moved += moved;
        references_collapsed += collapsed;
    }

    let trashed = sqlx::query(
        "update media set deleted_at = now(), deleted_by = $2, updated_at = now() \
         where id = any($1) and deleted_at is null",
    )
    .bind(&copies)
    .bind(by)
    .execute(&mut *transaction)
    .await?
    .rows_affected();

    transaction.commit().await?;

    tracing::info!(
        site_id = %site_id,
        checksum = %checksum,
        kept = %keep,
        trashed = trashed,
        references_moved = references_moved,
        references_collapsed = references_collapsed,
        shares_revoked = closed,
        "duplicate group merged"
    );

    Ok(MergeOutcome {
        kept: keep,
        trashed: copies,
        references_moved,
        references_collapsed,
    })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Trim a required string and refuse an empty one, naming the field.
fn normalize(value: &str, field: &'static str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MediaError::MergeRefused {
            reason: format!("`{field}` is required"),
        });
    }
    Ok(trimmed.to_owned())
}
