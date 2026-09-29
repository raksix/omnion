//! The restore preview's walk over live data (REQ-013, slice 2).
//!
//! A restore replaces live rows, so the wizard's job is to price that replacement *before*
//! it happens. This module is the half that reads the live side, kept separate from
//! [`crate::restore`] (which is pure) for the reason that separation is always worth: the
//! decision rules can then be unit-tested with no database, and a change in how live data is
//! counted cannot silently alter which warnings fire.
//!
//! # What is counted, and the three answers that look alike
//!
//! For every part, a restore has three possible relationships to the live data:
//!
//! * **both** — the archive holds it and live has it: it is *overwritten*.
//! * **archive only** — it is *added*.
//! * **live only** — it was created after the run and is *dropped*.
//!
//! The dangerous one is the third, and it is the one a manifest cannot show. The
//! `media` part is a plain list of object keys, so "dropped" is a set difference against the
//! live library; the `database` part is a **count**, not a list, so for it the honest answer
//! is "this many rows, we cannot say which" — and the preview says exactly that rather than
//! inventing a per-row loss. Both are reported; neither is dressed up as the other.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::restore::LiveCounts;

/// The largest number of object keys a preview will compare in one statement.
///
/// A restore of a 200 000-object library would otherwise build a 200 000-element `any($1)`
/// array, which is a statement PostgreSQL has to plan and a request that can time out. A
/// preview that fails to answer looks identical to a preview that found nothing, so the
/// comparison is bounded and the *capping itself* becomes a reported fact: when the archive
/// is bigger than the cap the counts are a floor, and the preview raises it rather than
/// quoting a number it computed from a truncated list.
pub const MAX_MATCHED_KEYS: i64 = 5_000;

/// How live data compares with an archive's, as three facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveComparison {
    /// Live rows/objects that exist now, within the tenant.
    pub live_total: i64,
    /// Of those, the ones the archive also holds.
    pub live_matches: i64,
    /// Live rows/objects the archive does not hold — these the restore drops.
    pub live_only: i64,
    /// Whether the object comparison was truncated at [`MAX_MATCHED_KEYS`].
    ///
    /// `true` means every count here is a **floor**, and the preview must say so. Reporting
    /// a truncated count as the number is the one way this module could turn a bounded
    /// safety check into a false reassurance.
    pub truncated: bool,
    /// Whether the live side is the platform's own rows rather than a tenant's.
    ///
    /// `false` on a multi-tenant installation means "this is every tenant's rows, so treat
    /// the number as an upper bound" — and it is a separate flag from `truncated` because
    /// they are different faults: one is a bounded check, the other is a coarse one.
    pub scoped: bool,
}

impl LiveComparison {
    /// The two numbers [`crate::restore::build_preview`] costs a restore with.
    #[must_use]
    pub fn counts(&self) -> LiveCounts {
        LiveCounts {
            total: self.live_total,
            matching: self.live_matches,
        }
    }
}

/// Count the live `media` rows this tenant owns, and how many the archive also holds.
///
/// Scoped by organization through `sites` for the same reason the media part is: an
/// unscoped count is a single-tenant deployment's correct answer and a multi-tenant
/// deployment's data leak, and a *count* leak is quieter than a bytes leak because the
/// number still looks plausible.
///
/// `deleted_at` and `purged_at` are honoured exactly as the backup part honours them, so the
/// preview and the archive agree on what "the library" means. A preview that counted trashed
/// files would price a restore as dropping rows the restore would never touch.
pub async fn compare_media(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    archive_storage_keys: &[String],
) -> Result<LiveComparison> {
    let live_total: i64 = sqlx::query_scalar(
        "select count(*) from media m join sites s on s.id = m.site_id \
         where m.deleted_at is null and m.purged_at is null \
           and (s.organization_id is not distinct from $1)",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    // No archive list at all means the media part is not being restored, so there is nothing
    // to overwrite or drop and the honest counts are "no live impact", not "0 matches out of
    // a library we did not look at".
    if archive_storage_keys.is_empty() {
        return Ok(LiveComparison {
            live_total,
            live_matches: 0,
            live_only: 0,
            truncated: false,
            scoped: organization_id.is_none(),
        });
    }

    let truncated = archive_storage_keys.len() as i64 > MAX_MATCHED_KEYS;
    let capped = if truncated {
        &archive_storage_keys[..MAX_MATCHED_KEYS as usize]
    } else {
        archive_storage_keys
    };

    let live_matches: i64 = sqlx::query_scalar(
        "select count(*) from media m join sites s on s.id = m.site_id \
         where m.deleted_at is null and m.purged_at is null \
           and (s.organization_id is not distinct from $1) \
           and m.storage_key = any($2)",
    )
    .bind(organization_id)
    .bind(capped)
    .fetch_one(pool)
    .await?;

    Ok(LiveComparison {
        live_total,
        live_matches,
        live_only: (live_total - live_matches).max(0),
        truncated,
        scoped: organization_id.is_none(),
    })
}

/// Read the live row counts for the non-object parts, which are counts by nature.
///
/// `database` is the only replacing part without an object list, and it is counted as
/// "rows the archive recorded" versus "rows live now".
///
/// **A tenant gets the platform-wide count, and `scoped` says so.** The platform's own
/// tables are not partitioned by `organization_id`, so there is no tenant-specific number to
/// read — and the tempting shortcut, reusing the media part's tenant-scoped count, would
/// price a database restore by the size of a media library. The comparison is therefore
/// honest about what it measured: on a single-tenant installation the number is exact, and
/// on a multi-tenant one it is an upper bound, which is the direction that makes the
/// `data_loss` warning fire when it should rather than stay quiet.
pub async fn compare_database(
    pool: &PgPool,
    archive_item_count: i32,
    organization_id: Option<Uuid>,
) -> Result<LiveComparison> {
    let live_total = count_platform_rows(pool).await?;
    let archived = i64::from(archive_item_count);
    Ok(LiveComparison {
        live_total,
        live_matches: archived.min(live_total).max(0),
        live_only: (live_total - archived).max(0),
        truncated: false,
        scoped: organization_id.is_none(),
    })
}

/// Every row in every public table.
///
/// One statement, for the reason `document_database` already spells out: the per-table
/// `query_to_xml` walk hits PostgreSQL's stack depth limit on an installation with a hundred
/// tables and answers `500 stack depth limit exceeded` on the first backup anybody takes.
async fn count_platform_rows(pool: &PgPool) -> Result<i64> {
    let total: i64 = sqlx::query_scalar(
        "select coalesce(sum(row_count), 0)::bigint from ( \
             select (xpath('/row/c/text()', query_to_xml( \
                 format('select count(*) as c from %I.%I', t.table_schema, t.table_name), \
                 false, true, '')))[1]::text::bigint as row_count \
             from information_schema.tables t \
             where t.table_schema = 'public' and t.table_type = 'BASE TABLE' \
         ) counted",
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}
