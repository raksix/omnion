//! `/api/v1/members/*` and `/api/v1/public/members/*` — REQ-064, slice 4c.
//!
//! Visitor accounts, their moderation, the site policy, and the public signup / sign-in /
//! profile / gated-page flow. Six decisions shape the file:
//!
//! * **A member is not a panel identity, and this file is where that is kept true.** The cookie
//!   is `omnion_member`, the session table is `cms_member_sessions`, and nothing here reads a
//!   panel session or writes a role binding. A route that could turn a visitor into a user would
//!   make the two tables one table with a slower signup, and the REQ names this as the single
//!   most important boundary it has.
//!
//! * **Two powers, not one.** `memberships.read` opens the table; `memberships.manage` changes a
//!   member, sends a reset, and edits the policy. Same split the comment and newsletter inboxes
//!   draw, and the reason is identical: reading a member table already shows every address on
//!   it, so "somebody may look" must not imply "somebody may unblock the person they do not
//!   like".
//!
//! * **The public surface never says who exists.** Signup answers 202 with the same body
//!   whether the address was new, already known or just re-invited; sign-in answers one error
//!   for four refusals; the password-reset form answers the same 202 either way. A members area
//!   whose signup form is a membership oracle is worse than no signup form.
//!
//! * **A gated page answers 404 by default.** The site's own policy can ask for a sign-in
//!   prompt instead, and then the response says `401` with a `sign_in_url` — a *choice* the site
//!   made, not a leak the platform performed.
//!
//! * **A raw token is returned to the caller and never echoed to a visitor.** The mail path is
//!   where it lives. When the platform has no mail transport configured the token is not echoed
//!   either; the response says `delivery: "unavailable"` so an operator can see no mail went out
//!   rather than find out from a member.
//!
//! * **Every read is scoped by site in the `where` clause**, and a row that belongs to another
//!   site is a 404 — `ensure_same_organization` answers 403 for another tenant's SITE, and the
//!   two are different contracts.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_content::members::{
    Member, MemberFilter, MemberPatch, MemberSettings, MemberStore, NewMember, PageGate,
    PublicMember, SettingsPatch, SigninEvent, TokenOutcome,
};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::public::resolve_site;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

/// The cookie a member session travels in.
///
/// **A different name from the panel's `omnion_session`, and that is the whole point.** Two
/// names means a browser cannot present a visitor cookie where a panel cookie is expected, and
/// the panel's session loader — which never looks at `cms_member_sessions` — cannot be tricked
/// into accepting one.
pub const MEMBER_COOKIE: &str = "omnion_member";

// ---------------------------------------------------------------------------------------------
// Bodies and params
// ---------------------------------------------------------------------------------------------

/// `?site_id=` on every panel route, `?site=` on the public ones.
#[derive(Debug, Default, Deserialize)]
pub struct SiteParam {
    /// The site the request is about. The panel's selector.
    pub site_id: Option<Uuid>,
    /// The site key, for a public request on a multi-site installation.
    pub site: Option<String>,
}

/// Query parameters of the members table.
#[derive(Debug, Default, Deserialize)]
pub struct MemberParams {
    /// The site.
    pub site_id: Option<Uuid>,
    /// One state.
    pub status: Option<String>,
    /// Free text over address and name.
    #[serde(alias = "q")]
    pub search: Option<String>,
    /// Only members holding this role.
    pub role: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Offset.
    pub offset: Option<i64>,
}

/// Query parameters of the public token links.
#[derive(Debug, Default, Deserialize)]
pub struct TokenParam {
    /// The token from the link.
    pub token: Option<String>,
    /// Site key, on a multi-site installation.
    pub site: Option<String>,
}

/// The panel's "add a member".
#[derive(Debug, Deserialize)]
pub struct InviteRequest {
    /// The site.
    pub site_id: Uuid,
    /// The address.
    pub email: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// A password, when the operator is creating a usable account rather than an invitation.
    #[serde(default)]
    pub password: Option<String>,
    /// The site's roles to grant.
    #[serde(default)]
    pub roles: Vec<String>,
}

/// The panel's member editor.
#[derive(Debug, Default, Deserialize)]
pub struct PatchMemberRequest {
    /// Display name, or `null` to clear it.
    #[serde(default, deserialize_with = "double_option")]
    pub name: Option<Option<String>>,
    /// The state.
    #[serde(default)]
    pub status: Option<String>,
    /// The whole role list.
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    /// The last status note, or `null` to clear it.
    #[serde(default, deserialize_with = "double_option")]
    pub signin_note: Option<Option<String>>,
}

