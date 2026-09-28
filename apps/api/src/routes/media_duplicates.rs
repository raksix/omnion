//! Duplicate detection and merge (REQ-010, slice 3): `/api/v1/media/duplicates`.
//!
//! One report, one destructive action, and a cross-site mode that is not merely a wider version of
//! the same query. Three decisions hold across this file:
//!
//! * **Reading the report is `media.read`, merging is `media.manage`.** Looking at what a site
//!   stores twice costs nothing and helps everybody; changing which row a page resolves to is a
//!   different power, and a team that may organise a library is not thereby a team that may
//!   rewrite what its pages point at.
//! * **The cross-site mode is a permission *and* a label, not a wider default.** Grouping across
//!   sites answers "what does this installation store twice?", which is a platform question, and a
//!   tenant account that could see the whole installation's storage profile would be a
//!   cross-tenant information leak wearing a feature's clothes. `platform_only` refuses it even
//!   when the caller holds every permission, for the same reason the tenancy surface refuses it.
//! * **The report never picks the keeper, and the merge never picks one either.** `keep` is a
//!   required field. An automatic choice breaks a live page, and the operator finds out from a
//!   404 rather than from the report — which is the one failure mode a storage tool must not have.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::scope::platform_only;
use crate::state::AppState;

use omnion_media::{
    CrossSiteCopy, CrossSiteGroup, DuplicateGroup, MediaFile, MergeOutcome, duplicate_groups,
    duplicate_groups_across, group_members, merge_group,
};

/// Shortest checksum a group may be named by.
///
/// A hex SHA-256 is 64 characters, and the row's own `checksum` column is what this is compared
/// against. Refusing a short one turns "the panel sent the first eight characters because the
/// column was truncated" into a `400` naming the field instead of an empty report.
const MIN_CHECKSUM_LENGTH: usize = 32;

/// Longest a checksum may be, so a caller cannot push an unbounded string through the index.
const MAX_CHECKSUM_LENGTH: usize = 128;

/// What the report is asked for.
#[derive(Debug, Default, Deserialize)]
pub struct DuplicateQuery {
    /// Site the report covers. Required unless `sites` names several.
    pub site_id: Option<Uuid>,
    /// Several sites, for the cross-site report. Only a platform account may send this.
    pub sites: Option<String>,
    /// Whether to include each group's member rows. Off by default: a library with four hundred
    /// duplicate pairs would answer with four thousand rows for a report the operator reads
    /// once, and the expansion costs one click.
    pub expand: Option<String>,
}

/// A cross-site group: one checksum the installation holds in more than one place.
///
/// Deliberately has no `reclaimable_bytes`. The bytes are real storage, but no button on this
/// report can return them — a merge repoints rows inside one site and cannot decide which tenant
/// keeps the file — so a "reclaimable" column here would be a number with no action behind it,
/// and an operator would quote it as a saving.
#[derive(Debug, Serialize)]
pub struct CrossSiteGroupBody {
    /// The checksum, short.
    pub checksum: String,
    /// The full checksum.
    pub full_checksum: String,
    /// How many live copies the installation holds.
    pub file_count: i32,
    /// How many sites hold it.
    pub site_count: i32,
    /// Bytes held by all of them.
    pub total_bytes: i64,
    /// Earliest upload.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen: time::OffsetDateTime,
    /// Latest upload.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: time::OffsetDateTime,
    /// Where the copies are.
    pub sites: Vec<CrossSiteCopyBody>,
}

/// One site's holding of a cross-site group.
#[derive(Debug, Serialize)]
pub struct CrossSiteCopyBody {
    /// Site id.
    pub site_id: Uuid,
    /// Site name.
    pub site_name: String,
    /// Copies that site holds.
    pub file_count: i32,
    /// Bytes that site holds.
    pub site_bytes: i64,
}

impl From<&CrossSiteCopy> for CrossSiteCopyBody {
    fn from(copy: &CrossSiteCopy) -> Self {
        Self {
            site_id: copy.site_id,
            site_name: copy.site_name.clone(),
            file_count: copy.file_count,
            site_bytes: copy.site_bytes,
        }
    }
}

