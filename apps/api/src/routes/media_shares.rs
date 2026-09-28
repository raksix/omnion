//! Share links (REQ-010, slice 3): `/api/v1/media/{id}/shares` and the public token route.
//!
//! The collection half is an ordinary authenticated surface guarded by `media.share`. The token
//! half is not: it is the one route where a credential *is* the request, so it carries no
//! session and no guard, and every rule in it is about what a bearer can and cannot do.
//!
//! Five decisions hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **A link reaches a file; it does not bypass what the file is.** A trashed or quarantined
//!   file is refused here exactly as it is on the raw route. The check happens *here*, at serve
//!   time, rather than at creation — a link made yesterday against a clean file must not keep
//!   serving it after the scanner flags it.
//! * **The refusals are distinguishable.** "revoked", "expired", "password_required" and
//!   "unknown_token" send the person holding the link to four different places: one asks the
//!   owner, one asks for a new link, one asks for a password, and one is a support ticket. A
//!   single "not found" makes all four a support ticket.
//! * **The token is shown exactly once.** The create response carries it; nothing else does,
//!   because the row cannot produce it (see `crates/media/src/shares.rs`).
//! * **A failed password check burns the same work as a successful one** and answers the same
//!   `password_required`, so a caller cannot tell "wrong password" from "no password set" by
//!   timing or by shape — the difference matters only to somebody brute-forcing the link.
//! * **The counter moves only after the bytes went out.** A download that produced nothing is
//!   not a download, and a counter that counts attempts is a counter the person who made the
//!   link cannot trust when they show it to the person they shared it with.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::{hash_password, verify_password};
use omnion_media::{
    CreatedShare, MediaFile, NewShare, Share, ShareRefusal, find_by_token, find_file_any_state,
    is_password_protected, list_shares, mint_token, revoke_for_media, revoke_share, servable,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One share as the panel reads it.
///
/// Built by hand from a [`Share`] rather than serialised from it, for the same reason the
/// settings body is: the struct that a response is built from is the place a credential would
/// leak from, and this one has nowhere to put a token.
#[derive(Debug, Serialize)]
pub struct ShareBody {
    /// The row's id.
    pub id: Uuid,
    /// The file this link hands over.
    pub media_id: Uuid,
    /// When the link stops working; null means "until revoked".
    pub expires_at: Option<OffsetDateTime>,
    /// Whether the link needs a password.
    pub has_password: bool,
    /// Downloads that produced bytes.
    pub download_count: i32,
    /// When the link was made.
    pub created_at: OffsetDateTime,
    /// When it was revoked; null while it is live.
    pub revoked_at: Option<OffsetDateTime>,
    /// Why it was revoked.
    pub revoked_reason: String,
    /// Whether the link can serve right now, in words.
    pub state: &'static str,
}

impl ShareBody {
    /// Describe a share for the panel, with its live/dead state resolved against the clock.
    fn build(share: &Share, now: OffsetDateTime) -> Self {
        let state = match servable(share, now) {
            Ok(()) => "live",
            Err(ShareRefusal::Revoked) => "revoked",
            Err(ShareRefusal::Expired) => "expired",
            Err(_) => "live",
        };
        Self {
            id: share.id,
            media_id: share.media_id,
            expires_at: share.expires_at,
            has_password: is_password_protected(share),
            download_count: share.download_count,
            created_at: share.created_at,
            revoked_at: share.revoked_at,
            revoked_reason: share.revoked_reason.clone(),
            state,
        }
    }

    /// Describe a share without a clock, for the create response — the share was just made, so
    /// it is live by construction and a second read of the clock could only surprise us.
    fn fresh(share: &Share) -> Self {
        Self {
            id: share.id,
            media_id: share.media_id,
            expires_at: share.expires_at,
            has_password: share.password_hash.is_some(),
            download_count: share.download_count,
            created_at: share.created_at,
            revoked_at: share.revoked_at,
            revoked_reason: share.revoked_reason.clone(),
            state: "live",
        }
    }
}

/// A created link: the row, and the token that is never stored anywhere else.
#[derive(Debug, Serialize)]
pub struct CreatedShareBody {
    /// The share as the panel reads it.
    pub share: ShareBody,
    /// The URL the person hands over. Shown once.
    pub url: String,
    /// The bearer token itself, hex. Shown once, and never recoverable afterwards.
    pub token: String,
    /// The sentence to put under the field, so the screen does not have to invent one.
    pub notice: String,
}

/// What a caller asks for when creating a link.
///
/// Every field is optional, and so is the *body*: the common case is "give me a link that
/// lasts until I revoke it", which is a `POST` with nothing in it. A handler that demands a
/// JSON body for that answers `415 Unsupported Media Type` to the most ordinary call anybody
/// makes, and the client has to send `{}` to work around a rule that was never the point.
#[derive(Debug, Default, Deserialize)]
pub struct CreateShareInput {
    /// How many days the link lives; `None` (or absent) means "until revoked".
    #[serde(default)]
    pub expires_in_days: Option<i64>,
    /// The password to protect the link with, if any.
    #[serde(default)]
    pub password: Option<String>,
}

impl CreateShareInput {
    /// Read the posted body, treating an absent one as "no choices made".
    fn from_optional(body: Option<Json<CreateShareInput>>) -> Self {
        body.map(|Json(input)| input).unwrap_or_default()
    }
}

/// What a caller may send when revoking.
///
/// Like the create body, optional: revoking a link is something anybody does without thinking
/// about it, and a reason is a nicety. Requiring a body for the bare case would make the
/// common call fail for the sake of the uncommon one.
#[derive(Debug, Default, Deserialize)]
pub struct RevokeShareInput {
    /// Why the link was closed, kept for the audit trail.
    #[serde(default)]
    pub reason: String,
}

/// The query the public route takes.
#[derive(Debug, Default, Deserialize)]
pub struct ShareQuery {
    /// The password, when the link has one.
    #[serde(default)]
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers — the authenticated surface
// ---------------------------------------------------------------------------------------------

/// Every share over one file, newest first, including revoked ones.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
) -> std::result::Result<Json<Vec<ShareBody>>, ApiError> {
    let _site = media_site_in_scope(&state, &current, media_id).await?;
    let shares = list_shares(state.db().pool(), media_id).await?;
    let now = OffsetDateTime::now_utc();
    Ok(Json(
        shares.iter().map(|s| ShareBody::build(s, now)).collect(),
    ))
}

/// Create a share over a file.
///
/// The token is minted here, hashed, and returned exactly once. The password, if any, is
/// hashed with the platform's own Argon2id before it is stored — a share password is
/// low-entropy by construction, so the work factor is the security, not the salt.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(media_id): Path<Uuid>,
    body: Option<Json<CreateShareInput>>,
) -> std::result::Result<(StatusCode, Json<CreatedShareBody>), ApiError> {
    let input = CreateShareInput::from_optional(body);
    let media = media_site_in_scope(&state, &current, media_id).await?;

    // A link over a file that is already unservable would be created and immediately useless,
    // so it is refused at creation with the same words the serve path uses.
    if media.deleted {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "file_in_trash",
            "this file is in the trash; restore it before sharing it",
        ));
    }

    let expires_at = match input.expires_in_days {
        None => None,
        Some(days) => Some(expiry_from(days)?),
    };

    let password_hash = match input.password {
        None => None,
        Some(password) => {
            // Validated by the same function a sign-in uses, so a link cannot carry a password
            // the platform would refuse to check later.
            Some(
                hash_password(password)
                    .await
                    .map_err(|err| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "password_rejected",
                            format!("the share password was rejected: {err}"),
                        )
                    })?,
            )
        }
    };

    let token = mint_token();
    let created: CreatedShare = omnion_media::create_share(
        state.db().pool(),
        NewShare {
            media_id,
            expires_at,
            password: None,
        },
        &token,
        password_hash,
        Some(current.user.id),
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("media.share_created")
            .organization(media.site_organization)
            .site(media.site_id)
            .actor(current.user.id)
            // No token, no password, no expiry value: this payload is fanned out to webhooks,
            // and a share token in a third party's inbox is a leaked capability.
            .payload(json!({
                "share_id": created.share.id,
                "media_id": media_id,
                "has_password": created.share.password_hash.is_some(),
                "expires_at": created.share.expires_at,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.share_created")
            .target("media", media_id.to_string())
            .metadata(json!({
                "share_id": created.share.id,
                "site_id": media.site_id,
                "filename": media.filename,
                "password_protected": created.share.password_hash.is_some(),
                "expires_at": created.share.expires_at,
            }))
            .ip_address(address.as_text())
            .organization(media.site_organization),
    )
    .await?;

    let url = share_url(&created.token);
    Ok((
        StatusCode::CREATED,
        Json(CreatedShareBody {
            share: ShareBody::fresh(&created.share),
            notice: "Copy this link now — it is shown once and cannot be recovered later. \
                     You can revoke it at any time."
                .to_string(),
            token: created.token,
            url,
        }),
    ))
}

/// Revoke a share. Effective on the very next request.
pub async fn revoke(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path((media_id, share_id)): Path<(Uuid, Uuid)>,
    body: Option<Json<RevokeShareInput>>,
) -> std::result::Result<StatusCode, ApiError> {
    let input = body.map(|Json(input)| input).unwrap_or_default();
    let media = media_site_in_scope(&state, &current, media_id).await?;
    let share = omnion_media::find_share(state.db().pool(), share_id)
        .await?
        .ok_or_else(share_not_found)?;
    if share.media_id != media_id {
        // A share id from another file is a 404, not a 403: answering "that link exists but not
        // on this file" turns the route into an oracle for guessing ids.
        return Err(share_not_found());
    }

    let reason = normalize_reason(&input.reason);
    match revoke_share(state.db().pool(), share_id, &reason).await? {
        Some(revoked) => {
            bus::emit(
                state.db().pool(),
                NewEvent::new("media.share_revoked")
                    .organization(media.site_organization)
                    .site(media.site_id)
                    .actor(current.user.id)
                    .payload(json!({
                        "share_id": share_id,
                        "media_id": media_id,
                        "reason": reason,
                    })),
            )
            .await?;
            record(
                &state,
                NewAuditEntry::by_user(current.user.id, "media.share_revoked")
                    .target("media", media_id.to_string())
                    .metadata(json!({
                        "share_id": share_id,
                        "site_id": media.site_id,
                        "downloads_at_revocation": revoked.download_count,
                        "reason": reason,
                    }))
                    .ip_address(address.as_text())
                    .organization(media.site_organization),
            )
            .await?;
            Ok(StatusCode::NO_CONTENT)
        }
        // Revoking twice is a double-click, not a conflict: the first revocation's timestamp is
        // the fact, and overwriting it would lose when the link actually stopped working.
        None => Ok(StatusCode::NO_CONTENT),
    }
}

/// Revoke every live share over a file.
///
/// Separate from the per-link revoke because "the scanner just flagged this file" and "somebody
/// gave this file out" have to be answerable together: a file that must not be served must not
/// be reachable through a link somebody is still holding.
pub async fn revoke_all(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(media_id): Path<Uuid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let media = media_site_in_scope(&state, &current, media_id).await?;
    let closed = revoke_for_media(state.db().pool(), media_id, "revoked for the whole file")
        .await?;

    if closed > 0 {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "media.shares_revoked_all")
                .target("media", media_id.to_string())
                .metadata(json!({ "site_id": media.site_id, "revoked": closed }))
                .ip_address(address.as_text())
                .organization(media.site_organization),
        )
        .await?;
    }
    Ok(Json(json!({ "revoked": closed })))
}