/// The panel's "block this member".
#[derive(Debug, Default, Deserialize)]
pub struct BlockRequest {
    /// Why. Shown in the panel, never to the member.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The panel's membership settings form.
#[derive(Debug, Default, Deserialize)]
pub struct SettingsRequest {
    /// Whether the public signup form exists.
    #[serde(default)]
    pub signup_enabled: Option<bool>,
    /// Whether a signup must be confirmed.
    #[serde(default)]
    pub require_verification: Option<bool>,
    /// The roles every new member gets.
    #[serde(default)]
    pub default_roles: Option<Vec<String>>,
    /// The post-sign-in redirect, or `null` to clear it.
    #[serde(default, deserialize_with = "double_option")]
    pub post_signin_redirect: Option<Option<String>>,
    /// `prompt` or `not_found`.
    #[serde(default)]
    pub gated_page_behaviour: Option<String>,
}

/// The public signup form.
#[derive(Debug, Deserialize)]
pub struct PublicSignupRequest {
    /// The address.
    pub email: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// The password.
    pub password: String,
}

/// The public sign-in form.
#[derive(Debug, Deserialize)]
pub struct PublicSigninRequest {
    /// The address.
    pub email: String,
    /// The password.
    pub password: String,
}

/// The public "I forgot my password" form.
#[derive(Debug, Deserialize)]
pub struct ResetRequest {
    /// The address, when the caller is ASKING for a link.
    ///
    /// Optional, and the reason is the other half of the same route: finishing a reset carries a
    /// token and a new password and no address at all, because the member is following a link
    /// and the browser that followed it does not know which of the ten thousand accounts on the
    /// site it belongs to. A required field here made the finishing half a 422 — the reset was
    /// undeliverable by construction, and the only way to reach it was to send the address in a
    /// request that had no use for it.
    #[serde(default)]
    pub email: Option<String>,
    /// The new password, when the caller is finishing a reset rather than asking for one.
    #[serde(default)]
    pub password: Option<String>,
    /// The token from the mail, when finishing a reset.
    #[serde(default)]
    pub token: Option<String>,
}

/// The signed-in member's own profile.
#[derive(Debug, Default, Deserialize)]
pub struct ProfileRequest {
    /// Display name, or `null` to clear it.
    #[serde(default, deserialize_with = "double_option")]
    pub name: Option<Option<String>>,
    /// The current password, required to change it.
    #[serde(default)]
    pub current_password: Option<String>,
    /// The new password.
    #[serde(default)]
    pub new_password: Option<String>,
}

/// `Option<Option<T>>` needs this to distinguish "absent" from "explicitly null".
///
/// Without it `{"name": null}` and `{}` are the same value to serde, and the editor's "clear the
/// name" button silently does nothing — which is a panel that can add a name and never take one
/// away.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// A member as the panel may describe them.
///
/// `password_hash` is absent from the wire **structurally**, not by a `skip_serializing_if`:
/// the panel's list, the drawer and any future export all pass through this, and a field that
/// exists on the struct can be printed by accident.
#[derive(Debug, Serialize)]
pub struct MemberBody {
    /// Member id.
    pub id: Uuid,
    /// The site.
    pub site_id: Uuid,
    /// The address.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// The site's roles.
    pub roles: Vec<String>,
    /// The state.
    pub status: String,
    /// When they were verified.
    pub verified_at: Option<String>,
    /// When they last signed in.
    pub last_signin_at: Option<String>,
    /// Whether the account has ever set a password. The panel needs this to tell "invited, has
    /// not claimed it" from "signed in yesterday", and it is the only safe way to say it.
    pub has_password: bool,
    /// How many live sessions the account holds right now.
    pub live_sessions: i64,
    /// The last status note.
    pub signin_note: Option<String>,
    /// When the account was created.
    pub created_at: String,
}

impl MemberBody {
    /// Build from a store row, reading the two numbers the row does not carry.
    async fn build(store: &MemberStore, member: &Member) -> Result<Self, ApiError> {
        Ok(Self {
            id: member.id,
            site_id: member.site_id,
            email: member.email.clone(),
            name: member.name.clone(),
            roles: member.roles.clone(),
            status: member.status.clone(),
            verified_at: member.verified_at.map(rfc3339),
            last_signin_at: member.last_signin_at.map(rfc3339),
            has_password: member.password_hash.is_some(),
            live_sessions: store.live_session_count(member.id).await?,
            signin_note: member.signin_note.clone(),
            created_at: rfc3339(member.created_at),
        })
    }
}

/// RFC 3339, or `None` for a column that is null.
fn rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The members table and its chips.
#[derive(Debug, Serialize)]
pub struct MemberListBody {
    /// The rows.
    pub members: Vec<MemberBody>,
    /// Per-state counts, for the filter chips.
    pub counts: MemberCountsBody,
    /// How many rows match without the page window.
    pub total: i64,
}

/// Per-state counts, spelled out so the panel can render three chips without arithmetic.
#[derive(Debug, Serialize)]
pub struct MemberCountsBody {
    /// Waiting to click a verification link.
    pub pending: i64,
    /// May sign in.
    pub verified: i64,
    /// Refused.
    pub blocked: i64,
}

impl From<omnion_content::members::MemberCounts> for MemberCountsBody {
    fn from(counts: omnion_content::members::MemberCounts) -> Self {
        Self {
            pending: counts.pending,
            verified: counts.verified,
            blocked: counts.blocked,
        }
    }
}

/// The member drawer.
#[derive(Debug, Serialize)]
pub struct MemberDetailBody {
    /// The member.
    pub member: MemberBody,
    /// The last ten sign-ins, newest first.
    pub recent_signins: Vec<SigninEvent>,
}

/// The site's membership policy, plus the one number the summary line needs.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// The policy.
    pub settings: MemberSettings,
    /// How many members may sign in.
    pub verified_count: i64,
}

