//! `/api/v1/organizations/{id}/members` and `/invitations` — the tenant's own people
//! (docs/requests/REQ-005, slice 1).
//!
//! `tenancy.rs` owns the organization row and its sites; this module owns everything that
//! answers "who belongs here" — the membership table, the invitations that grow it, and the
//! switcher's two session-scoped routes under `/api/v1/me`.
//!
//! Isolation is the point: every handler resolves the organization from the caller's
//! memberships, never from the request body, and a row that belongs to another tenant is
//! `404 organization_not_found` rather than `403` — the second answer would confirm that the
//! id exists.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::IdentityError;
use omnion_identity::memberships::{
    self, Membership, MembershipChanges, NewInvitation, NewMembership,
};
use omnion_identity::organizations::Organization;
use omnion_identity::users;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration as StdDuration, Instant};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// Window the invitation-preview rate limit counts over.
const PREVIEW_RATE_WINDOW: StdDuration = StdDuration::from_secs(300);

/// Invitation previews one caller may ask for inside [`PREVIEW_RATE_WINDOW`].
const PREVIEW_RATE_BUDGET: u64 = 20;

/// How many distinct caller buckets the preview limiter remembers before it prunes.
const PREVIEW_RATE_BUCKETS: usize = 4_096;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// One row of the Members tab.
#[derive(Debug, Serialize)]
pub struct MemberBody {
    /// Membership id.
    pub id: Uuid,
    /// The account.
    pub user_id: Uuid,
    /// Display name.
    pub display_name: String,
    /// Address, as stored.
    pub email: String,
    /// Account status (`active`, `invited`, `disabled`).
    pub user_status: String,
    /// Membership status (`active`, `invited`, `suspended`).
    pub status: String,
    /// Whether this is the account's home organization.
    pub is_primary: bool,
    /// When the membership became real.
    pub joined_at: Option<OffsetDateTime>,
    /// When the account was last seen in a session.
    pub last_active_at: Option<OffsetDateTime>,
    /// Roles the account holds in this organization, as `{id, name, key}`.
    pub roles: Vec<RoleChipBody>,
}

/// A role an account holds inside the organization.
#[derive(Debug, Clone, Serialize)]
pub struct RoleChipBody {
    /// Role id.
    pub id: Uuid,
    /// Role key.
    pub key: String,
    /// Role name.
    pub name: String,
}

/// Response body of `GET /api/v1/organizations/{id}/members`.
#[derive(Debug, Serialize)]
pub struct MembersResponse {
    /// The organization the members belong to.
    pub organization_id: Uuid,
    /// Its members, primary first.
    pub members: Vec<MemberBody>,
}

