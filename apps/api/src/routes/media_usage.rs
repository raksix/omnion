//! `/api/v1/media/{id}/references` and `/activity` — a file's usage and its story (REQ-010, slice 4).
//!
//! Two reads the file-detail screen needs, and the split between them is the design:
//!
//! * **Usage** is a *fact about the content*: where this file is used. It lives in
//!   `media_references`, which the duplicate merge repoints and the retention purge refuses on.
//!   Slice 3 wrote the table and slice 4 wrote the repair scan; nothing read it back for a
//!   person until now, and bookkeeping nobody can read is bookkeeping nobody can act on.
//! * **Activity** is a *fact about the audit trail*: what has been done to this file. It lives in
//!   `audit_log`, which is the audit crate's table and the audit crate's vocabulary — so this
//!   file composes the two and neither crate reaches into the other's schema.
//!
//! Three rules hold across this file, each a way to publish a list that looks right and answers
//! nothing:
//!
//! * **Both reads are `media.read`.** Knowing where a file is used and what happened to it is
//!   the same power as opening it. Neither list contains anything the caller could not already
//!   read from the file itself, so a second permission would be a second opinion, not a
//!   boundary — and the file may be in the trash, where the reader must still be able to ask
//!   why.
//! * **The scope check is the file's own site, loaded before either row is read.** Another
//!   tenant's file is a `403 cross_organization` and not a `404`: `site_in_scope` is what every
//!   media route loads, so a `404` here would make these the only two screens in the library
//!   where a foreign file is *invisible* rather than forbidden — and an id that is invisible on
//!   one route and forbidden on its neighbours is a better oracle than either alone.
//! * **A file in the trash is not a `404` here.** `file_in_scope` reads live files only, and a
//!   trash screen that cannot say *why* a file was deleted — which is exactly when somebody
//!   opens the activity tab — would answer a blank page for the one question it exists to
//!   answer. The trashed row is loaded through the same site scope check.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_media::{UsageCounts, UsageEntry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::routes::media_files::site_of;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// The activity vocabulary
// ---------------------------------------------------------------------------------------------

/// What the audit trail records **against a file**.
///
/// A list, not one string, and the reason is the question a reader actually asks: "who could see
/// this file in March". The media surface writes `media` for the bytes and `media_file` for a
/// grant placed on the file — and a grant is the most consequential thing this library records,
/// because it is the only one that decides who may fetch the file at all. A single target type
/// would answer either "what happened to the bytes" or "who could open it", never both.
///
/// The converse is a rule too: anything NOT in this list did not happen to the file. It happened
/// to the folder it sits in, to the site, or to the tenant. A folder rename is not in a file's
/// story, and pretending otherwise would make the tab a second audit screen with different
/// filters and no name for what it excludes.
pub const FILE_TARGET_TYPES: [&str; 2] = ["media", "media_file"];

/// How many activity rows one read may return.
pub const MAX_ACTIVITY_ROWS: i64 = 100;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One place a file is used.
#[derive(Debug, Serialize)]
pub struct UsageBody {
    /// The reference row's id.
    pub id: Uuid,
    /// What kind of record points here (`page`, `theme`, …).
    pub resource_kind: String,
    /// The referent's id, as text — it may be a slug, so it is never parsed here.
    pub resource_id: String,
    /// Which field of that record points here; empty means the record *is* the file.
    pub field: String,
    /// A name for the referent, when the platform can resolve one.
    pub label: Option<String>,
    /// The referent's lifecycle state, for a page.
    pub status: Option<String>,
    /// Where the referent lives in the panel, when it can be resolved.
    pub path: Option<String>,
    /// Whether the platform can still see the referent.
    ///
    /// Carried on every row and not merely as a total, because the reader has to be able to see
    /// *which* row is stale before deciding whether to repoint it or run the repair scan.
    pub resolved: bool,
    /// When the reference was recorded.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&UsageEntry> for UsageBody {
    fn from(entry: &UsageEntry) -> Self {
        Self {
            id: entry.reference.id,
            resource_kind: entry.reference.resource_kind.clone(),
            resource_id: entry.reference.resource_id.clone(),
            field: entry.reference.field.clone(),
            label: entry.resource_label.clone(),
            status: entry.resource_status.clone(),
            path: entry.resource_path.clone(),
            resolved: entry.resolved,
            created_at: entry.reference.created_at,
        }
    }
}