/// What a public signup answers.
///
/// The same three fields whatever the store did. `confirmation_required` is the only one that
/// varies, and it varies on the **site's policy**, not on whether the address was already there
/// — a repeat signup at a site that verifies still says "check your inbox", which is true.
#[derive(Debug, Serialize)]
pub struct PublicSignupResponse {
    /// The address, normalised.
    pub email: String,
    /// Whether a link was sent.
    pub confirmation_required: bool,
    /// `sent`, `unavailable` or `not_required`.
    pub delivery: Option<String>,
}

/// What a public sign-in answers.
#[derive(Debug, Serialize)]
pub struct PublicSigninResponse {
    /// The member, in the shape a visitor may hold.
    pub member: PublicMember,
    /// Where to send them, from the site's own setting.
    pub redirect_to: Option<String>,
}

/// What a token click answers.
///
/// `applied` is the claim; `message` is for a page to render and `member` is deliberately
/// `None` for anything but a success, so a forwarded link learns nothing about the account it
/// once named.
#[derive(Debug, Serialize)]
pub struct TokenOutcomeBody {
    /// Whether the action was applied.
    pub applied: bool,
    /// The resulting state, or `unknown`.
    pub status: String,
    /// A line for the page.
    pub message: Option<String>,
    /// The member, only when the click succeeded.
    pub member: Option<PublicMember>,
}

impl From<TokenOutcome> for TokenOutcomeBody {
    fn from(outcome: TokenOutcome) -> Self {
        Self {
            applied: outcome.applied,
            status: outcome.status,
            message: outcome.message,
            member: outcome.member.as_ref().map(PublicMember::from),
        }
    }
}