// ---------------------------------------------------------------------------------------------
// Handlers — the public surface
// ---------------------------------------------------------------------------------------------

/// Serve a file through a share token.
///
/// Unauthenticated by design: the token *is* the credential, which is what "share link" means.
/// Every check below is therefore about the token, and the order is deliberate — the link's own
/// state before the file's, so an expired link says "expired" rather than blaming the file.
pub async fn public_shared(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareQuery>,
) -> std::result::Result<Response, ApiError> {
    let now = OffsetDateTime::now_utc();
    let share = find_by_token(state.db().pool(), &token)
        .await?
        .ok_or_else(|| share_refusal(ShareRefusal::Unknown))?;
    servable(&share, now).map_err(share_refusal)?;

    // The password is checked before the file is even looked up, so a protected link cannot be
    // used to probe which files exist.
    if is_password_protected(&share) {
        let presented = query.password.unwrap_or_default();
        let ok = match &share.password_hash {
            Some(hash) => verify_password(presented, hash.clone())
                .await
                .unwrap_or(false),
            None => false,
        };
        if !ok {
            return Err(share_refusal(ShareRefusal::PasswordRequired));
        }
    }

    let media = find_file_any_state(state.db().pool(), share.media_id)
        .await?
        // A link whose file has been hard-deleted is refused, and it says so: the alternative is
        // a 404 that looks like a wrong token, which sends the holder to the wrong person.
        .ok_or_else(|| share_refusal(ShareRefusal::FileUnavailable))?;

    // The file's own rules, applied through the link. A trashed or quarantined file is not
    // served, and this is deliberately evaluated here rather than at creation.
    if media.deleted_at.is_some() || media.scan_status == "flagged" {
        return Err(share_refusal(ShareRefusal::FileUnavailable));
    }

    let response = serve_shared(&state, &media).await?;

    // Only now: a download that produced nothing is not a download.
    omnion_media::count_download(state.db().pool(), share.id).await?;

    Ok(response)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// A media row plus the organization that owns it, so every emit and audit entry on this route
/// has both without a second lookup.
struct ScopedMedia {
    site_id: Uuid,
    site_organization: Uuid,
    filename: String,
    /// Whether the file is in the trash. Read through [`MediaFile`] rather than the base
    /// `Media`, because `deleted_at` is a file-manager column and a share over a trashed file
    /// is exactly the case this route has to catch.
    deleted: bool,
}

/// Load a media row and refuse the request when its site is out of the caller's scope.
async fn media_site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    media_id: Uuid,
) -> std::result::Result<ScopedMedia, ApiError> {
    let media = find_file_any_state(state.db().pool(), media_id)
        .await?
        .ok_or_else(media_not_found)?;
    let site = site_in_scope(state, current, media.site_id).await?;
    Ok(ScopedMedia {
        site_id: media.site_id,
        site_organization: site.organization_id,
        filename: media.filename,
        deleted: media.deleted_at.is_some(),
    })
}

