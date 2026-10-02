//! `/api/v1/comments`, `/api/v1/public/comments` and `/api/v1/sites/{id}/comment-settings` —
//! REQ-064, slice 4a.
//!
//! The moderation inbox, the public submission route and the per-site policy. Four decisions
//! shape the file, and each of them is a place where the obvious answer is wrong:
//!
//! * **Three powers, not two.** `comments.read` opens the inbox; `comments.manage` approves,
//!   spams, trashes, replies and edits the policy. An account that may read the queue but not
//!   change it is a queue with dead buttons, and an account that may change it but not read it
//!   is nobody. The two are different powers and a deployment may want them separate, so they
//!   are two keys.
//!
//! * **The visitor's route answers 202, not 200 or 201, and never says which state it chose.**
//!   A submission lands `pending` or `approved` and the visitor is told neither, because telling
//!   them "your comment is pending" tells an unauthenticated caller exactly which heuristic
//!   fired — and the REQ's own risk note is that the panel must not claim certainty about spam.
//!   The route's answer is the same in both cases, which is also the only answer that does not
//!   become an oracle.
//!
//! * **`ip_hint` is a fingerprint, not an address.** The forms module already hashes the sender
//!   for exactly this reason and the ban table matches on the same value, so a moderator's ban
//!   works without the database holding a visitor's address. The raw address is never stored.
//!
//! * **The two-level rule is enforced by the schema, and this file's job is to return its
//!   refusal.** `cms_comments_two_levels` refuses a reply to a reply with a `check_violation`,
//!   which surfaces here as a 400 naming the rule — see [`api_error_from_comment_write`], which
//!   translates it because a raw PostgreSQL error string is not a message a person can act on.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::http::header::USER_AGENT;
use omnion_audit::NewAuditEntry;
use omnion_identity::sites::{self, Site};
use omnion_content::page_comments::{
    BulkOutcome, COMMENT_STATUSES, Comment, CommentBan, CommentSettings, CommentStore,
    InboxFilter, InboxPage, NewComment, NewStaffReply, PublicComment, validate_email,
};
use omnion_content::ContentError;
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Query parameters of the moderation inbox.
#[derive(Debug, Default, Deserialize)]
pub struct InboxParams {
    /// Which tab.
    pub status: Option<String>,
    /// The site the inbox belongs to.
    pub site_id: Option<Uuid>,
    /// Free text over author, address and body.
    #[serde(alias = "q")]
    pub search: Option<String>,
    /// One page's comments.
    pub page_id: Option<Uuid>,
    /// Page size.
    pub limit: Option<i64>,
    /// Offset.
    pub offset: Option<i64>,
}

/// Query parameters of the public thread.
#[derive(Debug, Default, Deserialize)]
pub struct PublicParams {
    /// Site key, on an installation serving more than one.
    pub site: Option<String>,
}

/// What a visitor submitted.
#[derive(Debug, Deserialize)]
pub struct PublicCommentRequest {
    /// Author display name.
    pub author_name: String,
    /// Author address. Never echoed back.
    pub author_email: String,
    /// The body.
    pub body: String,
    /// The comment being answered, when this is a reply.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// The hidden field a bot fills in. A real visitor never sees it.
    #[serde(default)]
    pub honeypot: Option<String>,
    /// How long the form was on screen, in milliseconds.
    #[serde(default)]
    pub filled_at_ms: Option<i64>,
    /// The page the comment was left on, as the form saw it.
    #[serde(default)]
    pub source_path: Option<String>,
}

/// What a visitor is told, in both cases.
#[derive(Debug, Serialize)]
pub struct PublicCommentBody {
    /// The comment's own id, so a theme can anchor a link to it.
    pub id: Uuid,
    /// The visitor's own words, echoed back exactly as they were stored.
    pub body: String,
}