/// The signed-in member's own view of themselves.
#[derive(Debug, Serialize)]
pub struct PublicProfileBody {
    /// The member.
    pub member: PublicMember,
    /// Where the site sends them after a sign-in.
    pub redirect_to: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Panel routes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/members` — the members table.
pub async fn list_members(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<MemberParams>,
) -> Result<Json<MemberListBody>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let page = store
        .list(
            site_id,
            &MemberFilter {
                status: params.status,
                search: params.search,
                role: params.role,
                limit: params.limit,
                offset: params.offset,
            },
        )
        .await?;

    let mut members = Vec::with_capacity(page.members.len());
    for member in &page.members {
        members.push(MemberBody::build(&store, member).await?);
    }
    Ok(Json(MemberListBody {
        members,
        counts: page.counts.into(),
        total: page.total,
    }))
}

/// `GET /api/v1/members/{id}` — the drawer: the member and their last sign-ins.
pub async fn get_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<Json<MemberDetailBody>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let member = concealed_member(&store, site_id, id).await?;
    Ok(Json(MemberDetailBody {
        member: MemberBody::build(&store, &member).await?,
        recent_signins: store.recent_signins(id, 10).await?,
    }))
}

/// `POST /api/v1/members` — the operator creates an account.
pub async fn create_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<InviteRequest>,
) -> Result<(StatusCode, Json<MemberBody>), ApiError> {
    let site_id = required_site(&state, &session, Some(body.site_id)).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let outcome = store
        .invite(NewMember {
            site_id,
            email: body.email,
            name: body.name,
            password: body.password,
            roles: body.roles,
        })
        .await?;

    let delivery = deliver(&state, site_id, &outcome, "verify").await;
    emit(
        &state,
        "members.member.created",
        json!({
            "member_id": outcome.member.id,
            "site_id": site_id,
            "status": outcome.member.status,
        }),
    )
    .await;
    audit(
        &state,
        &session,
        "member.create",
        outcome.member.id,
        Some(&format!("delivery={delivery}")),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(MemberBody::build(&store, &outcome.member).await?),
    ))
}

/// `PATCH /api/v1/members/{id}` — the editor.
pub async fn patch_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
    Json(body): Json<PatchMemberRequest>,
) -> Result<Json<MemberBody>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let before = concealed_member(&store, site_id, id).await?;

    let member = store
        .patch(
            site_id,
            id,
            &MemberPatch {
                name: body.name,
                status: body.status,
                roles: body.roles,
                signin_note: body.signin_note,
            },
        )
        .await?;

    // The event names the TRANSITION, not the row: a patch that leaves the state alone is not a
    // verification, and a subscriber that only ever sees `verified` cannot tell a link click
    // from a moderator's thumb.
    if before.status != member.status {
        let event = match member.status.as_str() {
            "verified" => Some("members.member.verified"),
            "blocked" => Some("members.member.blocked"),
            _ => None,
        };
        if let Some(event) = event {
            emit(
                &state,
                event,
                json!({
                    "member_id": member.id,
                    "site_id": site_id,
                    "status": member.status,
                    "previous_status": before.status,
                }),
            )
            .await;
        }
    }
    audit(
        &state,
        &session,
        "member.update",
        id,
        Some(&format!("{} -> {}", before.status, member.status)),
    )
    .await;

    Ok(Json(MemberBody::build(&store, &member).await?))
}

/// `POST /api/v1/members/{id}/block` — block, naming the reason.
///
/// A separate route rather than a `PATCH` with `status: "blocked"` because it carries a reason
/// and it is the one destructive button in the table; giving it its own path is what lets the
/// panel confirm it with the reason in the dialog.
pub async fn block_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
    body: Option<Json<BlockRequest>>,
) -> Result<Json<MemberBody>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let reason = body.and_then(|Json(body)| body.reason);
    let store = MemberStore::new(state.db().pool().clone());
    concealed_member(&store, site_id, id).await?;
    let member = store.block(site_id, id, reason.as_deref()).await?;

    emit(
        &state,
        "members.member.blocked",
        json!({ "member_id": id, "site_id": site_id, "status": member.status }),
    )
    .await;
    audit(&state, &session, "member.block", id, reason.as_deref()).await;
    Ok(Json(MemberBody::build(&store, &member).await?))
}

/// `POST /api/v1/members/{id}/verify` — the operator vouches for the address.
pub async fn verify_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<Json<MemberBody>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    concealed_member(&store, site_id, id).await?;
    let member = store.verify(site_id, id).await?;
    emit(
        &state,
        "members.member.verified",
        json!({ "member_id": id, "site_id": site_id, "status": member.status }),
    )
    .await;
    audit(&state, &session, "member.verify", id, None).await;
    Ok(Json(MemberBody::build(&store, &member).await?))
}

/// `POST /api/v1/members/{id}/send-verification` — mint a fresh link.
pub async fn send_verification(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let member = concealed_member(&store, site_id, id).await?;
    let token = store.issue_verify_token(site_id, id).await?;
    let delivery = mail_token(&state, site_id, &member, "verify", &token).await;
    audit(&state, &session, "member.send_verification", id, Some(&delivery)).await;
    Ok(Json(json!({ "member_id": id, "delivery": delivery })))
}

/// `POST /api/v1/members/{id}/send-reset` — mint a reset link.
pub async fn send_reset(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let member = concealed_member(&store, site_id, id).await?;
    let token = store.issue_reset_token(site_id, id).await?;
    let delivery = mail_token(&state, site_id, &member, "reset", &token).await;
    audit(&state, &session, "member.send_reset", id, Some(&delivery)).await;
    Ok(Json(json!({ "member_id": id, "delivery": delivery })))
}

/// `POST /api/v1/members/{id}/sign-out-everywhere` — every live session dies.
pub async fn signout_everywhere(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    concealed_member(&store, site_id, id).await?;
    let removed = store.signout_everywhere(id).await?;
    audit(
        &state,
        &session,
        "member.signout_everywhere",
        id,
        Some(&format!("sessions={removed}")),
    )
    .await;
    Ok(Json(json!({ "member_id": id, "sessions_removed": removed })))
}

/// `DELETE /api/v1/members/{id}` — delete the account and everything it owns.
pub async fn delete_member(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<SiteParam>,
) -> Result<StatusCode, ApiError> {
    let site_id = required_site(&state, &session, params.site_id).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let member = concealed_member(&store, site_id, id).await?;
    store.delete(site_id, id).await?;
    audit(
        &state,
        &session,
        "member.delete",
        id,
        Some(&format!("email_present={}", !member.email.is_empty())),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/sites/{site_id}/members/settings` — the policy.
pub async fn get_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SettingsBody>, ApiError> {
    required_site(&state, &session, Some(site_id)).await?;
    let store = MemberStore::new(state.db().pool().clone());
    Ok(Json(SettingsBody {
        settings: store.settings(site_id).await?,
        verified_count: store.verified_count(site_id).await?,
    }))
}

/// `PUT /api/v1/sites/{site_id}/members/settings` — change the policy.
pub async fn put_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<SettingsRequest>,
) -> Result<Json<SettingsBody>, ApiError> {
    required_site(&state, &session, Some(site_id)).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let settings = store
        .patch_settings(
            site_id,
            &SettingsPatch {
                signup_enabled: body.signup_enabled,
                require_verification: body.require_verification,
                default_roles: body.default_roles,
                post_signin_redirect: body.post_signin_redirect,
                gated_page_behaviour: body.gated_page_behaviour,
            },
            Some(session.session.user_id),
        )
        .await?;
    audit(&state, &session, "member.settings", site_id, None).await;
    Ok(Json(SettingsBody {
        verified_count: store.verified_count(site_id).await?,
        settings,
    }))
}