/// One invitation row.
#[derive(Debug, Serialize)]
pub struct InvitationBody {
    /// Invitation id.
    pub id: Uuid,
    /// Address it was sent to.
    pub email: String,
    /// Role it grants.
    pub role_id: Option<Uuid>,
    /// Role name, when the role still exists.
    pub role_name: Option<String>,
    /// Who sent it.
    pub invited_by: Option<Uuid>,
    /// Inviter display name, when the account still exists.
    pub invited_by_name: Option<String>,
    /// `pending`, `accepted`, `revoked` or `expired`.
    pub status: String,
    /// Personal message.
    pub message: String,
    /// When the token stops working, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Who accepted it.
    pub accepted_by: Option<Uuid>,
    /// When it was accepted, RFC 3339.
    pub accepted_at: Option<OffsetDateTime>,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response body of `GET /api/v1/organizations/{id}/invitations`.
#[derive(Debug, Serialize)]
pub struct InvitationsResponse {
    /// The organization the invitations are for.
    pub organization_id: Uuid,
    /// Invitations, newest first.
    pub invitations: Vec<InvitationBody>,
}

/// Response body of `POST /api/v1/organizations/{id}/invitations`.
#[derive(Debug, Serialize)]
pub struct InvitationCreatedResponse {
    /// The invitation row.
    pub invitation: InvitationBody,
    /// The raw token — returned exactly once, and never readable again.
    pub token: String,
    /// The link the inviter sends to the invitee.
    pub accept_url: String,
}

/// One membership of the caller — the switcher's row.
#[derive(Debug, Serialize)]
pub struct AccountOrganizationBody {
    /// Organization id.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// Stable handle.
    pub slug: String,
    /// Organization status.
    pub organization_status: String,
    /// Membership status.
    pub membership_status: String,
    /// Whether this is the caller's home organization.
    pub is_primary: bool,
    /// Roles held there, as `{id, name, key}`.
    pub roles: Vec<RoleChipBody>,
}

/// Response body of `GET /api/v1/me/organizations`.
#[derive(Debug, Serialize)]
pub struct MyOrganizationsResponse {
    /// The organization the session is working in (`null` for a platform account).
    pub current_organization_id: Option<Uuid>,
    /// Every organization the caller belongs to, primary first.
    pub organizations: Vec<AccountOrganizationBody>,
}

/// Response body of `POST /api/v1/me/organization`.
#[derive(Debug, Serialize)]
pub struct SwitchOrganizationResponse {
    /// The organization the session now works in.
    pub organization_id: Uuid,
    /// Its display name.
    pub name: String,
}

/// Public preview of an invitation (`GET /api/v1/invitations/{token}`).
#[derive(Debug, Serialize)]
pub struct InvitationPreviewResponse {
    /// The organization inviting.
    pub organization_name: String,
    /// The organization slug.
    pub organization_slug: String,
    /// Who sent the invitation.
    pub invited_by_name: Option<String>,
    /// The role the invitee receives.
    pub role_name: Option<String>,
    /// The address the invitation was sent to (masked: `a***@example.com`).
    pub email_masked: String,
    /// When the token stops working, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Whether the token can still be presented.
    pub usable: bool,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/organizations/{id}/members`.
#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    /// The account to add.
    pub user_id: Uuid,
    /// Membership status; `active` when absent.
    #[serde(default)]
    pub status: Option<String>,
    /// Whether the membership becomes the account's home organization.
    #[serde(default)]
    pub is_primary: Option<bool>,
}

/// `PATCH /api/v1/organizations/{id}/members/{user_id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateMemberRequest {
    /// New membership status.
    #[serde(default)]
    pub status: Option<String>,
    /// Promote or demote the membership to the account's home organization.
    #[serde(default)]
    pub is_primary: Option<bool>,
}

impl UpdateMemberRequest {
    fn changes(self) -> MembershipChanges {
        MembershipChanges {
            status: self.status,
            is_primary: self.is_primary,
        }
    }
}

/// `POST /api/v1/organizations/{id}/invitations`.
#[derive(Debug, Deserialize)]
pub struct CreateInvitationRequest {
    /// Address to invite.
    pub email: String,
    /// Role granted on acceptance.
    #[serde(default)]
    pub role_id: Option<Uuid>,
    /// Personal message (≤ 400 characters).
    #[serde(default)]
    pub message: Option<String>,
}

/// `POST /api/v1/invitations/{token}/accept` — the sign-up path.
#[derive(Debug, Deserialize)]
pub struct AcceptInvitationRequest {
    /// When the caller is not signed in: the account to create.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Password for the new account (≥ 12 characters, the platform's sign-up policy).
    #[serde(default)]
    pub password: Option<String>,
}

/// `POST /api/v1/me/organization`.
#[derive(Debug, Deserialize)]
pub struct SwitchOrganizationRequest {
    /// The organization to work in; must be one the caller belongs to.
    pub organization_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Membership handlers
// ---------------------------------------------------------------------------------------------

/// List the members of one organization.
pub async fn list_members(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<MembersResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    Ok(Json(MembersResponse {
        organization_id: organization.id,
        members: members_of(&state, organization.id).await?,
    }))
}

/// Add an account to the organization.
pub async fn add_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<MemberBody>), ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    let membership = memberships::add_member(
        state.db().pool(),
        NewMembership {
            organization_id: organization.id,
            user_id: body.user_id,
            status: body.status.unwrap_or_else(|| "active".to_owned()),
            is_primary: body.is_primary.unwrap_or(false),
        },
    )
    .await?;

    let member = member_body(&state, organization.id, membership.user_id)
        .await?
        .ok_or_else(member_not_found)?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.joined")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "user_id": membership.user_id,
                "status": membership.status,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.member.added")
            .target("user", membership.user_id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "status": membership.status,
                "is_primary": membership.is_primary,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(member)))
}

/// Change a membership's status, or promote/demote it as the account's home organization.
pub async fn update_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, user_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
    Json(body): Json<UpdateMemberRequest>,
) -> Result<Json<MemberBody>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    if find_membership(&state, organization.id, user_id)
        .await?
        .is_none()
    {
        return Err(member_not_found());
    }

    let membership = memberships::update_membership(
        state.db().pool(),
        organization.id,
        user_id,
        &body.changes(),
    )
    .await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.status_changed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "user_id": user_id,
                "status": membership.status,
                "is_primary": membership.is_primary,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.member.updated")
            .target("user", user_id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "status": membership.status,
                "is_primary": membership.is_primary,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    let member = member_body(&state, organization.id, user_id)
        .await?
        .ok_or_else(member_not_found)?;

    Ok(Json(member))
}