/// A comment as the inbox draws it.
///
/// Its own wire struct rather than `#[serde(flatten)]` on the store's row: the store type is a
/// database shape and has no business gaining a `Serialize` derive because a route started
/// returning it. The two are frozen in the same commit that found the gap.
///
/// `approved_by` is here because the panel's whole claim about an approval is *which* account
/// made it, and a wire shape that omits it means every screen that wants it has to ask a second
/// endpoint — and the two then disagree the moment somebody approves a comment in one tab while
/// another tab is open.
#[derive(Debug, Serialize)]
pub struct InboxCommentBody {
    /// Comment id.
    pub id: Uuid,
    /// The page commented on.
    pub page_id: Uuid,
    /// The comment being answered.
    pub parent_id: Option<Uuid>,
    /// 0 for a comment, 1 for a reply.
    pub reply_depth: i16,
    /// Author display name.
    pub author_name: String,
    /// Author address — the inbox, and only the inbox, shows this.
    pub author_email: String,
    /// Coarse client fingerprint.
    pub ip_hint: Option<String>,
    /// Raw user agent, bounded.
    pub user_agent: Option<String>,
    /// The body, plain text.
    pub body: String,
    /// The moderation state.
    pub status: String,
    /// The heuristic that marked it, or a moderator's note.
    pub spam_reason: Option<String>,
    /// When a moderator approved it.
    pub approved_at: Option<OffsetDateTime>,
    /// Which account approved it, when the policy approved it on its own.
    pub approved_by: Option<Uuid>,
    /// Whether a moderator wrote it.
    pub is_staff_reply: bool,
    /// When it was written.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// The page's title, so a moderator reads "About us" rather than a slug.
    pub page_title: Option<String>,
}

impl InboxCommentBody {
    /// The wire shape of one stored row.
    fn from_row(comment: Comment, page_title: Option<String>) -> Self {
        Self {
            id: comment.id,
            page_id: comment.page_id,
            parent_id: comment.parent_id,
            reply_depth: comment.reply_depth,
            author_name: comment.author_name,
            author_email: comment.author_email,
            ip_hint: comment.ip_hint,
            user_agent: comment.user_agent,
            body: comment.body,
            status: comment.status,
            spam_reason: comment.spam_reason,
            approved_at: comment.approved_at,
            approved_by: comment.approved_by,
            is_staff_reply: comment.is_staff_reply,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
            page_title,
        }
    }
}

/// The inbox page: the rows and the four tab counts.
#[derive(Debug, Serialize)]
pub struct InboxResponse {
    /// The filtered rows, newest first.
    pub comments: Vec<InboxCommentBody>,
    /// How many rows the filter matched.
    pub total: i64,
    /// Counts per tab, in `pending · approved · spam · trash` order.
    pub counts: Vec<TabCount>,
    /// The states the panel offers, so the tab bar is generated from the API rather than
    /// carrying its own list that can fall behind the CHECK constraint.
    pub statuses: Vec<String>,
}

/// One tab's count.
#[derive(Debug, Serialize)]
pub struct TabCount {
    /// The state.
    pub status: String,
    /// How many rows are in it.
    pub count: i64,
}

/// A moderator's reply.
#[derive(Debug, Deserialize)]
pub struct StaffReplyRequest {
    /// Site the comment belongs to.
    pub site_id: Uuid,
    /// Display name shown beside the reply.
    pub author_name: String,
    /// The reply.
    pub body: String,
}