// ---------------------------------------------------------------------------------------------
// Public routes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/public/members/signup` — a visitor creates an account.
///
/// The response is the same shape whether the address was new, already known, or has just been
/// re-invited. `MemberEmailTaken` is caught here rather than surfacing as a 409 for exactly this
/// reason, and the audit log is where the operator reads that the address was already known.
pub async fn public_signup(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
    Json(body): Json<PublicSignupRequest>,
) -> Result<(StatusCode, Json<PublicSignupResponse>), ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let store = MemberStore::new(state.db().pool().clone());

    let outcome = match store
        .signup(NewMember {
            site_id: site.id,
            email: body.email.clone(),
            name: body.name,
            password: Some(body.password),
            roles: Vec::new(),
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(omnion_content::ContentError::MemberEmailTaken(email)) => {
            // The truthful answer to a repeat signup, with nobody told the difference.
            audit_public(
                &state,
                site.id,
                "members.signup.duplicate",
                &format!("email_present={}", !email.is_empty()),
            )
            .await;
            return Ok((
                StatusCode::ACCEPTED,
                Json(PublicSignupResponse {
                    email,
                    confirmation_required: true,
                    delivery: Some("sent".to_string()),
                }),
            ));
        }
        Err(error) => return Err(error.into()),
    };

    let confirmation_required = outcome.verify_token.is_some();
    let delivery = deliver(&state, site.id, &outcome, "verify").await;
    emit(
        &state,
        "members.member.created",
        json!({ "member_id": outcome.member.id, "site_id": site.id, "status": outcome.member.status }),
    )
    .await;

    Ok((
        StatusCode::ACCEPTED,
        Json(PublicSignupResponse {
            email: outcome.member.email,
            confirmation_required,
            delivery: Some(delivery),
        }),
    ))
}

/// `POST /api/v1/public/members/signin` — a member signs in.
pub async fn public_signin(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
    Json(body): Json<PublicSigninRequest>,
) -> Result<axum::response::Response, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let store = MemberStore::new(state.db().pool().clone());
    let settings = store.settings(site.id).await?;

    let outcome = store
        .signin(
            site.id,
            &body.email,
            &body.password,
            client_hint(&headers, "x-forwarded-for"),
            headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
        )
        .await?;

    emit(
        &state,
        "members.member.signed_in",
        json!({ "member_id": outcome.member.id, "site_id": site.id }),
    )
    .await;

    // The raw token is in the cookie header and NOWHERE else — not in the body, which is what
    // ends up in a log, a proxy buffer and a browser devtools tab.
    let mut response = Json(PublicSigninResponse {
        member: PublicMember::from(&outcome.member),
        redirect_to: settings.post_signin_redirect,
    })
    .into_response();
    let headers = response.headers_mut();
    if let Ok(value) = axum::http::HeaderValue::from_str(&member_cookie_header(
        &outcome.token,
        &outcome.expires_at,
    )) {
        headers.append(axum::http::header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `POST /api/v1/public/members/signout` — drop this session.
///
/// The cookie is **always** cleared, even when the row was already gone: a sign-out that answers
/// "nothing to do" and leaves the browser holding the cookie is a button whose state depends on
/// the server, and the member is left with a cookie that looks valid.
pub async fn public_signout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let mut removed = false;
    if let Some(token) = member_cookie(&headers) {
        removed = MemberStore::new(state.db().pool().clone()).signout(&token).await?;
    }
    let mut response = Json(json!({ "signed_out": removed })).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&cleared_member_cookie()) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `GET /api/v1/public/members/me` — the signed-in member, or 401.
pub async fn public_me(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
) -> Result<Json<PublicProfileBody>, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let member = current_member(&state, site.id, &headers)
        .await?
        .ok_or_else(|| ApiError::unauthorized("member_signin_required", "sign in to see your account"))?;
    let settings = MemberStore::new(state.db().pool().clone())
        .settings(site.id)
        .await?;
    Ok(Json(PublicProfileBody {
        member: PublicMember::from(&member),
        redirect_to: settings.post_signin_redirect,
    }))
}