/// The installation-wide report.
#[derive(Debug, Serialize)]
pub struct CrossSiteReport {
    /// The sites it covered.
    pub site_ids: Vec<Uuid>,
    /// Always true; present so a client reads one field rather than inferring from the shape.
    pub cross_site: bool,
    /// How many checksums.
    pub group_count: usize,
    /// Bytes held by all the listed copies.
    pub total_bytes: i64,
    /// The groups, largest first.
    pub groups: Vec<CrossSiteGroupBody>,
    /// Why there is nothing to merge from here, in words the screen shows rather than hides.
    pub notice: String,
}

/// One group as the report returns it.
#[derive(Debug, Serialize)]
pub struct DuplicateGroupBody {
    /// Site the group belongs to.
    pub site_id: Uuid,
    /// Site name. Present only when the caller asked for names; the per-site report is already
    /// scoped to one site, so it is `None` there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    /// The checksum every member shares — short, for the eye and for a re-lookup.
    pub checksum: String,
    /// The full checksum, for the copy button that compares two files by hand.
    pub full_checksum: String,
    /// How many live files share it.
    pub file_count: i32,
    /// Bytes held by all of them.
    pub total_bytes: i64,
    /// Bytes a purge of the copies will return — the group minus one keeper.
    pub reclaimable_bytes: i64,
    /// Earliest upload in the group.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen: time::OffsetDateTime,
    /// Latest upload in the group.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: time::OffsetDateTime,
    /// The members, when the caller asked to expand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<DuplicateFileBody>>,
}

/// One file inside a group.
#[derive(Debug, Serialize)]
pub struct DuplicateFileBody {
    /// Media id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Folder it sits in.
    pub folder_id: Option<Uuid>,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Content type.
    pub content_type: String,
    /// When it was uploaded.
    #[serde(with = "time::serde::rfc3339")]
    pub uploaded_at: time::OffsetDateTime,
    /// How many records point at it.
    pub reference_count: i64,
    /// Panel read path, so the report can link to it.
    pub raw_path: String,
}

/// The whole report.
#[derive(Debug, Serialize)]
pub struct DuplicateReport {
    /// The sites the report covers.
    pub site_ids: Vec<Uuid>,
    /// Whether this was the cross-site mode.
    pub cross_site: bool,
    /// Bytes a purge of every listed group's copies would return.
    pub reclaimable_bytes: i64,
    /// How many groups.
    pub group_count: usize,
    /// The groups, largest reclaimable first.
    pub groups: Vec<DuplicateGroupBody>,
}

/// What a merge is asked to do.
#[derive(Debug, Deserialize)]
pub struct MergeInput {
    /// Site the group belongs to.
    pub site_id: Uuid,
    /// The group's checksum.
    pub checksum: String,
    /// The file to keep. Required: the caller names the keeper, never the platform.
    pub keep: Uuid,
}

/// What a merge did.
#[derive(Debug, Serialize)]
pub struct MergeBody {
    /// The file that survived.
    pub kept: Uuid,
    /// The copies moved to the trash, with the instant they will be purged if nobody restores them.
    pub trashed: Vec<Uuid>,
    /// How many reference rows now point at the keeper.
    pub references_moved: i64,
    /// How many duplicate reference rows the merge collapsed.
    pub references_collapsed: i64,
    /// Bytes the copies hold — *pending*, not reclaimed: they are in the trash.
    pub bytes_pending_purge: i64,
    /// Share links that were closed because the file behind them is gone.
    pub shares_revoked: i64,
    /// The sentence the screen shows under the result.
    pub notice: String,
}