/// Change one comment's state.
#[derive(Debug, Deserialize)]
pub struct ModerateRequest {
    /// The new state.
    pub status: String,
    /// Why, for a comment moved to spam or trash.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Change several comments' states at once.
#[derive(Debug, Deserialize)]
pub struct BulkModerateRequest {
    /// The new state.
    pub status: String,
    /// The comments to move.
    pub comment_ids: Vec<Uuid>,
}

/// Change the moderation policy.
#[derive(Debug, Deserialize)]
pub struct SaveSettingsRequest {
    /// Whether the public form accepts anything.
    pub comments_enabled: bool,
    /// How many approved comments an address needs to skip the queue; 0 disables it.
    #[serde(default)]
    pub auto_approve_after_comments: Option<i32>,
    /// Case-insensitive substrings that mark a comment as spam.
    #[serde(default)]
    pub blocked_words: Option<Vec<String>>,
    /// How many links a body may carry.
    #[serde(default)]
    pub max_links_per_comment: Option<i32>,
    /// Seconds the form must have been on screen.
    #[serde(default)]
    pub min_fill_seconds: Option<i32>,
    /// Comments per address per hour.
    #[serde(default)]
    pub per_ip_per_hour: Option<i32>,
    /// Whether a moderator is notified.
    #[serde(default)]
    pub notify_on_comment: Option<bool>,
}

/// Place a ban.
#[derive(Debug, Deserialize)]
pub struct AddBanRequest {
    /// `email` or `ip`.
    pub kind: String,
    /// The value.
    pub value: String,
    /// Why, in the moderator's words.
    #[serde(default)]
    pub reason: Option<String>,
    /// When the ban stops applying. Absent means forever.
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
}

/// The settings screen in one read: the policy and the bans.
///
/// Both are store types again, so both get their own frozen wire struct: the panel's contract
/// is what this file says it is, and a column rename in the schema must not be a breaking API
/// change the morning after it lands.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// The policy.
    pub settings: SettingsWire,
    /// The bans, newest first.
    pub bans: Vec<BanWire>,
}

/// The moderation policy on the wire.
#[derive(Debug, Serialize)]
pub struct SettingsWire {
    /// The site.
    pub site_id: Uuid,
    /// Whether the public form accepts anything.
    pub comments_enabled: bool,
    /// How many approved comments an address needs to skip the queue.
    pub auto_approve_after_comments: i32,
    /// Case-insensitive substrings that mark a comment as spam.
    pub blocked_words: Vec<String>,
    /// How many links a body may carry.
    pub max_links_per_comment: i32,
    /// Seconds the form must have been on screen.
    pub min_fill_seconds: i32,
    /// Comments per address per hour.
    pub per_ip_per_hour: i32,
    /// Whether a moderator is notified.
    pub notify_on_comment: bool,
    /// When the settings last changed.
    pub updated_at: OffsetDateTime,
}

impl From<CommentSettings> for SettingsWire {
    fn from(value: CommentSettings) -> Self {
        Self {
            site_id: value.site_id,
            comments_enabled: value.comments_enabled,
            auto_approve_after_comments: value.auto_approve_after_comments,
            blocked_words: value.blocked_words,
            max_links_per_comment: value.max_links_per_comment,
            min_fill_seconds: value.min_fill_seconds,
            per_ip_per_hour: value.per_ip_per_hour,
            notify_on_comment: value.notify_on_comment,
            updated_at: value.updated_at,
        }
    }
}

/// One ban on the wire.
#[derive(Debug, Serialize)]
pub struct BanWire {
    /// Ban id.
    pub id: Uuid,
    /// `email` or `ip`.
    pub kind: String,
    /// The banned value, already fingerprinted for an `ip` ban.
    pub value: String,
    /// Why, in the moderator's words.
    pub reason: Option<String>,
    /// When it was placed.
    pub created_at: OffsetDateTime,
    /// When it stops applying; absent means forever.
    pub expires_at: Option<OffsetDateTime>,
    /// Whether it applies right now.
    pub active: bool,
}