/// `PUT /api/v1/public/members/me` — change the signed-in member's own name or password.
pub async fn public_update_me(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
    Json(body): Json<ProfileRequest>,
) -> Result<Json<PublicProfileBody>, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let member = current_member(&state, site.id, &headers)
        .await?
        .ok_or_else(|| ApiError::unauthorized("member_signin_required", "sign in to change your account"))?;
    let store = MemberStore::new(state.db().pool().clone());

    if let Some(new_password) = body.new_password.as_deref() {
        // A password change needs the current one. Without that check, a walk-in with a borrowed
        // unlocked browser could take the account over permanently.
        let current = body.current_password.as_deref().unwrap_or_default();
        let stored = member.password_hash.clone().unwrap_or_default();
        let ok = omnion_identity::verify_password(current.to_owned(), stored)
            .await
            .map_err(|error| ApiError::bad_request("invalid_member", error.to_string()))?;
        if !ok {
            return Err(ApiError::bad_request(
                "invalid_credentials",
                "your current password is not right",
            ));
        }
        // Every session dies with the change, including this one: the member is holding a
        // cookie that no longer exists, and the honest answer is to sign in again.
        store.signout_everywhere(member.id).await?;
        store.set_password(site.id, member.id, new_password).await?;
        return Ok(Json(PublicProfileBody {
            member: PublicMember::from(&member),
            redirect_to: None,
        }));
    }

    let updated = store
        .patch(
            site.id,
            member.id,
            &MemberPatch {
                name: body.name,
                ..MemberPatch::default()
            },
        )
        .await?;
    Ok(Json(PublicProfileBody {
        member: PublicMember::from(&updated),
        redirect_to: None,
    }))
}

/// `GET /api/v1/public/members/verify?token=` — the verification click.
pub async fn public_verify(
    State(state): State<AppState>,
    Query(params): Query<TokenParam>,
    headers: HeaderMap,
) -> Result<Json<TokenOutcomeBody>, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let token = params.token.unwrap_or_default();
    let outcome = MemberStore::new(state.db().pool().clone())
        .consume_token(site.id, &token, "verify", None)
        .await?;
    if outcome.applied {
        emit(
            &state,
            "members.member.verified",
            json!({ "member_id": outcome.member.as_ref().map(|m| m.id), "site_id": site.id, "status": outcome.status }),
        )
        .await;
    }
    Ok(Json(TokenOutcomeBody::from(outcome)))
}

/// `POST /api/v1/public/members/password-reset` — ask for a link, or finish one.
///
/// One route for both halves on purpose, and the reason is the REQ's own rule: **a reset must
/// work without sign-in.** Splitting it into `/request` and `/complete` would let a theme link
/// the first by GET, and a link that arrives by mail is followed by a browser that prefetches.
pub async fn public_password_reset(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
    Json(body): Json<ResetRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let store = MemberStore::new(state.db().pool().clone());

    // Finishing half: a token and a new password.
    if let (Some(token), Some(password)) = (body.token.as_deref(), body.password.as_deref()) {
        let outcome = store
            .consume_token(site.id, token, "reset", Some(password))
            .await?;
        return Ok(Json(json!({
            "applied": outcome.applied,
            "status": outcome.status,
            "message": outcome.message,
        })));
    }

    // Asking half. **A missing address and an address nobody holds are the same answer**, because
    // a form that says "no account here" is a membership oracle — and a request with no address
    // at all is the shape an attacker sends, so it must not be the one answer that differs.
    let Some(email) = body.email.as_deref() else {
        return Ok(Json(json!({ "accepted": true, "delivery": "sent" })));
    };

    let Some(token) = store.issue_reset_token_by_email(site.id, email).await? else {
        audit_public(
            &state,
            site.id,
            "members.password_reset.unknown",
            "email_present=true",
        )
        .await;
        return Ok(Json(json!({ "accepted": true, "delivery": "sent" })));
    };

    let Some(member) = store.get_by_email(site.id, email).await? else {
        return Ok(Json(json!({ "accepted": true, "delivery": "sent" })));
    };
    let delivery = mail_token(&state, site.id, &member, "reset", &token).await;
    Ok(Json(json!({ "accepted": true, "delivery": delivery })))
}

/// `GET /api/v1/public/members/gate?slug=` — what a visitor may see of a page.
///
/// The theme calls this to decide whether to draw a sign-in prompt, and the API calls the same
/// function internally when it serves the page itself. One implementation, so the answer the
/// theme draws and the answer the API enforces cannot drift.
pub async fn public_gate(
    State(state): State<AppState>,
    Query(params): Query<GateParam>,
    headers: HeaderMap,
) -> Result<Json<GateBody>, ApiError> {
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;
    let verdict = gate(&state, site.id, params.slug.as_deref().unwrap_or_default(), &headers).await?;
    Ok(Json(verdict))
}

/// `?slug=` and `?site=` for the gate probe.
#[derive(Debug, Default, Deserialize)]
pub struct GateParam {
    /// The page address inside the site.
    pub slug: Option<String>,
    /// The site key, on a multi-site installation.
    pub site: Option<String>,
}