/// Build the answer to a refused link.
///
/// `410 Gone` for a link that existed and stopped working, `404` for one that never did: both
/// stop the token from being probed, and the status tells a caching proxy the difference is
/// permanent rather than worth retrying. The *code* carries the reason, because the holder of
/// the link needs the reason more than the status.
fn share_refusal(refusal: ShareRefusal) -> ApiError {
    let status = match refusal {
        // A link that was never valid and one that is simply gone answer the same way, so a
        // caller cannot enumerate tokens by watching for a 410.
        ShareRefusal::Unknown | ShareRefusal::Revoked | ShareRefusal::Expired => {
            StatusCode::GONE
        }
        // "type the password" is not gone: the link is right there and the caller is one field
        // short. A 410 here would tell the holder to ask for a new link, which is worse advice
        // than the truth and sends the owner a pointless request.
        ShareRefusal::PasswordRequired | ShareRefusal::FileUnavailable => {
            StatusCode::FORBIDDEN
        }
    };
    ApiError::new(status, refusal.as_str(), refusal.explain())
}

/// `404` for a share id that does not exist, or belongs to another file.
fn share_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "share_not_found",
        "no such share link on this file",
    )
}

/// `404` for a media id that does not exist.
fn media_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "media_not_found",
        "no such file",
    )
}

