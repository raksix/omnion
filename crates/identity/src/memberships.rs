//! Organization memberships and invitations (docs/requests/REQ-005, slice 1).
//!
//! `users.organization_id` answers "where does this account work by default"; this module
//! answers "who belongs to this organization". The two are not the same: an account may belong
//! to several organizations, each membership carrying its own status, and only one of them is
//! the primary one the home column points at.
//!
//! Invitations are single-use tokens stored as a hash — the raw token is returned once, at
//! creation, and never again. Nothing in this module writes a mail or an event: the API layer
//! owns both, so the store stays a store (see `apps/api/src/routes/tenancy.rs`).

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::sessions::{generate_token, hash_token};
use crate::users::normalize_email;

/// Longest accepted organization key / module key — the schema shapes them like a permission key.
const MAX_KEY_LENGTH: usize = 64;

/// Membership statuses a row may carry.
pub const MEMBER_STATUSES: [&str; 3] = ["active", "invited", "suspended"];

/// Invitation statuses a row may carry.
pub const INVITATION_STATUSES: [&str; 4] = ["pending", "accepted", "revoked", "expired"];

/// Longest accepted personal message on an invitation.
pub const MAX_INVITATION_MESSAGE: usize = 400;

/// How long an invitation stays usable when the request does not say.
pub const DEFAULT_INVITATION_TTL_DAYS: i64 = 14;

/// A membership as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Membership {
    /// Primary key.
    pub id: Uuid,
    /// Organization the account belongs to.
    pub organization_id: Uuid,
    /// The account.
    pub user_id: Uuid,
    /// `active`, `invited` or `suspended`.
    pub status: String,
    /// Whether this is the account's home organization.
    pub is_primary: bool,
    /// When the membership became real (a pending invitation has none).
    pub joined_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// An invitation as stored — without the token, which is not stored at all.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Invitation {
    /// Primary key.
    pub id: Uuid,
    /// Organization the invitation is for.
    pub organization_id: Uuid,
    /// Address the invitation was sent to (normalized).
    pub email: String,
    /// Role the invitee receives on acceptance.
    pub role_id: Option<Uuid>,
    /// Hash of the single-use token.
    pub token_hash: String,
    /// Who sent it.
    pub invited_by: Option<Uuid>,
    /// `pending`, `accepted`, `revoked` or `expired`.
    pub status: String,
    /// Personal message the inviter wrote.
    pub message: String,
    /// When the token stops working.
    pub expires_at: OffsetDateTime,
    /// Who accepted it.
    pub accepted_by: Option<Uuid>,
    /// When it was accepted.
    pub accepted_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

impl Invitation {
    /// `true` when the token can still be presented.
    #[must_use]
    pub fn is_usable(&self, now: OffsetDateTime) -> bool {
        self.status == "pending" && self.expires_at > now
    }
}

/// A membership to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMembership {
    /// Organization to join.
    pub organization_id: Uuid,
    /// Account joining.
    pub user_id: Uuid,
    /// Membership status; `active` for a direct add.
    pub status: String,
    /// Whether the membership becomes the account's home organization.
    pub is_primary: bool,
}

/// An invitation to create, together with the raw token.
#[derive(Debug, Clone)]
pub struct CreatedInvitation {
    /// The stored row.
    pub invitation: Invitation,
    /// The raw token — returned once, hashed into `token_hash`, never readable again.
    pub token: String,
}

/// Fields [`update_membership`] may change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipChanges {
    /// New status.
    pub status: Option<String>,
    /// New primary flag.
    pub is_primary: Option<bool>,
}

impl MembershipChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.status.is_none() && self.is_primary.is_none()
    }
}

/// Validate a membership status.
pub fn validate_member_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if MEMBER_STATUSES.contains(&status.as_str()) {
        return Ok(status);
    }
    Err(IdentityError::InvalidMembership(format!(
        "status {status:?} must be one of {}",
        MEMBER_STATUSES.join(", ")
    )))
}

/// Validate a personal invitation message (trimmed, bounded).
pub fn validate_message(message: &str) -> Result<String> {
    let message = message.trim().to_owned();
    if message.chars().count() > MAX_INVITATION_MESSAGE {
        return Err(IdentityError::InvalidInvitation(format!(
            "message must be at most {MAX_INVITATION_MESSAGE} characters"
        )));
    }
    Ok(message)
}