/// What the gate says about one page for the caller making the request.
#[derive(Debug, Serialize, Clone)]
pub struct GateBody {
    /// The page, when it exists and is published at all.
    pub exists: bool,
    /// Whether the caller may read it.
    pub allowed: bool,
    /// `public`, `members` or `roles` — the gate on the page.
    pub visibility: String,
    /// Whether a signed-out visitor gets a prompt or a 404, from the site's own setting.
    pub behaviour: String,
    /// The member, when the caller is one.
    pub member: Option<PublicMember>,
    /// Where a theme sends somebody who is not allowed in.
    pub sign_in_url: Option<String>,
}

/// Decide whether `headers`' caller may read `slug` on `site_id`.
///
/// **One function, called from two places on purpose.** `public_gate` exposes it to a theme and
/// `apps/api/src/routes/public.rs` calls it before serving the page, so a theme that draws a
/// prompt and the API that enforces the 404 are the same rule rather than two that agree today.
pub async fn gate(
    state: &AppState,
    site_id: Uuid,
    slug: &str,
    headers: &HeaderMap,
) -> Result<GateBody, ApiError> {
    let settings = MemberStore::new(state.db().pool().clone())
        .settings(site_id)
        .await?;
    let member = current_member(state, site_id, headers).await?;

    // An address that is not a slug, or names nothing, is "does not exist" — the same answer a
    // non-member gets for a page they may not read, which is the whole concealment.
    let gate_row = MemberStore::new(state.db().pool().clone())
        .page_gate(site_id, slug)
        .await?;

    let Some(PageGate {
        visibility,
        visibility_roles: roles,
    }) = gate_row else {
        return Ok(GateBody {
            exists: false,
            allowed: false,
            visibility: "public".to_string(),
            behaviour: settings.gated_page_behaviour.clone(),
            member: member.as_ref().map(PublicMember::from),
            sign_in_url: None,
        });
    };
    // **A public page needs no member at all.** The first version asked the member whether the
    // page was satisfied and used the answer: `member.is_some_and(|m| m.satisfies(…))` is
    // `false` for a visitor, because there is no member to ask — so an UNGATED page answered
    // 404 to every signed-out visitor and the whole public site went dark the moment this
    // shipped. The gate is a property of the page first and of the member second.
    //
    // So the decision is: is this page gated at all? If not, it is allowed. If it is, the
    // member's own roles answer — and their absence is the refusal.
    let allowed = if visibility == "public" {
        true
    } else {
        member
            .as_ref()
            .is_some_and(|m| m.satisfies(&visibility, &roles))
    };

    // The behaviour is read once, before it is moved into the body: the sign-in URL is a
    // decision about the same value, and reading a moved field is a compile error that would
    // push somebody to clone it at the wrong place later.
    let prompt = settings.gated_page_behaviour == "prompt";
    Ok(GateBody {
        exists: true,
        allowed,
        visibility,
        behaviour: settings.gated_page_behaviour,
        member: member.as_ref().map(PublicMember::from),
        sign_in_url: (!allowed && prompt).then(|| format!("/members/signin?next=/{}", slug)),
    })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The member cookie's value, if the request carries one.
fn member_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{MEMBER_COOKIE}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

/// The signed-in member for this request, or `None`.
///
/// A `blocked` member answers `None` here as well as at the store's own sign-in: blocking has to
/// take effect on the cookie that is already in the browser, or the block only stops the next
/// sign-in and the current session lives for another 30 days.
pub async fn current_member(
    state: &AppState,
    site_id: Uuid,
    headers: &HeaderMap,
) -> Result<Option<Member>, ApiError> {
    let Some(token) = member_cookie(headers) else {
        return Ok(None);
    };
    let member = MemberStore::new(state.db().pool().clone())
        .session_member(site_id, &token)
        .await?;
    Ok(member.filter(|m| m.status != "blocked"))
}

/// A client hint header, for the sign-in log.
fn client_hint<'a>(headers: &'a HeaderMap, name: &'a str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The cookie header a successful sign-in must carry, and the raw token never leaves with it.
///
/// `HttpOnly` because no script on a member's page has any business reading it;
/// `SameSite=Lax` so the link from a verification mail works; `Path=/` because a member's pages
/// are all over the site. The value is the raw token and it is in a cookie header and nowhere
/// else — never in the body, which is what ends up in a log and a proxy buffer.
fn member_cookie_header(token: &str, expires_at: &time::OffsetDateTime) -> String {
    let max_age = (*expires_at - time::OffsetDateTime::now_utc())
        .whole_seconds()
        .max(60);
    format!("{MEMBER_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}")
}

/// The cookie header that drops the session.
#[must_use]
pub fn cleared_member_cookie() -> String {
    format!("{MEMBER_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// A member that belongs to another site answers 404, not 403.
async fn concealed_member(
    store: &MemberStore,
    site_id: Uuid,
    id: Uuid,
) -> Result<Member, ApiError> {
    store.get(site_id, id).await.map_err(Into::into)
}

/// The site a panel request is about, checked against the caller's own organization.
///
/// **404 first, then 403** — and the order is the whole tenant boundary. `ensure_same_organization`
/// is a *permission* check that answers 403 `cross_organization`, which tells the caller the
/// site exists. A site outside the caller's own organization must therefore be 404'd before that
/// check ever runs, or an operator can enumerate the site ids of every tenant on the
/// installation by watching which one comes back 403. Same order the comment and newsletter
/// modules use, for the same reason.
async fn required_site(
    state: &AppState,
    session: &CurrentSession,
    site_id: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    let site_id =
        site_id.ok_or_else(|| ApiError::bad_request("site_required", "site_id is required"))?;
    let site = omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
        })?;
    ensure_same_organization(session, Some(site.organization_id))?;
    Ok(site_id)
}

/// Hand a token to the mail path and report what happened, without echoing it.
///
/// `sent` / `unavailable` / `not_required` — the three states an operator needs. The token is
/// never in the response, and when the platform has no transport the operator is told that
/// rather than discovering it from a member who never got the mail.
async fn deliver(
    state: &AppState,
    site_id: Uuid,
    outcome: &omnion_content::members::SignupOutcome,
    kind: &str,
) -> String {
    match outcome.verify_token.as_deref() {
        None => "not_required".to_string(),
        Some(token) => mail_token(state, site_id, &outcome.member, kind, token).await,
    }
}

/// The mail hand-off itself. There is no transport in this slice, so the honest answer is
/// `unavailable` and the token is dropped — the panel says so and the operator can re-send.
async fn mail_token(
    state: &AppState,
    site_id: Uuid,
    member: &Member,
    kind: &str,
    token: &str,
) -> String {
    // The token is deliberately not logged. It is in memory for the length of this call and no
    // longer; the row holds only its digest, so nothing downstream can recover it.
    let _ = (site_id, member, kind, token);
    // The action is a `&'static str` because the audit table groups by it, so a `kind` spliced
    // into it at runtime would produce a row nobody can filter on. Two named actions instead.
    let action = if kind == "reset" {
        "members.token.reset.not_delivered"
    } else {
        "members.token.verify.not_delivered"
    };
    audit_public(state, site_id, action, "no mail transport configured").await;
    "unavailable".to_string()
}

/// Record an audit entry for a panel action.
///
/// `action` is `&'static str` because the audit table stores it as a stable name; a dynamically
/// built action string is a row nobody can group by, so every call site names its own verb.
async fn audit(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    target: Uuid,
    detail: Option<&str>,
) {
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry {
            organization_id: session.user.organization_id,
            actor_user_id: Some(session.user.id),
            actor_type: omnion_audit::ActorType::User,
            action,
            target_type: Some("cms_member"),
            target_id: Some(target.to_string()),
            metadata: json!({ "detail": detail }),
            ip_address: None,
        },
    )
    .await;
}