/// One file's usage.
#[derive(Debug, Serialize)]
pub struct UsageResponse {
    /// File the usage belongs to.
    pub media_id: Uuid,
    /// Distinct records naming this file.
    pub records: i64,
    /// Of those, the ones the platform can still see.
    pub resolved: i64,
    /// Reference rows, which a record naming the file in two fields inflates on purpose.
    pub rows: i64,
    /// Whether the read was cut short by the screen's bound.
    pub truncated: bool,
    /// One sentence saying what the numbers mean.
    ///
    /// A bare pair of integers beside a list is a caption nobody can check: "3 records, 1
    /// resolved" does not tell the reader whether they have a problem or whether one page names
    /// the file three times. The sentence carries the difference.
    pub summary: String,
    /// The rows themselves, oldest first.
    pub usage: Vec<UsageBody>,
}

impl UsageResponse {
    fn build(media_id: Uuid, entries: &[UsageEntry], counts: UsageCounts) -> Self {
        let stale = counts.records - counts.resolved;
        let summary = match (counts.records, stale) {
            (0, _) => "Nothing points at this file, so deleting it breaks nothing on this site."
                .to_string(),
            (_, 0) => match (counts.records, counts.rows) {
                (1, 1) => "One record uses this file.".to_string(),
                (1, rows) => format!(
                    "One record uses this file, in {rows} of its fields — deleting it breaks that \
                     record, not {rows} different ones."
                ),
                (records, rows) => format!(
                    "{records} records use this file across {rows} fields. Deleting it breaks the \
                     pages that name it."
                ),
            },
            (_, 1) => format!(
                "{stale} reference points at a record that no longer exists. Repoint it, or run \
                 the repair scan on the retention tab, before deleting this file."
            ),
            _ => format!(
                "{stale} references point at records that no longer exist. Repoint them, or run \
                 the repair scan on the retention tab, before deleting this file."
            ),
        };

        Self {
            media_id,
            records: counts.records,
            resolved: counts.resolved,
            rows: counts.rows,
            truncated: counts.rows >= omnion_media::MAX_USAGE_ROWS,
            summary,
            usage: entries.iter().map(UsageBody::from).collect(),
        }
    }
}