impl MergeBody {
    /// Describe a merge in words, so the screen does not have to invent a sentence that might
    /// claim the bytes are already back.
    fn build(outcome: &MergeOutcome, bytes: i64, shares: i64) -> Self {
        let moved = outcome.references_moved - outcome.references_collapsed;
        let mut notice = format!(
            "{} {} merged into the file you kept.",
            plural(outcome.trashed.len() as i64, "copy", "copies"),
            if moved == 0 {
                "were".to_owned()
            } else {
                "was".to_owned()
            },
        );
        if outcome.references_collapsed > 0 {
            notice.push_str(&format!(
                " {} duplicate {} collapsed.",
                outcome.references_collapsed,
                if outcome.references_collapsed == 1 {
                    "link was"
                } else {
                    "links were"
                }
            ));
        }
        // The subject changes with the count, and a sentence that ignores it reads as broken
        // English on exactly the screen an operator reads while deciding. The walkthrough caught
        // this in the plural case: "their bytes are is only reclaimed", because the verb phrase
        // and the "is" were both carried by one branch.
        let (subject, verb) = if outcome.trashed.len() == 1 {
            ("is in the trash and its".to_owned(), "is")
        } else {
            ("are in the trash and their".to_owned(), "are")
        };
        notice.push_str(&format!(
            " The {} {subject} {bytes} {} {verb} only reclaimed when the trash is purged.",
            plural(outcome.trashed.len() as i64, "copy", "copies"),
            if bytes == 1 { "byte" } else { "bytes" },
        ));
        if shares > 0 {
            notice.push_str(&format!(
                " {} {} closed, because the file behind {} no longer exists.",
                shares,
                if shares == 1 { "link was" } else { "links were" },
                if shares == 1 { "it" } else { "them" },
            ));
        }
        Self {
            kept: outcome.kept,
            trashed: outcome.trashed.clone(),
            references_moved: moved.max(0),
            references_collapsed: outcome.references_collapsed,
            bytes_pending_purge: bytes,
            shares_revoked: shares,
            notice,
        }
    }
}

/// `one copy` / `3 copies`, without the `1 file(s)` construction.
fn plural(count: i64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The duplicate report of a site, or of the whole installation for a platform owner.
///
/// Two shapes rather than one with an optional part: the per-site report is actionable (every
/// group has a keeper to pick) and the cross-site one is not (no merge can decide which tenant
/// keeps a file), so a client that read one shape and got the other would find a `Merge` button
/// on a row that cannot be merged.
pub async fn report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DuplicateQuery>,
) -> std::result::Result<Response, ApiError> {
    let pool = state.db().pool();

    // Two sites is one question with a different scope, so it is a different code path rather
    // than a wider `site_id`: the platform check has to happen *before* any rows are read, not
    // after they have been counted.
    if let Some(raw) = query.sites.as_deref().map(str::trim).filter(|raw| !raw.is_empty()) {
        platform_only(&current)?;
        let site_ids = parse_sites(raw)?;
        if site_ids.is_empty() {
            return Err(ApiError::bad_request(
                "sites",
                "name at least one site, or use `site_id` for a single one",
            ));
        }
        let groups = duplicate_groups_across(pool, &site_ids).await?;
        return Ok(Json(cross_site_body(site_ids, groups)).into_response());
    }

    let site_id = query
        .site_id
        .ok_or_else(|| ApiError::bad_request("site_id", "a duplicate report covers one site"))?;
    let site = site_in_scope(&state, &current, site_id).await?;
    let groups = duplicate_groups(pool, site.id).await?;
    let body = build_report(pool, vec![site.id], groups, expanded(&query)).await?;
    Ok(Json(body).into_response())
}