/// Remove a member from the organization.
///
/// The last primary member cannot be removed: the organization would be left with nobody whose
/// home is this tenant, and every scoped query would answer for an account that has no home.
pub async fn remove_member(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, user_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    let membership = find_membership(&state, organization.id, user_id)
        .await?
        .ok_or_else(member_not_found)?;

    if membership.is_primary {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "last_owner",
            "this account's home organization is the one you are removing — switch it to another \
             organization first, or promote another member",
        ));
    }

    memberships::remove_member(state.db().pool(), organization.id, user_id).await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.removed")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "user_id": user_id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.member.removed")
            .target("user", user_id.to_string())
            .metadata(json!({ "organization_id": organization.id }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Invitation handlers
// ---------------------------------------------------------------------------------------------

/// List the invitations of one organization.
pub async fn list_invitations(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<InvitationsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let invitations = memberships::list_invitations(state.db().pool(), organization.id).await?;

    let bodies = invitation_bodies(state.db().pool(), invitations).await;
    Ok(Json(InvitationsResponse {
        organization_id: organization.id,
        invitations: bodies,
    }))
}

/// Invite an address into the organization.
///
/// Two refusals are specific on purpose: an address that already belongs here is answered with
/// the member's name, and an address that already holds a live invitation is answered with that
/// invitation's id and expiry instead of mailing it twice.
pub async fn create_invitation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<CreateInvitationRequest>,
) -> Result<(StatusCode, Json<InvitationCreatedResponse>), ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let decision = invite_policy_decision(&state, &current, organization.id).await?;

    if let Some(member_id) =
        memberships::address_is_member(state.db().pool(), organization.id, &body.email).await?
    {
        let name = member_display_name(state.db().pool(), member_id)
            .await
            .unwrap_or_else(|| body.email.clone());
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_member",
            format!("{name} is already a member of this organization"),
        )
        .with_details(json!({ "user_id": member_id })));
    }

    if let Some(role_id) = body.role_id {
        role_belongs_to_organization(state.db().pool(), role_id, organization.id).await?;
    }

    let created = match memberships::create_invitation(
        state.db().pool(),
        NewInvitation {
            organization_id: organization.id,
            email: body.email.clone(),
            role_id: body.role_id,
            invited_by: Some(current.user.id),
            message: body.message.unwrap_or_default(),
            expires_at: None,
            queued: decision.queues,
        },
    )
    .await
    {
        Ok(created) => created,
        // The unique index caught a second live invitation for the same address: answer with
        // the row that already exists, so the panel can show it instead of duplicating it.
        Err(IdentityError::InvitationAlreadyPending(_)) => {
            let pending = memberships::find_pending_invitation(
                state.db().pool(),
                organization.id,
                &body.email,
            )
            .await
            .ok()
            .flatten();
            let mut refusal = ApiError::new(
                StatusCode::CONFLICT,
                "invitation_already_pending",
                "this address already has a pending invitation in this organization",
            );
            if let Some(pending) = pending {
                refusal = refusal.with_details(json!({
                    "invitation_id": pending.id,
                    "expires_at": pending.expires_at,
                }));
            }
            return Err(refusal);
        }
        Err(other) => return Err(ApiError::from(other)),
    };

    let invitation = invitation_bodies(state.db().pool(), vec![created.invitation.clone()])
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable!("the invitation was just created"));

    bus::emit(
        state.db().pool(),
        NewEvent::new(if decision.queues {
            "organization.member.invitation_queued"
        } else {
            "organization.member.invited"
        })
        .organization(organization.id)
        .actor(current.user.id)
        .payload(json!({
            "organization_id": organization.id,
            "invitation_id": created.invitation.id,
            // The address never leaves the tenant in an event payload: an integration that
            // subscribes to the tenant's events must not learn who was invited.
            "email_masked": mask_email(&created.invitation.email),
            "role_id": created.invitation.role_id,
            "expires_at": created.invitation.expires_at,
            "invite_policy": decision.policy,
        })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(
            current.user.id,
            if decision.queues {
                "organization.member.invitation_queued"
            } else {
                "organization.member.invited"
            },
        )
        .target("organization_invitation", created.invitation.id.to_string())
        .metadata(json!({
            "organization_id": organization.id,
            "email_masked": mask_email(&created.invitation.email),
            "invite_policy": decision.policy,
        }))
        .ip_address(address.as_text())
        .organization(organization.id),
    )
    .await?;

    // A queued invitation has no working link, so it has no token to hand out. Returning one
    // would give the invitee a code that answers "waiting for an owner" — a dead credential that
    // looks alive, and a second one to lose. The owner gets the token when they release it.
    if decision.queues {
        return Ok((
            StatusCode::ACCEPTED,
            Json(InvitationCreatedResponse {
                invitation,
                token: String::new(),
                accept_url: String::new(),
            }),
        ));
    }

    Ok((
        StatusCode::CREATED,
        Json(InvitationCreatedResponse {
            invitation,
            token: created.token.clone(),
            accept_url: format!("/invite/{}", created.token),
        }),
    ))
}

/// The invitations waiting for an owner, oldest first.
///
/// The same rows the ordinary listing already returns, in the order a queue is worked: the
/// `owner_approval` policy is only meaningful if the panel can show what is piling up, and a
/// filter on the client would need the whole (unbounded) listing to find it.
pub async fn list_queued_invitations(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<InvitationsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let invitations =
        memberships::list_queued_invitations(state.db().pool(), organization.id).await?;

    Ok(Json(InvitationsResponse {
        organization_id: organization.id,
        invitations: invitation_bodies(state.db().pool(), invitations).await,
    }))
}

