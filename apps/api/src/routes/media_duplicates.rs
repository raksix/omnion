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
    DuplicateGroup, MediaFile, MergeOutcome, duplicate_groups, duplicate_groups_across,
    group_members, merge_group, site_labels,
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

/// One group as the report returns it.
#[derive(Debug, Serialize)]
pub struct DuplicateGroupBody {
    /// Site the group belongs to.
    pub site_id: Uuid,
    /// Site name, present only in a cross-site report.
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
        notice.push_str(&format!(
            " The {} are in the trash and their {} is only reclaimed when the trash is purged.",
            plural(outcome.trashed.len() as i64, "copy", "copies"),
            if bytes == 1 { "byte is" } else { "bytes are" },
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
pub async fn report(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DuplicateQuery>,
) -> std::result::Result<Json<DuplicateReport>, ApiError> {
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
        let labels = site_labels(pool, &site_ids).await?;
        let groups = duplicate_groups_across(pool, &site_ids).await?;
        let body = build_report(pool, site_ids, true, groups, &labels, expanded(&query)).await?;
        return Ok(Json(body));
    }

    let site_id = query
        .site_id
        .ok_or_else(|| ApiError::bad_request("site_id", "a duplicate report covers one site"))?;
    let site = site_in_scope(&state, &current, site_id).await?;
    let groups = duplicate_groups(pool, site.id).await?;
    let body = build_report(pool, vec![site.id], false, groups, &[], expanded(&query)).await?;
    Ok(Json(body))
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

/// Build the response body, expanding each group when asked.
///
/// `expand` costs one query per group rather than one big one: a library with four hundred groups
/// that nobody expands pays nothing, and one that is expanded pays four hundred cheap indexed
/// reads rather than a single statement whose result set nobody is going to scroll.
async fn build_report(
    pool: &sqlx::PgPool,
    site_ids: Vec<Uuid>,
    cross_site: bool,
    groups: Vec<DuplicateGroup>,
    labels: &[omnion_media::SiteLabel],
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
            site_name: labels
                .iter()
                .find(|label| label.id == site_id)
                .map(|label| label.name.clone()),
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
        cross_site,
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
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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