/// Number of accounts currently holding an `active` membership in an organization.
pub async fn count_active_members(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let total: i64 = sqlx::query_scalar(
        "select count(*) from organization_members \
         where organization_id = $1 and status = 'active'",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Add an account to an organization, or answer [`IdentityError::MemberAlreadyPresent`].
///
/// The membership is inserted inside a transaction that also moves `users.organization_id`
/// when the membership is primary, so the home column and the membership table can never
/// disagree.
pub async fn add_member(pool: &PgPool, new: NewMembership) -> Result<Membership> {
    let status = validate_member_status(&new.status)?;

    let mut tx = pool.begin().await?;

    // Same ordering rule as [`update_membership`]: the partial unique index over
    // `user_id where is_primary` means the old primary has to be cleared first.
    if new.is_primary {
        sqlx::query(
            "update organization_members set is_primary = false, updated_at = now() \
                     where user_id = $1 and is_primary",
        )
        .bind(new.user_id)
        .execute(&mut *tx)
        .await?;
    }

    let membership: Membership = sqlx::query_as(
        "insert into organization_members (organization_id, user_id, status, is_primary, joined_at) \
         values ($1, $2, $3, $4, case when $3 = 'active' then now() else null end) \
         returning id, organization_id, user_id, status, is_primary, joined_at, created_at, updated_at",
    )
    .bind(new.organization_id)
    .bind(new.user_id)
    .bind(&status)
    .bind(new.is_primary)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_member_insert_error)?
    .ok_or(IdentityError::MemberAlreadyPresent)?;

    if new.is_primary {
        // The home column follows the primary flag; the index guarantees one primary row.
        sqlx::query("update users set organization_id = $1, updated_at = now() where id = $2")
            .bind(membership.organization_id)
            .bind(new.user_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(membership)
}

/// Look one membership up.
pub async fn find_member(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Membership>> {
    let membership = sqlx::query_as::<_, Membership>(
        "select id, organization_id, user_id, status, is_primary, joined_at, created_at, updated_at \
         from organization_members where organization_id = $1 and user_id = $2",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(membership)
}

/// The members of one organization, newest membership last.
pub async fn list_members(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Membership>> {
    let members = sqlx::query_as::<_, Membership>(
        "select id, organization_id, user_id, status, is_primary, joined_at, created_at, updated_at \
         from organization_members where organization_id = $1 \
         order by is_primary desc, joined_at asc nulls last, created_at asc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(members)
}

/// Every membership of one account, with the organization joined in — the switcher's list.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AccountMembership {
    /// Organization the account belongs to.
    pub organization_id: Uuid,
    /// Organization display name.
    pub organization_name: String,
    /// Organization slug.
    pub organization_slug: String,
    /// Organization status (`suspended` and `archived` rows are still listed, marked).
    pub organization_status: String,
    /// Membership status.
    pub status: String,
    /// Whether this is the account's home organization.
    pub is_primary: bool,
    /// Roles bound to the account in this organization, as `key:name` pairs.
    pub roles: Vec<String>,
}

/// Every organization an account belongs to, primary first then by name.
///
/// A platform account (no membership at all) gets an empty list, which is how the panel tells
/// "belongs to no tenant" from "belongs to one".
pub async fn list_account_memberships(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<AccountMembership>> {
    let rows = sqlx::query_as::<_, AccountMembership>(
        "select m.organization_id, o.name as organization_name, o.slug as organization_slug, \
                o.status as organization_status, m.status, m.is_primary, \
                coalesce( \
                    (select array_agg(r.key || ':' || r.name order by r.priority desc) \
                       from role_bindings b \
                       join roles r on r.id = b.role_id \
                      where b.user_id = m.user_id \
                        and b.revoked_at is null \
                        and (b.expires_at is null or b.expires_at > now()) \
                        and (b.organization_id = m.organization_id or b.organization_id is null)), \
                    '{}'::text[]) as roles \
           from organization_members m \
           join organizations o on o.id = m.organization_id \
          where m.user_id = $1 \
          order by m.is_primary desc, o.name asc",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Apply `changes` to a membership.
pub async fn update_membership(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
    changes: &MembershipChanges,
) -> Result<Membership> {
    let current = find_member(pool, organization_id, user_id)
        .await?
        .ok_or(IdentityError::MemberNotFound)?;
    if changes.is_empty() {
        return Ok(current);
    }

    let status = match &changes.status {
        Some(status) => Some(validate_member_status(status)?),
        None => None,
    };
    let is_primary = changes.is_primary;

    let mut tx = pool.begin().await?;

    // Promotion clears the previous primary **before** the row that becomes primary is written:
    // `organization_members_primary_key` is a partial unique index over `user_id where
    // is_primary`, so setting the new row first would collide with the old one and the whole
    // promotion would fail on the second switch.
    if is_primary == Some(true) {
        sqlx::query(
            "update organization_members set is_primary = false, updated_at = now() \
                     where user_id = $1 and is_primary and organization_id <> $2",
        )
        .bind(user_id)
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;
    }

    let updated: Membership = sqlx::query_as(
        "update organization_members set \
            status = coalesce($3, status), \
            is_primary = coalesce($4, is_primary), \
            joined_at = case when $3 = 'active' and joined_at is null then now() else joined_at end, \
            updated_at = now() \
         where organization_id = $1 and user_id = $2 \
         returning id, organization_id, user_id, status, is_primary, joined_at, created_at, updated_at",
    )
    .bind(organization_id)
    .bind(user_id)
    .bind(status)
    .bind(is_primary)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IdentityError::MemberNotFound)?;

    if updated.is_primary {
        // `users.organization_id` follows the primary flag, never the other way round.
        sqlx::query("update users set organization_id = $1, updated_at = now() where id = $2")
            .bind(updated.organization_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    } else if let Some(status) = &changes.status {
        if status == "suspended" {
            sqlx::query(
                "update users set updated_at = now() where id = $1 and organization_id = $2",
            )
            .bind(user_id)
            .bind(organization_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(updated)
}

/// Remove a membership. `false` when the row was already gone.
///
/// The home column follows: an account whose only membership is removed loses
/// `organization_id` and becomes a platform-level account again, which is exactly what
/// `users.organization_id`'s `on delete set null` did before memberships existed.
pub async fn remove_member(pool: &PgPool, organization_id: Uuid, user_id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;

    let removed = sqlx::query(
        "delete from organization_members \
                               where organization_id = $1 and user_id = $2",
    )
    .bind(organization_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;

    if removed {
        sqlx::query(
            "update users set organization_id = null, updated_at = now() \
             where id = $1 and organization_id = $2",
        )
        .bind(user_id)
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(removed)
}

/// Make `organization_id` the account's primary membership, joining the organization when the
/// account is not a member yet. This is what the switcher's `POST /me/organization` calls.
pub async fn make_primary(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Membership> {
    if find_member(pool, organization_id, user_id).await?.is_none() {
        add_member(
            pool,
            NewMembership {
                organization_id,
                user_id,
                status: "active".to_owned(),
                is_primary: false,
            },
        )
        .await?;
    }

    let membership = update_membership(
        pool,
        organization_id,
        user_id,
        &MembershipChanges {
            status: Some("active".to_owned()),
            is_primary: Some(true),
        },
    )
    .await?;
    Ok(membership)
}

// ---------------------------------------------------------------------------------------------
// Invitations
// ---------------------------------------------------------------------------------------------

/// An invitation to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvitation {
    /// Organization the invitee joins.
    pub organization_id: Uuid,
    /// Address to invite.
    pub email: String,
    /// Role granted on acceptance.
    pub role_id: Option<Uuid>,
    /// Who sends the invitation.
    pub invited_by: Option<Uuid>,
    /// Personal message.
    pub message: String,
    /// When the token stops working; the default lifetime when `None`.
    pub expires_at: Option<OffsetDateTime>,
}

/// Create an invitation and return it with its raw token.
///
/// An address that already holds a live invitation in this organization gets that pending
/// invitation back instead of a second one — the caller can then answer with it rather than
/// mail the same address twice.
pub async fn create_invitation(pool: &PgPool, new: NewInvitation) -> Result<CreatedInvitation> {
    let email = normalize_email(&new.email)?;
    let message = validate_message(&new.message)?;
    let expires_at = new
        .expires_at
        .unwrap_or(OffsetDateTime::now_utc() + Duration::days(DEFAULT_INVITATION_TTL_DAYS));

    if let Some(pending) = find_pending_invitation(pool, new.organization_id, &email).await? {
        if pending.expires_at > OffsetDateTime::now_utc() {
            return Err(IdentityError::InvitationAlreadyPending(pending));
        }
    }

    let token = generate_token();
    let token_hash = hash_token(&token);

    let invitation: Invitation = sqlx::query_as(
        "insert into organization_invitations \
            (organization_id, email, role_id, token_hash, invited_by, status, message, expires_at) \
         values ($1, $2, $3, $4, $5, 'pending', $6, $7) \
         returning id, organization_id, email, role_id, token_hash, invited_by, status, message, \
                   expires_at, accepted_by, accepted_at, created_at",
    )
    .bind(new.organization_id)
    .bind(&email)
    .bind(new.role_id)
    .bind(&token_hash)
    .bind(new.invited_by)
    .bind(&message)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(map_invitation_insert_error)?;

    Ok(CreatedInvitation { invitation, token })
}

/// The live invitation this address holds in this organization, when there is one.
pub async fn find_pending_invitation(
    pool: &PgPool,
    organization_id: Uuid,
    email: &str,
) -> Result<Option<Invitation>> {
    let invitation = sqlx::query_as::<_, Invitation>(
        "select id, organization_id, email, role_id, token_hash, invited_by, status, message, \
                expires_at, accepted_by, accepted_at, created_at \
           from organization_invitations \
          where organization_id = $1 and lower(email) = lower($2) and status = 'pending' \
          order by created_at desc limit 1",
    )
    .bind(organization_id)
    .bind(email)
    .fetch_optional(pool)
    .await?;
    Ok(invitation)
}

/// Look an invitation up by its token hash.
pub async fn find_invitation_by_token(pool: &PgPool, token: &str) -> Result<Option<Invitation>> {
    let token_hash = hash_token(token);
    let invitation = sqlx::query_as::<_, Invitation>(
        "select id, organization_id, email, role_id, token_hash, invited_by, status, message, \
                expires_at, accepted_by, accepted_at, created_at \
           from organization_invitations where token_hash = $1",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(invitation)
}

/// The invitations of one organization, newest first.
pub async fn list_invitations(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Invitation>> {
    let invitations = sqlx::query_as::<_, Invitation>(
        "select id, organization_id, email, role_id, token_hash, invited_by, status, message, \
                expires_at, accepted_by, accepted_at, created_at \
           from organization_invitations where organization_id = $1 order by created_at desc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(invitations)
}

/// Revoke a live invitation. `false` when the row was already decided or gone.
pub async fn revoke_invitation(pool: &PgPool, id: Uuid) -> Result<bool> {
    let revoked = sqlx::query(
        "update organization_invitations set status = 'revoked' \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    Ok(revoked)
}

/// Accept an invitation: it becomes `accepted`, the account joins the organization as an
/// `active` member and — when the organization is the account's only home — the membership
/// becomes primary.
///
/// A `pending` invitation whose token has expired is marked `expired` and refused with
/// [`IdentityError::InvitationExpired`], so the second attempt sees the same answer as the
/// first instead of a fresh acceptance.
pub async fn accept_invitation(pool: &PgPool, token: &str, user_id: Uuid) -> Result<Invitation> {
    let invitation = find_invitation_by_token(pool, token)
        .await?
        .ok_or(IdentityError::InvitationNotFound)?;

    if invitation.status == "accepted" {
        return Err(IdentityError::InvitationAlreadyUsed);
    }
    if invitation.status == "revoked" {
        return Err(IdentityError::InvitationRevoked);
    }
    if invitation.status == "expired" || invitation.expires_at <= OffsetDateTime::now_utc() {
        if invitation.status == "pending" {
            sqlx::query("update organization_invitations set status = 'expired' where id = $1")
                .bind(invitation.id)
                .execute(pool)
                .await?;
        }
        return Err(IdentityError::InvitationExpired);
    }

    // The account's home organization before the acceptance: when it had none, this
    // organization becomes its home and the membership becomes the primary one, so the
    // switcher has something to show the moment the invitation is accepted.
    let had_home: bool =
        sqlx::query_scalar("select organization_id is not null from users where id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;

    let mut tx = pool.begin().await?;

    // The membership is inserted in the same transaction as the acceptance: an account that is
    // a member already (the invitee belongs to the organization) simply re-activates.
    sqlx::query(
        "insert into organization_members (organization_id, user_id, status, is_primary, joined_at) \
         values ($1, $2, 'active', $3, now()) \
         on conflict (organization_id, user_id) do update \
            set status = 'active', is_primary = organization_members.is_primary or $3, \
                joined_at = coalesce(organization_members.joined_at, now()), \
                updated_at = now()",
    )
    .bind(invitation.organization_id)
    .bind(user_id)
    .bind(!had_home)
    .execute(&mut *tx)
    .await?;

    let accepted: Invitation = sqlx::query_as(
        "update organization_invitations \
            set status = 'accepted', accepted_by = $2, accepted_at = now() \
          where id = $1 and status = 'pending' \
          returning id, organization_id, email, role_id, token_hash, invited_by, status, message, \
                    expires_at, accepted_by, accepted_at, created_at",
    )
    .bind(invitation.id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IdentityError::InvitationAlreadyUsed)?;

    if !had_home {
        // An account with no home organization adopts this one: `users.organization_id` may not
        // be null for a member to work in a tenant.
        sqlx::query("update users set organization_id = $1, updated_at = now() where id = $2")
            .bind(invitation.organization_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    Ok(accepted)
}

/// Does this address already belong to the organization (in any membership status)?
pub async fn address_is_member(
    pool: &PgPool,
    organization_id: Uuid,
    email: &str,
) -> Result<Option<Uuid>> {
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "select m.user_id from organization_members m \
           join users u on u.id = m.user_id \
          where m.organization_id = $1 and lower(u.email) = lower($2)",
    )
    .bind(organization_id)
    .bind(email)
    .fetch_optional(pool)
    .await?;
    Ok(user_id)
}

/// `true` when the key has the shape the schema's `key_format` check enforces.
#[must_use]
pub fn is_shaped_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LENGTH
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key.ends_with(|c: char| c.is_ascii_alphanumeric())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn map_member_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            // The two unique indexes answer different questions: the same pair twice is a
            // duplicate member, a second primary row is a conflicting promotion.
            if db_err.constraint() == Some("organization_members_primary_key") {
                IdentityError::InvalidMembership(
                    "the account already has a primary organization".to_owned(),
                )
            } else {
                IdentityError::MemberAlreadyPresent
            }
        }
        other => IdentityError::Database(other),
    }
}

fn map_invitation_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            IdentityError::InvitationAlreadyPending(Invitation {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                email: String::new(),
                role_id: None,
                token_hash: String::new(),
                invited_by: None,
                status: String::new(),
                message: String::new(),
                expires_at: OffsetDateTime::UNIX_EPOCH,
                accepted_by: None,
                accepted_at: None,
                created_at: OffsetDateTime::UNIX_EPOCH,
            })
        }
        other => IdentityError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_statuses_come_from_the_schema_set() {
        assert_eq!(validate_member_status(" Active ").expect("valid"), "active");
        for status in MEMBER_STATUSES {
            assert!(validate_member_status(status).is_ok());
        }
        for bad in ["", "banned", "deleted"] {
            assert!(
                validate_member_status(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn invitation_messages_are_trimmed_and_bounded() {
        assert_eq!(
            validate_message("  welcome aboard  ").expect("valid"),
            "welcome aboard"
        );
        assert_eq!(validate_message("").expect("empty is fine"), "");
        assert!(
            validate_message(&"x".repeat(MAX_INVITATION_MESSAGE + 1)).is_err(),
            "a message longer than the ceiling is refused"
        );
        assert!(validate_message(&"x".repeat(MAX_INVITATION_MESSAGE)).is_ok());
    }

    #[test]
    fn membership_change_sets_report_emptiness() {
        assert!(MembershipChanges::default().is_empty());
        assert!(
            !MembershipChanges {
                status: Some("suspended".to_owned()),
                is_primary: None,
            }
            .is_empty()
        );
        assert!(
            !MembershipChanges {
                status: None,
                is_primary: Some(false),
            }
            .is_empty()
        );
    }

    #[test]
    fn keys_follow_the_schema_shape() {
        for good in ["marketing", "site-a", "a1"] {
            assert!(is_shaped_key(good), "{good:?} must be accepted");
        }
        for bad in ["", "-lead", "lead-", "Lead", "a_b", "a b"] {
            assert!(!is_shaped_key(bad), "{bad:?} must be rejected");
        }
        assert!(!is_shaped_key(&"a".repeat(MAX_KEY_LENGTH + 1)));
    }

    #[test]
    fn an_invitation_is_only_usable_while_it_is_pending_and_unexpired() {
        let now = OffsetDateTime::now_utc();
        let mut invitation = Invitation {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            email: "ada@example.com".to_owned(),
            role_id: None,
            token_hash: "hash".to_owned(),
            invited_by: None,
            status: "pending".to_owned(),
            message: String::new(),
            expires_at: now + Duration::days(1),
            accepted_by: None,
            accepted_at: None,
            created_at: now,
        };
        assert!(invitation.is_usable(now));

        invitation.expires_at = now - Duration::seconds(1);
        assert!(!invitation.is_usable(now), "an expired token is not usable");

        invitation.expires_at = now + Duration::days(1);
        invitation.status = "revoked".to_owned();
        assert!(!invitation.is_usable(now), "a revoked token is not usable");

        invitation.status = "accepted".to_owned();
        assert!(!invitation.is_usable(now), "a used token is not usable");
    }
}