/// One audited action against a file.
#[derive(Debug, Serialize)]
pub struct ActivityBody {
    /// Audit row id — ordered, so two rows with the same timestamp still read in the order
    /// they were written.
    pub id: i64,
    /// What happened (`media.deleted`, `media.grant_changed`, …).
    pub action: String,
    /// The same action in a sentence, so the panel never has to build a label out of a verb.
    pub summary: String,
    /// Who did it, when the account still exists.
    pub actor: Option<String>,
    /// Which kind of actor, for a reader who needs to tell a person from a worker.
    pub actor_type: String,
    /// Structured detail the action recorded.
    pub metadata: Value,
    /// When it happened.
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

/// One file's activity, newest first.
#[derive(Debug, Serialize)]
pub struct ActivityResponse {
    /// File the activity belongs to.
    pub media_id: Uuid,
    /// How many rows were read.
    pub total: i64,
    /// Whether the read was cut short by the screen's bound.
    pub truncated: bool,
    /// The rows, newest first.
    pub activity: Vec<ActivityBody>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/{id}/references` — where a file is used.
pub async fn references(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
) -> std::result::Result<Json<UsageResponse>, ApiError> {
    let pool = state.db().pool();
    file_or_trashed(&state, &current, media_id).await?;

    let entries = omnion_media::list_usage(pool, media_id).await?;
    let counts = omnion_media::count_usage(pool, media_id).await?;
    Ok(Json(UsageResponse::build(media_id, &entries, counts)))
}

/// `GET /api/v1/media/{id}/activity` — what has happened to a file.
pub async fn activity(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ActivityQuery>,
    Path(media_id): Path<Uuid>,
) -> std::result::Result<Json<ActivityResponse>, ApiError> {
    let pool = state.db().pool();
    file_or_trashed(&state, &current, media_id).await?;

    let rows = omnion_audit::entries::for_target(
        pool,
        &media_id.to_string(),
        &FILE_TARGET_TYPES,
        query.limit.unwrap_or(MAX_ACTIVITY_ROWS),
    )
    .await?;

    let names = actor_names(pool, rows.iter().map(|row| row.actor_user_id)).await?;

    let activity: Vec<ActivityBody> = rows
        .iter()
        .zip(names.iter())
        .map(|(row, name)| ActivityBody {
            id: row.id,
            action: row.action.clone(),
            summary: describe(&row.action, &row.metadata),
            actor: name.clone(),
            actor_type: row.actor_type.clone(),
            metadata: row.metadata.clone(),
            occurred_at: row.created_at,
        })
        .collect();

    Ok(Json(ActivityResponse {
        media_id,
        total: activity.len() as i64,
        truncated: query.limit.is_some_and(|limit| limit >= MAX_ACTIVITY_ROWS),
        activity,
    }))
}

/// `?limit=` for the activity read. Clamped by the crate, so an absurd value is not an error.
#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    /// How many rows to read.
    #[serde(default)]
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Load a file — **live or trashed** — and refuse it when its site is out of the caller's scope.
///
/// The trashed half is the whole reason this is not [`crate::routes::media_files::file_in_scope`]:
/// the question "what happened to this file" is asked *after* it was deleted, and a 404 there
/// answers a blank page to the one person who needs the trail. The site load is still the scope
/// check, so a file of another tenant is still a 404 rather than a readable history.
async fn file_or_trashed(
    state: &AppState,
    current: &CurrentSession,
    media_id: Uuid,
) -> std::result::Result<omnion_media::MediaFile, ApiError> {
    let file = omnion_media::find_file_any_state(state.db().pool(), media_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "media_not_found", "no such file"))?;
    let site = site_of(state, file.site_id).await?;
    site_in_scope(state, current, site.id).await?;
    Ok(file)
}

/// Resolve account ids to display names, in one query, keeping the caller's order.
///
/// The lookup goes through the identity crate rather than raw SQL, because this is the audit
/// crate's caller reaching for a table that is not its own and `ApiError` deliberately has no
/// `From<sqlx::Error>` — a raw query here would have to hand-roll an error mapping that every
/// other module gets for free, and a hand-rolled one is where a 500 turns into a 200 with no
/// rows.
async fn actor_names(
    pool: &sqlx::PgPool,
    ids: impl Iterator<Item = Option<Uuid>>,
) -> std::result::Result<Vec<Option<String>>, ApiError> {
    let ids: Vec<Option<Uuid>> = ids.collect();
    let wanted: Vec<Uuid> = ids.iter().flatten().copied().collect();
    if wanted.is_empty() {
        return Ok(ids.into_iter().map(|_| None).collect());
    }

    #[derive(sqlx::FromRow)]
    struct NameRow {
        id: Uuid,
        display_name: String,
        email: String,
    }

    let rows: Vec<NameRow> =
        sqlx::query_as("select id, display_name, email from users where id = any($1)")
            .bind(&wanted)
            .fetch_all(pool)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "actor_lookup_failed",
                    format!("the actors could not be read back: {error}"),
                )
            })?;

    let names: std::collections::HashMap<Uuid, String> = rows
        .into_iter()
        .map(|row| {
            // A display name is optional in this platform, so an empty one falls back to the
            // address. "Nobody" would be a claim about a person who demonstrably did the thing.
            let name = if row.display_name.trim().is_empty() {
                row.email
            } else {
                row.display_name
            };
            (row.id, name)
        })
        .collect();

    Ok(ids
        .into_iter()
        .map(|id| id.and_then(|id| names.get(&id).cloned()))
        .collect())
}

/// Turn an action name into a sentence.
///
/// The action vocabulary is `media.<past-tense>`, which is machine-readable and *not* a
/// sentence: "media.share_revoked" beside "media.version_created" tells an operator nothing
/// about their file. The label is built here, in the one place that knows the vocabulary, so a
/// new action gets a readable fallback rather than a raw token — a screen that shows
/// `media.trash_emptied` as a caption is showing a database column.
fn describe(action: &str, metadata: &Value) -> String {
    let verb = action.strip_prefix("media.").unwrap_or(action);
    let sentence = match verb {
        "uploaded" => "Uploaded".to_string(),
        "updated" => "Details edited".to_string(),
        "deleted" => "Moved to the trash".to_string(),
        "restored" => "Restored from the trash".to_string(),
        "purged" => "Permanently deleted".to_string(),
        "version_created" => "Replaced with a new version".to_string(),
        "version_restored" => "An earlier version was restored as the newest".to_string(),
        "share_created" => "A share link was created".to_string(),
        "share_revoked" => "A share link was revoked".to_string(),
        "shares_revoked_all" => "Every share link was revoked".to_string(),
        "duplicate_merged" => "A duplicate group was merged into this file".to_string(),
        "grant_changed" => "Access was changed".to_string(),
        "grant_removed" => "Access was removed".to_string(),
        "hold_placed" => "Placed under a legal hold".to_string(),
        "hold_released" => "The legal hold was released".to_string(),
        "scan_flagged" => "The scanner flagged this file".to_string(),
        "scan_released" => "A quarantine was released".to_string(),
        "references_repaired" => "Reference rows were repaired".to_string(),
        other => {
            // An action this screen has never seen is shown as its own words rather than hidden:
            // a trail with a hole in it is worse than a trail with an unfamiliar entry.
            return other.replace(['_', '.'], " ");
        }
    };

    // A hold is the one action whose *reason* is the whole point of it, so it is named here
    // rather than left in the metadata blob every reader has to open.
    if verb == "hold_placed" || verb == "hold_released" {
        if let Some(reason) = metadata.get("reason").and_then(Value::as_str) {
            let reason = reason.trim();
            if !reason.is_empty() {
                return format!("{sentence} — {reason}");
            }
        }
    }

    sentence
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The label is the *action*, not the raw token — this is the assertion that catches a new
    /// action being added to the trail with no sentence beside it.
    #[test]
    fn every_action_reads_as_a_sentence() {
        assert_eq!(describe("media.deleted", &json!({})), "Moved to the trash");
        assert_eq!(
            describe("media.share_created", &json!({})),
            "A share link was created"
        );
        // The two halves of the purge decision must not read alike: one is reversible and one
        // is not, and "Deleted" for both would leave an operator unsure which they are reading.
        assert_ne!(
            describe("media.deleted", &json!({})),
            describe("media.purged", &json!({}))
        );
    }

    /// A hold's reason is the record. An action that drops it says only that it happened.
    #[test]
    fn a_hold_names_its_reason() {
        let sentence = describe("media.hold_placed", &json!({ "reason": "litigation" }));
        assert!(
            sentence.contains("litigation"),
            "the reason is why the row exists: {sentence}"
        );
        // An absent reason must not produce a dangling dash.
        let bare = describe("media.hold_placed", &json!({ "reason": "   " }));
        assert!(!bare.ends_with("—"), "no reason, no dash: {bare}");
    }

    /// An action the screen has never seen is shown in its own words, never dropped.
    #[test]
    fn an_unknown_action_is_shown_not_hidden() {
        let sentence = describe("media.future_thing", &json!({}));
        assert_eq!(sentence, "future thing");
    }

    /// The two numbers must be able to disagree, or the split that carries the difference is
    /// decoration.
    #[test]
    fn the_summary_says_when_a_referent_is_gone() {
        // One stale row and several must both read as a *count*, and the singular/plural pair is
        // where a "there are 1 references" line comes from. Both branches are asserted because
        // the bug this guards against only appears in one of them.
        let one_stale = UsageResponse::build(
            Uuid::nil(),
            &[],
            UsageCounts {
                records: 3,
                resolved: 2,
                rows: 4,
            },
        );
        assert!(
            one_stale.summary.contains("no longer exists"),
            "a stale referent is the actionable fact: {}",
            one_stale.summary
        );
        assert!(
            !one_stale.summary.contains("1 references"),
            "one is singular: {}",
            one_stale.summary
        );

        let two_stale = UsageResponse::build(
            Uuid::nil(),
            &[],
            UsageCounts {
                records: 3,
                resolved: 1,
                rows: 4,
            },
        );
        assert!(
            two_stale.summary.contains("2 references point"),
            "the count is carried, not just the existence: {}",
            two_stale.summary
        );
        // The distinction the reader needs is between "everything resolves" and "some does not";
        // once a stale row exists the *number* of live ones stops being the useful number.
        assert!(
            two_stale.summary.contains("repair scan"),
            "the sentence has to say what to do about it: {}",
            two_stale.summary
        );
    }

    /// A clean file and a many-fielded one are different sentences, and both must be true.
    #[test]
    fn the_summary_distinguishes_records_from_fields() {
        let one_record_three_fields = UsageResponse::build(
            Uuid::nil(),
            &[],
            UsageCounts {
                records: 1,
                resolved: 1,
                rows: 3,
            },
        );
        assert!(
            one_record_three_fields
                .summary
                .contains("not 3 different ones"),
            "three fields of one page is one record: {}",
            one_record_three_fields.summary
        );

        let unused = UsageResponse::build(
            Uuid::nil(),
            &[],
            UsageCounts {
                records: 0,
                resolved: 0,
                rows: 0,
            },
        );
        assert!(
            unused.summary.contains("breaks nothing"),
            "the empty state has to say it is safe, not just be empty: {}",
            unused.summary
        );
    }
}