/// Merge a group down to one file: the keeper the caller named, references repointed, the
/// copies in the trash.
pub async fn merge(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<MergeInput>,
) -> std::result::Result<Json<MergeBody>, ApiError> {
    let pool = state.db().pool();
    let site = site_in_scope(&state, &current, input.site_id).await?;
    let checksum = input.checksum.trim().to_owned();
    if checksum.len() < MIN_CHECKSUM_LENGTH || checksum.len() > MAX_CHECKSUM_LENGTH {
        return Err(ApiError::bad_request(
            "checksum",
            format!(
                "a checksum is {MIN_CHECKSUM_LENGTH}–{MAX_CHECKSUM_LENGTH} characters; the report \\
                 carries the full value"
            ),
        ));
    }

    // The copies' bytes are read *before* the merge, because after it they are trashed rows and
    // the report they came from no longer exists. A merge that reported "0 bytes pending" because
    // it looked afterwards would be a number that is wrong in the only direction that matters.
    let before: Vec<MediaFile> = group_members(pool, site.id, &checksum)
        .await?
        .into_iter()
        .map(|member| member.file)
        .filter(|file| file.id != input.keep)
        .collect();
    let bytes: i64 = before.iter().map(|file| file.size() as i64).sum();
    let shares: i64 = count_live_shares(pool, &before.iter().map(|f| f.id).collect::<Vec<_>>()).await?;

    let outcome = merge_group(pool, site.id, &checksum, input.keep, current.user.id).await?;

    bus::emit(
        pool,
        NewEvent::new("media.duplicate_merged")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "kept": outcome.kept,
                "trashed": outcome.trashed,
                "checksum": checksum,
                "references_moved": outcome.references_moved,
                "references_collapsed": outcome.references_collapsed,
                "bytes_pending_purge": bytes,
            })),
    )
    .await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "media.duplicate_merged")
            .target("media", outcome.kept.to_string())
            .metadata(json!({
                "site_id": site.id,
                "checksum": checksum,
                "kept": outcome.kept,
                "trashed": outcome.trashed,
                "references_moved": outcome.references_moved,
                "references_collapsed": outcome.references_collapsed,
                "bytes_pending_purge": bytes,
                "shares_revoked": shares,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(MergeBody::build(&outcome, bytes, shares)))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Whether the caller asked for the member rows.
fn expanded(query: &DuplicateQuery) -> bool {
    query
        .expand
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Build the installation-wide report body.
fn cross_site_body(site_ids: Vec<Uuid>, groups: Vec<CrossSiteGroup>) -> CrossSiteReport {
    let total_bytes: i64 = groups.iter().map(|group| group.total_bytes).sum();
    let bodies: Vec<CrossSiteGroupBody> = groups
        .into_iter()
        .map(|group| CrossSiteGroupBody {
            checksum: short_checksum(&group.checksum),
            full_checksum: group.checksum.clone(),
            file_count: group.file_count,
            site_count: group.site_count,
            total_bytes: group.total_bytes,
            first_seen: group.first_seen,
            last_seen: group.last_seen,
            sites: group.sites.iter().map(CrossSiteCopyBody::from).collect(),
        })
        .collect();
    CrossSiteReport {
        group_count: bodies.len(),
        total_bytes,
        notice: "This report says where each file is stored more than once across the whole \
                 installation. Reclaiming it is a per-site decision — open the site's own \
                 duplicate report to merge the copies inside it."
            .to_owned(),
        site_ids,
        cross_site: true,
        groups: bodies,
    }
}

/// Build the response body, expanding each group when asked.
///
/// `expand` costs one query per group rather than one big one: a library with four hundred groups
/// that nobody expands pays nothing, and one that is expanded pays four hundred cheap indexed
/// reads rather than a single statement whose result set nobody is going to scroll.
async fn build_report(
    pool: &sqlx::PgPool,
    site_ids: Vec<Uuid>,
    groups: Vec<DuplicateGroup>,
    expand: bool,
) -> std::result::Result<DuplicateReport, ApiError> {
    let mut reclaimable = 0i64;
    let mut bodies = Vec::with_capacity(groups.len());
    for group in groups {
        reclaimable += group.reclaimable_bytes;
        let site_id = group
            .site_id
            .expect("a group always carries the site it was grouped for");
        let files = if expand {
            let members = group_members(pool, site_id, &group.checksum).await?;
            Some(
                members
                    .into_iter()
                    .map(|member| DuplicateFileBody {
                        id: member.file.id,
                        filename: member.file.filename,
                        folder_id: member.file.folder_id,
                        size_bytes: member.file.size_bytes,
                        content_type: member.file.content_type,
                        uploaded_at: member.file.created_at,
                        reference_count: member.reference_count,
                        raw_path: format!("/api/v1/media/{}/raw", member.file.id),
                    })
                    .collect(),
            )
        } else {
            None
        };
        bodies.push(DuplicateGroupBody {
            site_id,
            site_name: None,
            checksum: short_checksum(&group.checksum),
            full_checksum: group.checksum.clone(),
            file_count: group.file_count,
            total_bytes: group.total_bytes,
            reclaimable_bytes: group.reclaimable_bytes,
            first_seen: group.first_seen,
            last_seen: group.last_seen,
            files,
        });
    }
    Ok(DuplicateReport {
        site_ids,
        cross_site: false,
        reclaimable_bytes: reclaimable,
        group_count: bodies.len(),
        groups: bodies,
    })
}

/// The first sixteen characters of a checksum, with an ellipsis — enough to recognise a row, not
/// enough to paste into anything.
fn short_checksum(full: &str) -> String {
    let head: String = full.chars().take(16).collect();
    if full.chars().count() <= 16 {
        return full.to_owned();
    }
    format!("{head}…")
}

/// Parse a `sites=a,b,c` list.
///
/// Split on commas rather than whitespace so `?sites=` copied out of a URL works as written, and
/// so a `+` that arrives percent-decoded as a space cannot smuggle an extra id into a list the
/// caller did not read.
fn parse_sites(raw: &str) -> std::result::Result<Vec<Uuid>, ApiError> {
    let mut ids = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = Uuid::parse_str(part).map_err(|_| {
            ApiError::bad_request("sites", format!("`{part}` is not a site id"))
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// How many live share links the files hold — reported so a merge says it closed them.
///
/// Read through the crate rather than raw, for the same reason every other statement here is:
/// `apps/api` has no `From<sqlx::Error>` for `ApiError`, and a route that reached past the crate
/// to answer its own query would have to invent an error mapping that only exists on this one
/// screen.
async fn count_live_shares(
    pool: &sqlx::PgPool,
    media_ids: &[Uuid],
) -> std::result::Result<i64, ApiError> {
    Ok(omnion_media::count_live_shares(pool, media_ids).await?)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn a_site_list_survives_the_shape_a_url_actually_arrives_in() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(parse_sites(&format!("{a},{b}")).expect("ids"), vec![a, b]);
        assert_eq!(parse_sites(&format!(" {a} , {b} ")).expect("ids"), vec![a, b]);
        // A repeated id is one site, not two rows in the report.
        assert_eq!(parse_sites(&format!("{a},{a}")).expect("ids"), vec![a]);
        assert!(parse_sites("").expect("empty list").is_empty());
    }

    #[test]
    fn a_site_list_refuses_what_is_not_a_site_id() {
        let err = parse_sites("not-a-uuid").expect_err("must refuse");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.code(), "sites");
    }

    #[test]
    fn a_short_checksum_is_readable_and_still_not_pasteable() {
        let full = "a".repeat(64);
        let short = short_checksum(&full);
        assert!(short.chars().count() < 20, "{short}");
        assert!(full.starts_with(short.trim_end_matches('…')));
        // A checksum of exactly the cut is returned whole: the ellipsis is for a *long* value.
        assert_eq!(short_checksum(&"a".repeat(16)), "a".repeat(16));
    }

    #[test]
    fn a_merge_notice_does_not_claim_the_bytes_are_back() {
        let outcome = MergeOutcome {
            kept: Uuid::nil(),
            trashed: vec![Uuid::nil()],
            references_moved: 2,
            references_collapsed: 1,
        };
        let body = MergeBody::build(&outcome, 4096, 0);
        assert!(body.notice.contains("in the trash"), "{}", body.notice);
        assert!(
            body.notice.contains("only reclaimed when the trash is purged"),
            "{}",
            body.notice
        );
        // The moved count is the *net* one: a collapsed row is not a second link moved.
        assert_eq!(body.references_moved, 1);
        assert_eq!(body.references_collapsed, 1);
    }

    #[test]
    fn the_notice_is_grammatical_in_both_numbers() {
        // The first draft carried the verb phrase and the "is" on the same branch, so the plural
        // case read "their bytes are is only reclaimed" — caught by the walkthrough screenshot
        // path, not by an assertion, because every word was present and only the grammar was
        // wrong. Both numbers are pinned here.
        let one = MergeBody::build(
            &MergeOutcome {
                kept: Uuid::nil(),
                trashed: vec![Uuid::nil()],
                references_moved: 0,
                references_collapsed: 0,
            },
            1,
            0,
        );
        assert!(
            one.notice.contains("is in the trash and its 1 byte is only reclaimed"),
            "{}",
            one.notice
        );
        assert!(!one.notice.contains("is is"), "{}", one.notice);

        let many = MergeBody::build(
            &MergeOutcome {
                kept: Uuid::nil(),
                trashed: vec![Uuid::nil(); 5],
                references_moved: 0,
                references_collapsed: 0,
            },
            5000,
            0,
        );
        assert!(
            many.notice.contains("are in the trash and their 5000 bytes are only reclaimed"),
            "{}",
            many.notice
        );
        assert!(!many.notice.contains("are is"), "{}", many.notice);
    }

    #[test]
    fn a_merge_that_closed_links_says_so() {
        let outcome = MergeOutcome {
            kept: Uuid::nil(),
            trashed: vec![Uuid::nil(), Uuid::nil()],
            references_moved: 0,
            references_collapsed: 0,
        };
        let body = MergeBody::build(&outcome, 0, 3);
        assert!(body.notice.contains("3 links were closed"), "{}", body.notice);
        assert!(body.notice.contains("2 copies are in the trash"), "{}", body.notice);
    }
}