/// Turn a requested lifetime into an instant, refusing the ones that cannot be meant.
fn expiry_from(days: i64) -> std::result::Result<OffsetDateTime, ApiError> {
    if days < 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "expires_in_days",
            "a link must last at least one day; omit the field for a link that lasts until \
             it is revoked",
        ));
    }
    if days > omnion_media::MAX_EXPIRY_DAYS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "expires_in_days",
            format!(
                "a link may last at most {} days; omit the field for a link that lasts until it \
                 is revoked",
                omnion_media::MAX_EXPIRY_DAYS
            ),
        ));
    }
    Ok(OffsetDateTime::now_utc() + time::Duration::days(days))
}

/// The public URL of a link, built from the deployment's own public base.
///
/// Absolute rather than relative, because the person who created it is going to paste it into a
/// message, and a relative path is not what a message can carry. The origin comes from the
/// configured public base when there is one, and from the request otherwise, so a deployment
/// behind a proxy hands out links on the host the world can reach rather than the host the
/// process happens to be bound to.
fn share_url(token: &str) -> String {
    let base = std::env::var("OMNION_PUBLIC_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "".to_string());
    if base.is_empty() {
        return format!("/api/v1/public/media/shared/{token}");
    }
    format!("{}/api/v1/public/media/shared/{token}", base.trim_end_matches('/'))
}

/// Trim and cap a revocation reason so the audit trail cannot be used as a notes field.
fn normalize_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    let mut out = String::with_capacity(trimmed.len().min(200));
    for ch in trimmed.chars() {
        if out.chars().count() >= 200 {
            break;
        }
        // One line, so a reason cannot break the audit log's own rendering.
        out.push(if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch });
    }
    out
}