/// Release a queued invitation: it becomes live and its single-use link is returned *once*.
///
/// Only the tenant's owner may do this. A manager who queued an invitation cannot release it —
/// that is the whole difference between `owner_approval` and `self_serve`, and letting the
/// queuer approve would make the queue advisory.
///
/// The link returned here is minted by the release, not recovered from the create: the queued
/// row's token was never shown to anybody, and the stored hash is one-way. So the old link never
/// existed and this is the only time a working one is ever visible.
pub async fn approve_invitation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, invitation_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
) -> Result<(StatusCode, Json<InvitationCreatedResponse>), ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    if !caller_is_organization_owner(&state, &current, organization.id).await? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_an_organization_owner",
            "only an owner of this organization can release a queued invitation",
        )
        .with_details(json!({ "invite_policy": "owner_approval" })));
    }

    let released = memberships::approve_invitation(state.db().pool(), invitation_id, current.user.id)
        .await?;

    let Some((released, raw_token)) = released else {
        let invitations = memberships::list_invitations(state.db().pool(), organization.id).await?;
        let existing = invitations
            .into_iter()
            .find(|invitation| invitation.id == invitation_id)
            .ok_or_else(invitation_not_found)?;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "invitation_not_queued",
            format!(
                "this invitation is {}, not waiting for approval",
                existing.status
            ),
        ));
    };

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.invitation_released")
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "organization_id": organization.id,
                "invitation_id": released.id,
                "email_masked": mask_email(&released.email),
                "role_id": released.role_id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.member.invitation_released")
            .target("organization_invitation", released.id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "email_masked": mask_email(&released.email),
                "invited_by": released.invited_by,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    let invitation = invitation_bodies(state.db().pool(), vec![released])
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable!("the invitation was just released"));

    Ok((
        StatusCode::OK,
        Json(InvitationCreatedResponse {
            invitation,
            token: raw_token.clone(),
            accept_url: format!("/invite/{raw_token}"),
        }),
    ))
}