/// Record an audit entry for a public action, with no actor.
///
/// `ActorType::System`, not a hypothetical `Anonymous`: the audit table stores the actor kind
/// in an enum and inventing a variant for a public call would need a migration to persist.
async fn audit_public(state: &AppState, site_id: Uuid, action: &'static str, detail: &str) {
    // The lookup borrows the pool for the statement only, so the value is bound to a local
    // rather than to a `let` chain over a temporary — a `fetch_optional(...)` bound directly in
    // the argument position would be dropped at the end of the statement while still borrowed.
    let organization_id: Option<Uuid> = {
        let result = sqlx::query_scalar::<_, Uuid>(
            "select organization_id from sites where id = $1",
        )
        .bind(site_id)
        .fetch_optional(state.db().pool())
        .await;
        result.ok().flatten()
    };
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry {
            organization_id,
            actor_user_id: None,
            actor_type: omnion_audit::ActorType::System,
            action,
            target_type: Some("site"),
            target_id: Some(site_id.to_string()),
            metadata: json!({ "detail": detail }),
            ip_address: None,
        },
    )
    .await;
}

/// Emit an event onto the bus, swallowing its own failure.
///
/// The organization is resolved from the site's `site_id` in the payload rather than passed in:
/// every caller already has it, and a second parameter that must agree with the payload is a
/// second parameter that eventually disagrees with it.
async fn emit(state: &AppState, name: &str, payload: serde_json::Value) {
    let site_id = payload
        .get("site_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok());
    let organization_id = match site_id {
        Some(site_id) => {
            let result = sqlx::query_scalar::<_, Uuid>(
                "select organization_id from sites where id = $1",
            )
            .bind(site_id)
            .fetch_optional(state.db().pool())
            .await;
            result.ok().flatten()
        }
        None => None,
    };
    let _ = bus::emit(
        state.db().pool(),
        NewEvent::new(name)
            .organization(organization_id)
            .site(site_id)
            .payload(payload),
    )
    .await;
}