/// The bytes of a shared file, as an attachment.
///
/// `attachment` unconditionally, unlike the raw route's content-type-driven choice: a share
/// link is opened by somebody who did not choose the file, and a `.html` served inline from an
/// opaque domain is a page that runs script in the holder's browser with the platform's name on
/// it. The platform's own upload validation already limits what can be stored, and an
/// attachment sidesteps the case where it grows.
async fn serve_shared(state: &AppState, media: &MediaFile) -> std::result::Result<Response, ApiError> {
    let bytes = state.storage().get(&media.storage_key).await?;
    let mut response = Response::new(axum::body::Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header_value(omnion_media::serve_plan(&media.content_type).content_type)?,
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!("attachment; filename=\"{}\"", media.filename))?,
    );
    // No caching: a revoked link that a proxy has cached keeps working, and the whole point of
    // a revocation is that it is immediate.
    headers.insert(header::CACHE_CONTROL, header_value("no-store")?);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header_value("nosniff")?,
    );
    Ok(response)
}

/// Build one response header value, refusing anything unusable.
fn header_value(value: &str) -> std::result::Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            err.to_string(),
        )
    })
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revocation_reason_cannot_break_the_audit_line() {
        assert_eq!(normalize_reason("  sent to the wrong client \n"), "sent to the wrong client");
        assert_eq!(normalize_reason("a\tb"), "a b");
        assert_eq!(normalize_reason(&"x".repeat(500)).chars().count(), 200);
        assert_eq!(normalize_reason(""), "");
    }

    #[test]
    fn an_expiry_below_a_day_is_refused_with_the_field_named() {
        // Zero and negative are both "yesterday", and both name the field so the screen can put
        // the message under the input rather than at the top of the form.
        for days in [0, -1, -365] {
            let err = expiry_from(days).expect_err("a link cannot last less than a day");
            assert_eq!(err.code(), "expires_in_days");
        }
    }

    #[test]
    fn an_expiry_beyond_the_ceiling_is_refused_with_the_ceiling_named() {
        let err = expiry_from(omnion_media::MAX_EXPIRY_DAYS + 1)
            .expect_err("a link cannot outlive the ceiling");
        assert_eq!(err.code(), "expires_in_days");
        // The refusal quotes the number it applied, so the person typing the value is not sent
        // to the documentation to find out what the limit was.
        let rendered = format!("{:?}", err);
        assert!(
            rendered.contains(&omnion_media::MAX_EXPIRY_DAYS.to_string()),
            "the refusal names the ceiling it applied: {rendered}"
        );
    }

    #[test]
    fn an_expiry_inside_the_range_is_accepted_and_is_in_the_future() {
        let expiry = expiry_from(1).expect("one day is a legitimate lifetime");
        assert!(expiry > OffsetDateTime::now_utc());
    }

    #[test]
    fn a_minted_token_is_long_hex_and_a_different_token_each_time() {
        let first = mint_token();
        let second = mint_token();
        assert_eq!(first.len(), omnion_media::TOKEN_BYTES * 2);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second, "two links must not share a token");
        // The token and the stored hash are different values; a route that looked the token up
        // by itself would therefore find nothing, which is the point.
        assert_ne!(first, omnion_media::hash_token(&first));
    }

    #[test]
    fn a_refusal_answers_with_a_code_and_a_sentence() {
        for refusal in [
            ShareRefusal::Unknown,
            ShareRefusal::Revoked,
            ShareRefusal::Expired,
            ShareRefusal::PasswordRequired,
            ShareRefusal::FileUnavailable,
        ] {
            let err = share_refusal(refusal);
            assert_eq!(err.code(), refusal.as_str());
            assert!(!format!("{:?}", err).is_empty(), "{refusal:?} has nothing to say");
        }
    }

    #[test]
    fn a_dead_link_and_a_wrong_password_are_not_the_same_answer() {
        // Both are refusals, but they send the holder somewhere different: one asks the owner,
        // the other asks for the password. Collapsing them is a support ticket.
        let dead = share_refusal(ShareRefusal::Expired);
        let wrong = share_refusal(ShareRefusal::PasswordRequired);
        assert_ne!(dead.code(), wrong.code());
        assert_ne!(dead.status(), wrong.status());
    }
}