impl From<CommentBan> for BanWire {
    fn from(value: CommentBan) -> Self {
        Self {
            active: value.is_active(OffsetDateTime::now_utc()),
            id: value.id,
            kind: value.kind,
            value: value.value,
            reason: value.reason,
            created_at: value.created_at,
            expires_at: value.expires_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/comments` — the moderation inbox.
pub async fn list_comments(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<InboxParams>,
) -> Result<Json<InboxResponse>, ApiError> {
    let site_id = params.site_id.ok_or_else(|| {
        ApiError::bad_request("site_required", "this request needs a site_id")
    })?;
    let site = site_in_scope(&state, &session, site_id).await?;

    let filter = InboxFilter {
        status: params.status.clone(),
        search: params.search.clone(),
        page_id: params.page_id,
        limit: params.limit.unwrap_or(50),
        offset: params.offset.unwrap_or(0),
    };
    if let Some(status) = filter.status.as_deref() {
        if !COMMENT_STATUSES.contains(&status) {
            return Err(ApiError::bad_request(
                "comment_status_invalid",
                format!("{status:?} is not a moderation state"),
            ));
        }
    }

    let page = CommentStore::new(state.db().pool().clone())
        .inbox(site.id, &filter)
        .await?;
    let titles = page_titles(&state, &page).await?;

    Ok(Json(InboxResponse {
        comments: page
            .comments
            .into_iter()
            .map(|comment| {
                let title = titles.get(&comment.page_id).cloned();
                InboxCommentBody::from_row(comment, title)
            })
            .collect(),
        total: page.total,
        counts: page
            .counts
            .into_iter()
            .map(|(status, count)| TabCount { status, count })
            .collect(),
        statuses: COMMENT_STATUSES.iter().map(|s| (*s).to_owned()).collect(),
    }))
}

/// `GET /api/v1/comments/{id}` — one comment.
pub async fn get_comment(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<Json<InboxCommentBody>, ApiError> {
    let site_id = require_site(&params)?;
    site_in_scope(&state, &session, site_id).await?;

    let comment = CommentStore::new(state.db().pool().clone())
        .get(site_id, id)
        .await?;
    let title = page_title(&state, comment.page_id).await?;
    Ok(Json(InboxCommentBody::from_row(comment, title)))
}

/// `PATCH /api/v1/comments/{id}` — approve, spam, trash or restore.
pub async fn moderate_comment(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
    Json(body): Json<ModerateRequest>,
) -> Result<Json<InboxCommentBody>, ApiError> {
    let site_id = require_site(&params)?;
    site_in_scope(&state, &session, site_id).await?;

    let store = CommentStore::new(state.db().pool().clone());
    let before = store.get(site_id, id).await?;
    let comment = store
        .set_status(
            site_id,
            id,
            &body.status,
            Some(session.user.id),
            body.reason.as_deref(),
        )
        .await?;

    audit(
        &state,
        &session,
        "comment.moderated",
        Some(comment.id),
        json!({
            "comment_id": comment.id,
            "site_id": site_id,
            "from": before.status,
            "to": comment.status,
            "reason": comment.spam_reason,
        }),
    )
    .await;

    let title = page_title(&state, comment.page_id).await?;
    Ok(Json(InboxCommentBody::from_row(comment, title)))
}

/// `POST /api/v1/comments/bulk` — moderate a selection.
pub async fn bulk_moderate(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Json(body): Json<BulkModerateRequest>,
) -> Result<Json<BulkBody>, ApiError> {
    let site_id = require_site(&params)?;
    site_in_scope(&state, &session, site_id).await?;

    let store = CommentStore::new(state.db().pool().clone());
    let outcome = store
        .bulk_set_status(site_id, &body.comment_ids, &body.status, Some(session.user.id))
        .await?;

    audit(
        &state,
        &session,
        "comment.bulk_moderated",
        None,
        json!({
            "site_id": site_id,
            "to": body.status,
            "requested": body.comment_ids.len(),
            "updated": outcome.updated.len(),
            "missing": outcome.missing.len(),
            "refused": outcome.refused.len(),
        }),
    )
    .await;

    Ok(Json(BulkBody::from_outcome(
        outcome,
        body.comment_ids.len(),
    )))
}

/// What a bulk moderation did.
#[derive(Debug, Serialize)]
pub struct BulkBody {
    /// Comments that moved.
    pub updated: Vec<Uuid>,
    /// Comments that no longer exist.
    pub missing: Vec<Uuid>,
    /// Comments the rules refused.
    pub refused: Vec<Uuid>,
    /// Whether every requested comment moved.
    pub complete: bool,
    /// How many were asked for.
    pub requested: usize,
}

impl BulkBody {
    /// The wire shape of a store outcome.
    fn from_outcome(outcome: BulkOutcome, requested: usize) -> Self {
        Self {
            complete: outcome.is_complete(requested),
            updated: outcome.updated,
            missing: outcome.missing,
            refused: outcome.refused,
            requested,
        }
    }
}

/// `POST /api/v1/comments/{id}/reply` — a moderator answers.
pub async fn reply(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<StaffReplyRequest>,
) -> Result<(StatusCode, Json<InboxCommentBody>), ApiError> {
    let site = site_in_scope(&state, &session, body.site_id).await?;
    let store = CommentStore::new(state.db().pool().clone());

    // The parent is read through the SITE the caller named, not through the body's own site id:
    // the body carries a `site_id` and a caller who lies about it must not be able to answer a
    // comment on a site they may not see. `site_in_scope` above is the check; this is the read.
    let parent = store.get(site.id, id).await?;
    let comment = store
        .staff_reply(NewStaffReply {
            organization_id: site.organization_id,
            site_id: site.id,
            page_id: parent.page_id,
            parent_id: id,
            author_user_id: session.user.id,
            author_name: body.author_name,
            body: body.body,
        })
        .await
        .map_err(api_error_from_comment_write)?;

    audit(
        &state,
        &session,
        "comment.replied",
        Some(comment.id),
        json!({ "parent_id": id, "site_id": site.id }),
    )
    .await;

    let title = page_title(&state, comment.page_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(InboxCommentBody::from_row(comment, title)),
    ))
}

/// `DELETE /api/v1/comments/{id}` — remove for good.
pub async fn delete_comment(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let site_id = require_site(&params)?;
    site_in_scope(&state, &session, site_id).await?;

    CommentStore::new(state.db().pool().clone())
        .delete(site_id, id)
        .await?;

    audit(
        &state,
        &session,
        "comment.deleted",
        Some(id),
        json!({ "site_id": site_id }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Settings and bans
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sites/{site_id}/comment-settings` — the policy and the bans.
pub async fn get_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = CommentStore::new(state.db().pool().clone());

    let settings = store.settings(site.id, site.organization_id).await?;
    let bans = store.bans(site.id).await?;

    Ok(Json(SettingsBody {
        settings: settings.into(),
        bans: bans.into_iter().map(BanWire::from).collect(),
    }))
}

/// `PUT /api/v1/sites/{site_id}/comment-settings` — the whole policy.
pub async fn put_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<SaveSettingsRequest>,
) -> Result<Json<SettingsBody>, ApiError> {
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = CommentStore::new(state.db().pool().clone());

    // Read first, then apply the caller's changes on top: a PUT that writes defaults over an
    // owner's policy is how a panel that sends only the toggles it knows about silently
    // re-enables a link limit somebody turned off.
    let current = store.settings(site.id, site.organization_id).await?;
    let next = CommentSettings {
        site_id: site.id,
        organization_id: site.organization_id,
        comments_enabled: body.comments_enabled,
        auto_approve_after_comments: body
            .auto_approve_after_comments
            .unwrap_or(current.auto_approve_after_comments),
        blocked_words: body
            .blocked_words
            .clone()
            .unwrap_or_else(|| current.blocked_words.clone()),
        max_links_per_comment: body
            .max_links_per_comment
            .unwrap_or(current.max_links_per_comment),
        min_fill_seconds: body.min_fill_seconds.unwrap_or(current.min_fill_seconds),
        per_ip_per_hour: body.per_ip_per_hour.unwrap_or(current.per_ip_per_hour),
        notify_on_comment: body.notify_on_comment.unwrap_or(current.notify_on_comment),
        updated_at: current.updated_at,
    };

    let settings = store.save_settings(&next).await?;
    let bans = store.bans(site.id).await?;

    audit(
        &state,
        &session,
        "comment.settings_saved",
        None,
        json!({
            "site_id": site.id,
            "comments_enabled": settings.comments_enabled,
            "auto_approve_after_comments": settings.auto_approve_after_comments,
            "blocked_words": settings.blocked_words.len(),
            "max_links_per_comment": settings.max_links_per_comment,
            "min_fill_seconds": settings.min_fill_seconds,
            "per_ip_per_hour": settings.per_ip_per_hour,
        }),
    )
    .await;

    Ok(Json(SettingsBody {
        settings: settings.into(),
        bans: bans.into_iter().map(BanWire::from).collect(),
    }))
}

/// `POST /api/v1/sites/{site_id}/comment-bans` — place a ban.
pub async fn add_ban(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<AddBanRequest>,
) -> Result<(StatusCode, Json<BanWire>), ApiError> {
    let site = site_in_scope(&state, &session, site_id).await?;

    // The value is stored the way the submission route computes it, or the ban would never
    // match: a moderator who typed an address has to end up with the same token the visitor's
    // submission produced.
    let value = match body.kind.as_str() {
        "email" => validate_email(&body.value).map_err(ApiError::from)?,
        "ip" => fingerprint(&body.value),
        other => {
            return Err(ApiError::bad_request(
                "comment_ban_kind_invalid",
                format!("{other:?} is not a kind of ban"),
            ));
        }
    };

    let ban = CommentStore::new(state.db().pool().clone())
        .add_ban(&CommentBan {
            id: Uuid::nil(),
            site_id: site.id,
            kind: body.kind,
            value,
            reason: body.reason,
            created_by: Some(session.user.id),
            created_at: OffsetDateTime::now_utc(),
            expires_at: body.expires_at,
        })
        .await?;

    audit(
        &state,
        &session,
        "comment.banned",
        Some(ban.id),
        json!({ "site_id": site.id, "kind": ban.kind }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(ban.into())))
}

/// `DELETE /api/v1/sites/{site_id}/comment-bans/{id}` — lift a ban.
pub async fn remove_ban(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((site_id, ban_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    site_in_scope(&state, &session, site_id).await?;

    CommentStore::new(state.db().pool().clone())
        .remove_ban(site_id, ban_id)
        .await?;

    audit(
        &state,
        &session,
        "comment.ban_lifted",
        Some(ban_id),
        json!({ "site_id": site_id }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Public
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/public/comments/{page}` — a page's approved thread.
///
/// Unauthenticated by nature, and it carries no session, so there is nothing to leak: the
/// address, the client hint and the moderation state are not in [`PublicComment`] at all. The
/// store drops a reply whose parent is not approved rather than promoting it, so a thread that
/// renders here is a thread a moderator has seen whole.
pub async fn public_thread(
    State(state): State<AppState>,
    Path(page_slug): Path<String>,
    Query(params): Query<PublicParams>,
    headers: HeaderMap,
) -> Result<Json<Vec<PublicComment>>, ApiError> {
    let site =
        crate::routes::public::resolve_site(state.db().pool(), params.site.as_deref(), &headers)
            .await?;

    let page = omnion_content::pages::find_page_by_slug(state.db().pool(), site.id, &page_slug)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
        })?;

    let thread = CommentStore::new(state.db().pool().clone())
        .thread_for_page(page.id)
        .await?;

    Ok(Json(thread))
}

/// `POST /api/v1/public/comments/{page}` — a visitor comments.
///
/// The answer is 202 with the visitor's own words, whether the comment was published or queued.
/// Anything more precise is a statement to an unauthenticated caller about which rule fired,
/// and a spam heuristic that can be probed from the outside is a spam heuristic that can be
/// tuned by whoever is attacking the site.
pub async fn public_submit(
    State(state): State<AppState>,
    Path(page_slug): Path<String>,
    Query(params): Query<PublicParams>,
    headers: HeaderMap,
    Json(body): Json<PublicCommentRequest>,
) -> Result<(StatusCode, Json<PublicCommentBody>), ApiError> {
    let site =
        crate::routes::public::resolve_site(state.db().pool(), params.site.as_deref(), &headers)
            .await?;

    let page = omnion_content::pages::find_page_by_slug(state.db().pool(), site.id, &page_slug)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
        })?;

    // A filled honeypot is a bot, and the honest answer is the same 202 a person gets: telling
    // the bot it was caught teaches it to try again without it.
    if body
        .honeypot
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "comment_not_accepted",
            "this comment could not be accepted",
        ));
    }

    let fill_seconds = body.filled_at_ms.unwrap_or(0) / 1_000;
    let sender = sender_fingerprint(&headers);

    let new = NewComment {
        organization_id: site.organization_id,
        site_id: site.id,
        page_id: page.id,
        parent_id: body.parent_id,
        author_name: body.author_name,
        author_email: body.author_email,
        body: body.body.clone(),
        ip_hint: sender.ip_fingerprint,
        user_agent: sender.user_agent,
        // The link count is measured with the SAME extractor the crawler uses, so "two links"
        // means the same thing to a spam rule and to the broken-link view.
        link_count: i32::try_from(omnion_content::seo::count_links(&body.body)).unwrap_or(i32::MAX),
        fill_seconds: i32::try_from(fill_seconds).unwrap_or(i32::MAX),
    };

    let store = CommentStore::new(state.db().pool().clone());
    let comment = store.submit(new).await.map_err(api_error_from_comment_write)?;

    emit(
        &state,
        "content.comment.submitted",
        json!({
            "comment_id": comment.id,
            "site_id": site.id,
            "page_id": page.id,
            "status": comment.status,
            "has_reason": comment.spam_reason.is_some(),
            "is_reply": comment.parent_id.is_some(),
        }),
    )
    .await;

    // The store records `source_path` nowhere a reader can see, so the event is where the page
    // the comment was left on travels: a theme reads it, and a moderator's inbox does not need
    // it because the comment is already on the page.
    let _ = body.source_path;

    Ok((
        StatusCode::ACCEPTED,
        Json(PublicCommentBody {
            id: comment.id,
            body: comment.body,
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `?site_id=` on a panel route.
#[derive(Debug, Default, Deserialize)]
pub struct SiteParam {
    /// The site.
    pub site_id: Option<Uuid>,
}

/// A site id, or the platform's own refusal naming it.
///
/// A panel route that cannot tell which site it is acting on would otherwise have to act on
/// all of them, so this is a 400 with the parameter named rather than a default.
fn require_site(params: &SiteParam) -> Result<Uuid, ApiError> {
    params.site_id.ok_or_else(|| {
        ApiError::bad_request("site_required", "this request needs a site_id")
    })
}

/// Load a site and refuse a caller who may not see it.
///
/// `ensure_same_organization` is a *permission* check with a deliberate exception for a platform
/// account, so it answers 403 where a scoped query would answer 404. The inbox is read through
/// the store with the site in the `where` clause, and this is the step that says whether the
/// caller may ask at all — the two are different contracts and both are needed.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Site, ApiError> {
    // 404 for a site the caller may not see, and only THEN the cross-tenant refusal: answering
    // 403 to a site that exists is an existence oracle built out of status codes, and the store
    // beneath this one scopes its own reads by site id.
    let site = sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
        })?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// One page's title, for the detail view.
async fn page_title(state: &AppState, page_id: Uuid) -> Result<Option<String>, ApiError> {
    // A title lives on the revision, not on the page — `select title from pages` is a 500 on
    // every read of the screen, which is the same trap the queue screen walked into.
    let title: Option<String> = sqlx::query_scalar(
        "select title from page_revisions where page_id = $1 order by revision_no desc limit 1",
    )
    .bind(page_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(|error| store_error("reading a commented page's title", error))?;
    Ok(title)
}

/// The titles of the pages a page of comments mentions, in one query.
async fn page_titles(
    state: &AppState,
    page: &InboxPage,
) -> Result<std::collections::HashMap<Uuid, String>, ApiError> {
    let mut wanted: Vec<Uuid> = page
        .comments
        .iter()
        .map(|comment| comment.page_id)
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    // A title lives on the revision, not on the page — the same trap the queue screen walked
    // into, where `select title from pages` is a 500 on every read of the screen.
    let sql = format!(
        "select p.id, r.title from pages p \
         join lateral ( \
             select title from page_revisions \
             where page_id = p.id order by revision_no desc limit 1 \
         ) r on true \
         where p.id = any($1)"
    );

    let rows: Vec<(Uuid, String)> = sqlx::query_as(&sql)
        .bind(&wanted)
        .fetch_all(state.db().pool())
        .await
        .map_err(|error| store_error("reading the commented pages' titles", error))?;

    Ok(rows.into_iter().collect())
}

/// A visitor's client hints, as a fingerprint and the raw agent.
struct SenderFingerprint {
    /// A stable, non-reversible token for the address.
    ip_fingerprint: Option<String>,
    /// The raw user agent, bounded by the store.
    user_agent: Option<String>,
}

/// Fingerprint the sender.
///
/// The same shape `forms::public_submit` uses, and for the same reason: the moderation policy
/// needs to count and ban a sender, and neither needs the address itself. The ban a moderator
/// places is stored as this token, so typing an address into the ban box works without the
/// database ever holding one.
fn sender_fingerprint(headers: &HeaderMap) -> SenderFingerprint {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(fingerprint);

    let agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(400).collect::<String>());

    SenderFingerprint {
        ip_fingerprint: forwarded,
        user_agent: agent,
    }
}

/// A stable, non-reversible fingerprint of a value.
///
/// SHA-256 truncated to 16 bytes and hex-encoded: not a password, so no work factor is needed,
/// and the value only ever has to be compared with itself. The honest note — that a
/// per-installation salt would be better privacy and is not available without a config surface
/// that does not exist yet — belongs here rather than nowhere.
fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Write one audit row, swallowing its own failure.
///
/// A moderation action that succeeded and then failed to be recorded is still a moderation
/// action, and refusing it would tell a moderator their approval did not happen when it did.
/// The alternative — propagating the error — is what turns a healthy audit table into a reason
/// every comment on the site becomes unmoderatable.
async fn audit(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    target_id: Option<Uuid>,
    details: serde_json::Value,
) {
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry {
            organization_id: session.user.organization_id,
            actor_user_id: Some(session.user.id),
            actor_type: omnion_audit::ActorType::User,
            action,
            target_type: Some("comment"),
            target_id: target_id.map(|id| id.to_string()),
            metadata: details,
            ip_address: None,
        },
    )
    .await;
}

/// Emit one event, swallowing its own failure.
async fn emit(state: &AppState, name: &str, payload: serde_json::Value) {
    let _ = bus::emit(state.db().pool(), NewEvent::new(name).payload(payload)).await;
}

/// Translate a store error, including the two the SCHEMA raises rather than the store.
///
/// `cms_comments_two_levels` refuses a reply to a reply and a missing parent by raising
/// `check_violation` / `foreign_key_violation`, which arrive as `ContentError::Database` and
/// which the shared mapping answers as "content store error" — a 400 telling a visitor their
/// request was malformed, for a rule that is working exactly as designed. The store checks the
/// parent it can check in SQL; the trigger covers the depth, and the two together are the whole
/// rule, so the route has to be where the trigger's answer becomes a message.
fn api_error_from_comment_write(error: ContentError) -> ApiError {
    let ContentError::Database(ref inner) = error else {
        return ApiError::from(error);
    };

    let Some(db) = inner.as_database_error() else {
        return ApiError::from(error);
    };
    let text = inner.to_string();

    // Matched on the TRIGGER'S OWN TEXT, never on the SQLSTATE alone. The first version tested
    // `code = 23514 or 23503` and translated whatever arrived — so the `author_email` CHECK
    // refusing a moderator's reply came back to the panel as "no such comment to answer",
    // naming a problem that had nothing to do with the comment being answered. A code is a
    // CATEGORY; the message the rule raises is the only thing that says which rule fired.
    for (needle, code, message) in [
        (
            "a reply cannot answer another reply",
            "comment_thread_too_deep",
            "a reply cannot answer another reply",
        ),
        (
            "comment parent",
            "comment_parent_not_found",
            "no such comment to answer",
        ),
    ] {
        if text.contains(needle) {
            return ApiError::new(StatusCode::BAD_REQUEST, code, message);
        }
    }

    // Any other check violation is the schema refusing something the store should have caught,
    // and the honest answer names the schema rather than guessing which rule it was.
    if db.code().as_deref() == Some("23514") {
        return ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "comment_rule_violation",
            "this comment breaks a rule the platform enforces on every comment",
        );
    }

    ApiError::from(error)
}

/// One store failure, as a 500 with the action named.
///
/// The house shape: a `sqlx::Error` has no `From` into `ApiError`, so a caller that forgets this
/// closure answers "your request was malformed" for a database that was down.
fn store_error(action: &str, error: sqlx::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("{action}: {error}"),
    )
}