/// Revoke a pending invitation.
pub async fn revoke_invitation(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((organization_id, invitation_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    let invitations = memberships::list_invitations(state.db().pool(), organization.id).await?;
    let invitation = invitations
        .into_iter()
        .find(|invitation| invitation.id == invitation_id)
        .ok_or_else(invitation_not_found)?;

    if !memberships::revoke_invitation(state.db().pool(), invitation.id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "invitation_already_decided",
            format!("this invitation is already {}", invitation.status),
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.member.invitation_revoked")
            .target("organization_invitation", invitation.id.to_string())
            .metadata(json!({
                "organization_id": organization.id,
                "email_masked": mask_email(&invitation.email),
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Public preview of an invitation link. Rate-limited and deliberately uninformative: a token
/// that is not usable answers with the same shape and `usable: false`, so the page cannot be
/// used to discover whether an organization exists.
pub async fn preview_invitation(
    State(state): State<AppState>,
    address: ClientAddress,
    Path(token): Path<String>,
) -> Result<Json<InvitationPreviewResponse>, ApiError> {
    if !preview_limiter().allows(&preview_key(address.0), PREVIEW_RATE_BUDGET, Instant::now()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many invitation previews — try again in a few minutes",
        ));
    }

    let invitation = memberships::find_invitation_by_token(state.db().pool(), &token)
        .await?
        .ok_or_else(invitation_not_found)?;

    let organization = omnion_identity::organizations::find_organization(
        state.db().pool(),
        invitation.organization_id,
    )
    .await?
    .ok_or_else(invitation_not_found)?;

    let role_name = match invitation.role_id {
        Some(role_id) => role_name(state.db().pool(), role_id).await,
        None => None,
    };
    let invited_by_name = match invitation.invited_by {
        Some(user_id) => member_display_name(state.db().pool(), user_id).await,
        None => None,
    };

    Ok(Json(InvitationPreviewResponse {
        organization_name: organization.name,
        organization_slug: organization.slug,
        invited_by_name,
        role_name,
        email_masked: mask_email(&invitation.email),
        expires_at: invitation.expires_at,
        usable: invitation.is_usable(OffsetDateTime::now_utc()),
    }))
}

/// Accept an invitation. The caller may be signed in already, or may sign up in the same
/// request — the account it creates is the one that joins.
pub async fn accept_invitation(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    address: ClientAddress,
    Path(token): Path<String>,
    Json(body): Json<AcceptInvitationRequest>,
) -> Result<axum::response::Response, ApiError> {
    let invitation = memberships::find_invitation_by_token(state.db().pool(), &token)
        .await?
        .ok_or_else(invitation_not_found)?;

    // The seat ceiling (REQ-005, slice 3) is checked here, *before* the acceptance — and before
    // a sign-up account is created. The REQ is explicit that enforcement belongs to acceptance
    // rather than to inviting ("an invitation is a request, the plan is charged for people who
    // have joined"), and checking early means a refusal does not leave a brand new account
    // behind that holds no membership and cannot get in.
    super::tenancy_limits::guard_seat_limit(&state, invitation.organization_id).await?;

    // A signed-in caller accepts as itself; a signed-out one signs up as the address the
    // invitation was sent to. `Option<CurrentSession>` is not an extractor (an unauthenticated
    // request is a rejection, not a value), so the session is resolved here instead — and a
    // stale cookie simply reads as signed out rather than failing the acceptance.
    let current = CurrentSession::resolve(&state, &headers).await.ok();

    // A sign-up inside the acceptance is a first-class sign-in: it earns the same session
    // cookie a password sign-in would, so the reader lands on the organization already signed
    // in instead of being asked to sign in again with an address they only half know.
    let (user_id, created_account) = match current {
        Some(session) => (session.user.id, None),
        None => {
            // A signed-out accept creates the account the invitation was addressed to. The
            // address is locked to the invitation's own, so a link cannot be used to sign
            // somebody else up.
            let password = body.password.ok_or_else(|| {
                ApiError::bad_request(
                    "password_required",
                    "create an account to accept: a password is required",
                )
            })?;
            if password.chars().count() < MIN_SIGNUP_PASSWORD_LENGTH {
                return Err(ApiError::bad_request(
                    "weak_password",
                    format!("password must be at least {MIN_SIGNUP_PASSWORD_LENGTH} characters"),
                ));
            }
            let display_name = body.display_name.unwrap_or_else(|| {
                invitation
                    .email
                    .split('@')
                    .next()
                    .unwrap_or("New member")
                    .to_owned()
            });

            let user = users::create_user(
                state.db().pool(),
                users::NewUser {
                    email: invitation.email.clone(),
                    password,
                    display_name,
                    organization_id: None,
                },
            )
            .await?;
            (user.id, Some(user))
        }
    };

    let accepted = memberships::accept_invitation(state.db().pool(), &token, user_id).await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("organization.member.joined")
            .organization(accepted.organization_id)
            .actor(user_id)
            .payload(json!({
                "organization_id": accepted.organization_id,
                "user_id": user_id,
                "via": "invitation",
                "invitation_id": accepted.id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(user_id, "organization.member.joined")
            .target("organization", accepted.organization_id.to_string())
            .metadata(json!({
                "organization_id": accepted.organization_id,
                "via": "invitation",
            }))
            .organization(accepted.organization_id),
    )
    .await?;

    // The offered role is granted at organization scope in the same request, so "accept an
    // invitation with the Editor role" really does grant it: without this the role would be
    // stored on the invitation and never applied, which is the kind of half-feature the
    // acceptance criteria forbid.
    if let Some(role_id) = accepted.role_id {
        omnion_permissions::bindings::grant_if_missing(
            state.db().pool(),
            omnion_permissions::model::NewBinding {
                role_id,
                user_id,
                scope: omnion_permissions::Scope::Organization {
                    organization_id: accepted.organization_id,
                },
                granted_by: None,
                expires_at: None,
            },
        )
        .await
        .map_err(ApiError::from)?;
    }

    let organization = omnion_identity::organizations::find_organization(
        state.db().pool(),
        accepted.organization_id,
    )
    .await?
    .ok_or_else(invitation_not_found)?;

    let payload = Json(AcceptResponse {
        organization_id: accepted.organization_id,
        organization_name: organization.name,
        user_id,
    });

    // A sign-up that happened inside the acceptance answers with the session cookie it earned.
    let Some(account) = created_account else {
        return Ok(payload.into_response());
    };
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut response = crate::routes::auth::start_session_with_body(
        &state,
        &account,
        user_agent,
        address.as_text(),
        vec!["invitation".to_owned()],
        payload.0,
    )
    .await?;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

/// Response body of `POST /api/v1/invitations/{token}/accept`.
#[derive(Debug, Serialize)]
pub struct AcceptResponse {
    /// The organization the account now belongs to.
    pub organization_id: Uuid,
    /// Its display name.
    pub organization_name: String,
    /// The account that accepted.
    pub user_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Switcher handlers
// ---------------------------------------------------------------------------------------------

/// The caller's own organizations — the switcher's list.
///
/// A platform account (no membership at all) gets an empty list with a `null` current
/// organization, which is how the panel tells "belongs to no tenant" from "belongs to one".
pub async fn my_organizations(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<MyOrganizationsResponse>, ApiError> {
    let rows = memberships::list_account_memberships(state.db().pool(), current.user.id).await?;

    let organizations = rows
        .into_iter()
        .map(|row| AccountOrganizationBody {
            organization_id: row.organization_id,
            name: row.organization_name,
            slug: row.organization_slug,
            organization_status: row.organization_status,
            membership_status: row.status,
            is_primary: row.is_primary,
            roles: row
                .roles
                .iter()
                .filter_map(|role| role.split_once(':'))
                .map(|(key, name)| RoleChipBody {
                    id: Uuid::nil(),
                    key: key.to_owned(),
                    name: name.to_owned(),
                })
                .collect(),
        })
        .collect();

    Ok(Json(MyOrganizationsResponse {
        current_organization_id: current.user.organization_id,
        organizations,
    }))
}

/// Switch the session's organization.
///
/// The account must be a member: the store joins the organization when it is not, so a switch
/// that names a tenant the account cannot be a member of is refused here rather than silently
/// granting a membership.
pub async fn switch_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SwitchOrganizationRequest>,
) -> Result<Json<SwitchOrganizationResponse>, ApiError> {
    let organization =
        omnion_identity::organizations::find_organization(state.db().pool(), body.organization_id)
            .await?
            .ok_or_else(invitation_not_found)?;

    let memberships_of_user =
        memberships::list_account_memberships(state.db().pool(), current.user.id).await?;
    let known = memberships_of_user
        .iter()
        .any(|row| row.organization_id == organization.id);
    if !known {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_a_member",
            "you are not a member of this organization",
        ));
    }
    if organization.status != "active" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "organization_suspended",
            format!(
                "this organization is {} — its owner has to reactivate it before you can work in it",
                organization.status
            ),
        ));
    }

    let membership =
        memberships::make_primary(state.db().pool(), organization.id, current.user.id).await?;
    debug_assert!(membership.is_primary);

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.switched")
            .target("organization", organization.id.to_string())
            .metadata(json!({
                "from": current.user.organization_id,
                "to": organization.id,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(Json(SwitchOrganizationResponse {
        organization_id: organization.id,
        name: organization.name,
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Shortest password an invited account may sign up with (the panel's sign-up rule).
const MIN_SIGNUP_PASSWORD_LENGTH: usize = 12;

/// Load an organization the caller may act on, or answer `404 organization_not_found`.
///
/// A platform account (no membership) may act on any organization; an account with a
/// membership may act only on the organizations it belongs to, and another tenant's id is
/// `404` — never `403`, which would confirm that the id exists.
///
/// Shared with the departments surface: every tenancy handler resolves the tenant through this
/// one function, so the isolation rule is written once rather than per route.
pub(crate) async fn organization_in_scope(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> Result<Organization, ApiError> {
    let organization =
        omnion_identity::organizations::find_organization(state.db().pool(), organization_id)
            .await?
            .ok_or_else(organization_not_found)?;

    let rows = memberships::list_account_memberships(state.db().pool(), current.user.id).await?;
    if rows
        .iter()
        .any(|row| row.organization_id == organization.id)
    {
        return Ok(organization);
    }

    // A platform account owns no tenant but works across them; the panel's first-run owner is
    // exactly this shape.
    if rows.is_empty() {
        return Ok(organization);
    }

    Err(organization_not_found())
}

/// Every member of one organization, with their account row and the roles they hold there.
async fn members_of(state: &AppState, organization_id: Uuid) -> Result<Vec<MemberBody>, ApiError> {
    let pool = state.db().pool();
    let members = memberships::list_members(pool, organization_id).await?;

    let roles: HashMap<Uuid, Vec<RoleChipBody>> = member_roles(pool, organization_id).await?;
    let accounts = account_rows(pool, &members).await?;

    Ok(members
        .into_iter()
        .filter_map(|membership| {
            let account = accounts.get(&membership.user_id)?;
            Some(MemberBody {
                id: membership.id,
                user_id: membership.user_id,
                display_name: account.display_name.clone(),
                email: account.email.clone(),
                user_status: account.status.clone(),
                status: membership.status,
                is_primary: membership.is_primary,
                joined_at: membership.joined_at,
                last_active_at: account.last_active_at,
                roles: roles.get(&membership.user_id).cloned().unwrap_or_default(),
            })
        })
        .collect())
}

/// The member row of one account, or `None` when the account is gone.
async fn member_body(
    state: &AppState,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<MemberBody>, ApiError> {
    Ok(members_of(state, organization_id)
        .await?
        .into_iter()
        .find(|member| member.user_id == user_id))
}

/// Load one membership or answer `404 member_not_found`.
async fn find_membership(
    state: &AppState,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Membership>, ApiError> {
    Ok(memberships::find_member(state.db().pool(), organization_id, user_id).await?)
}

/// Account columns the Members tab reads, in one query.
struct AccountRow {
    /// Display name.
    display_name: String,
    /// Address.
    email: String,
    /// Account status.
    status: String,
    /// Last session activity.
    last_active_at: Option<OffsetDateTime>,
}

async fn account_rows(
    pool: &sqlx::PgPool,
    members: &[Membership],
) -> Result<HashMap<Uuid, AccountRow>, ApiError> {
    let ids: Vec<Uuid> = members.iter().map(|member| member.user_id).collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<OffsetDateTime>)>(
        "select u.id, u.display_name, u.email, u.status, \
                (select max(s.last_seen_at) from sessions s where s.user_id = u.id \
                   and s.revoked_at is null) as last_active_at \
           from users u where u.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(store)?;

    Ok(rows
        .into_iter()
        .map(|(id, display_name, email, status, last_active_at)| {
            (
                id,
                AccountRow {
                    display_name,
                    email,
                    status,
                    last_active_at,
                },
            )
        })
        .collect())
}

/// The roles each member holds at organization scope inside this organization.
async fn member_roles(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
) -> Result<HashMap<Uuid, Vec<RoleChipBody>>, ApiError> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String)>(
        "select b.user_id, r.id, r.key, r.name \
           from role_bindings b \
           join roles r on r.id = b.role_id \
          where b.organization_id = $1 \
            and b.revoked_at is null \
            and (b.expires_at is null or b.expires_at > now()) \
          order by r.priority desc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(store)?;

    let mut by_user: HashMap<Uuid, Vec<RoleChipBody>> = HashMap::new();
    for (user_id, role_id, key, name) in rows {
        by_user.entry(user_id).or_default().push(RoleChipBody {
            id: role_id,
            key,
            name,
        });
    }
    Ok(by_user)
}

/// Turn invitations into their response shape, resolving the role and inviter names in two
/// queries instead of one per row.
async fn invitation_bodies(
    pool: &sqlx::PgPool,
    invitations: Vec<memberships::Invitation>,
) -> Vec<InvitationBody> {
    let role_ids: Vec<Uuid> = invitations
        .iter()
        .filter_map(|invitation| invitation.role_id)
        .collect();
    let inviter_ids: Vec<Uuid> = invitations
        .iter()
        .filter_map(|invitation| invitation.invited_by)
        .collect();

    let roles: HashMap<Uuid, String> = if role_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, String)>("select id, name from roles where id = any($1)")
            .bind(&role_ids)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    };
    let inviters: HashMap<Uuid, String> = if inviter_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, String)>("select id, display_name from users where id = any($1)")
            .bind(&inviter_ids)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    };

    invitations
        .into_iter()
        .map(|invitation| InvitationBody {
            id: invitation.id,
            email: invitation.email,
            role_id: invitation.role_id,
            role_name: invitation.role_id.and_then(|id| roles.get(&id).cloned()),
            invited_by: invitation.invited_by,
            invited_by_name: invitation
                .invited_by
                .and_then(|id| inviters.get(&id).cloned()),
            status: invitation.status,
            message: invitation.message,
            expires_at: invitation.expires_at,
            accepted_by: invitation.accepted_by,
            accepted_at: invitation.accepted_at,
            created_at: invitation.created_at,
        })
        .collect()
}

/// The name of a role, when it still exists.
async fn role_name(pool: &sqlx::PgPool, role_id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, String>("select name from roles where id = $1")
        .bind(role_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// The display name of an account, when it still exists.
async fn member_display_name(pool: &sqlx::PgPool, user_id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, String>("select display_name from users where id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// What the organization's invite policy says about *this* invitation.
///
/// The three policies are not three permissions, which is the point: `self_serve` needs no new
/// check at all, because the route's existing `organizations.manage` guard already is the rule.
/// Only `closed` (a refusal) and `owner_approval` (a queue) need anything computed here, and both
/// need the *inviter's* role as well as the policy — an owner inviting into their own tenant is
/// the one case where `owner_approval` cannot deadlock on itself.
struct InviteDecision {
    /// Whether the new row starts in the queue rather than as a live invitation.
    queues: bool,
    /// The policy that produced the decision, for the event, the audit row and the panel.
    policy: String,
}

/// Read `organization_settings.invite_policy` and apply it to this caller.
///
/// A `closed` tenant refuses by name and with the policy in `details`, so the panel can explain
/// *why* the button did nothing instead of showing a generic failure. The refusal happens before
/// the duplicate-member check on purpose: a closed tenant does not need to confirm that the
/// address exists to tell you it will not invite anybody.
async fn invite_policy_decision(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> Result<InviteDecision, ApiError> {
    let policy = omnion_identity::tenancy_limits::load_settings(state.db().pool(), organization_id)
        .await?
        .invite_policy;

    if policy == "closed" {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invitations_closed",
            "this organization is closed to new invitations — an owner has to open it again in \
             Settings first",
        )
        .with_details(json!({ "invite_policy": policy })));
    }

    let queues = policy == "owner_approval" && !caller_is_organization_owner(state, current, organization_id).await?;

    Ok(InviteDecision { queues, policy })
}

/// Whether the caller owns *this* tenant — the one fact that releases a queued invitation.
///
/// The seed binds `owner` at **platform** scope (`roles.organization_id is null`), so "is an
/// owner" is two questions: does the account hold an `owner` binding, and does that binding
/// reach this organization? Ignoring the second question would let the owner of tenant A release
/// tenant B's queue, which is the exact over-grant a per-tenant policy exists to prevent.
///
/// The scopes that reach a tenant are `global` (a platform operator) and `organization` *at this
/// tenant*; a `site`-scoped binding deliberately does not, because a site lead is not the owner
/// of the whole organization.
///
/// The last fallback is the account with no membership at all — the panel's first-run owner,
/// which `users.organization_id = null` and is therefore invisible to the membership check. It
/// is the same shape the `organization_in_scope` helper already treats as a platform account, so
/// the two agree instead of disagreeing about the same person.
async fn caller_is_organization_owner(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
) -> Result<bool, ApiError> {
    let role_id: Option<Uuid> = sqlx::query_scalar(
        "select id from roles where key = 'owner' and organization_id is null limit 1",
    )
    .fetch_optional(state.db().pool())
    .await
    .map_err(store)?;

    if let Some(role_id) = role_id {
        let reaches: bool = sqlx::query_scalar(
            "select exists (
                 select 1 from role_bindings
                  where subject_type = 'user' and subject_id = $1 and role_id = $3
                    and (expires_at is null or expires_at > now())
                    and (scope_type = 'global'
                         or (scope_type = 'organization' and organization_id = $2))
             )",
        )
        .bind(current.user.id)
        .bind(organization_id)
        .bind(role_id)
        .fetch_one(state.db().pool())
        .await
        .map_err(store)?;

        if reaches {
            return Ok(true);
        }
    }

    let memberships = memberships::list_account_memberships(state.db().pool(), current.user.id).await?;
    Ok(memberships.is_empty())
}

/// Refuse a role that belongs to another organization (or to the platform) as a tenant role.
async fn role_belongs_to_organization(
    pool: &sqlx::PgPool,
    role_id: Uuid,
    organization_id: Uuid,
) -> Result<(), ApiError> {
    let owner: Option<Uuid> = sqlx::query_scalar("select organization_id from roles where id = $1")
        .bind(role_id)
        .fetch_optional(pool)
        .await
        .map_err(store)?;
    match owner {
        Some(owner) if owner == organization_id => Ok(()),
        None => Err(ApiError::bad_request("role_not_found", "no such role")),
        Some(_) => Err(ApiError::bad_request(
            "role_out_of_scope",
            "this role belongs to another organization",
        )),
    }
}

/// Mask an address for events and audit rows: `ada.lovelace@example.com` → `a***@example.com`.
#[must_use]
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let head = local.chars().next().unwrap_or('*');
            format!("{head}***@{domain}")
        }
        None => "***".to_owned(),
    }
}

/// The bucket key the preview rate limit counts: the address when there is one, `anon`
/// otherwise, so an in-process test (which carries no connection info) is not thrown into
/// one shared bucket.
fn preview_key(address: Option<IpAddr>) -> String {
    address
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "anon".to_owned())
}

/// Fixed-window counter for the invitation preview, in memory.
#[derive(Debug, Default)]
struct PreviewLimiter {
    buckets: Mutex<HashMap<String, (Instant, u64)>>,
}

impl PreviewLimiter {
    fn allows(&self, key: &str, budget: u64, now: Instant) -> bool {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if buckets.len() > PREVIEW_RATE_BUCKETS {
            buckets.retain(|_, (started, _)| now.duration_since(*started) < PREVIEW_RATE_WINDOW);
        }

        let entry = buckets.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= PREVIEW_RATE_WINDOW {
            *entry = (now, 0);
        }
        entry.1 += 1;

        entry.1 <= budget
    }
}

fn preview_limiter() -> &'static PreviewLimiter {
    static LIMITER: OnceLock<PreviewLimiter> = OnceLock::new();
    LIMITER.get_or_init(PreviewLimiter::default)
}

/// Database failures become the same `500`/`503` the rest of the API answers with.
fn store(error: sqlx::Error) -> ApiError {
    omnion_identity::IdentityError::Database(error).into()
}

async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

fn member_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "member_not_found",
        "this account is not a member of the organization",
    )
}

fn invitation_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invitation_not_found",
        "this invitation link is not valid",
    )
}

fn organization_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "organization_not_found",
        "no such organization",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_masked_down_to_their_first_letter() {
        assert_eq!(mask_email("ada@example.com"), "a***@example.com");
        assert_eq!(mask_email("a@example.com"), "a***@example.com");
        assert_eq!(mask_email("not-an-address"), "***");
    }

    #[test]
    fn the_preview_limiter_counts_inside_its_window_and_then_forgets() {
        let limiter = PreviewLimiter::default();
        let now = Instant::now();

        for hit in 0..PREVIEW_RATE_BUDGET {
            assert!(
                limiter.allows("1.2.3.4", PREVIEW_RATE_BUDGET, now),
                "hit {hit}"
            );
        }
        assert!(
            !limiter.allows("1.2.3.4", PREVIEW_RATE_BUDGET, now),
            "the budget is spent after {PREVIEW_RATE_BUDGET} previews"
        );
        assert!(
            limiter.allows("5.6.7.8", PREVIEW_RATE_BUDGET, now),
            "another caller has its own bucket"
        );
        assert!(
            limiter.allows("1.2.3.4", PREVIEW_RATE_BUDGET, now + PREVIEW_RATE_WINDOW),
            "the bucket resets once the window passed"
        );
    }

    #[test]
    fn requests_without_a_connection_info_share_one_anonymous_bucket() {
        assert_eq!(preview_key(None), "anon");
        assert_eq!(
            preview_key(Some("127.0.0.1".parse().expect("ip"))),
            "127.0.0.1"
        );
    }

    #[test]
    fn member_requests_fold_into_change_sets() {
        let changes = UpdateMemberRequest {
            status: Some("suspended".to_owned()),
            is_primary: Some(false),
        }
        .changes();
        assert_eq!(changes.status.as_deref(), Some("suspended"));
        assert_eq!(changes.is_primary, Some(false));
        assert!(!changes.is_empty());
    }
}
