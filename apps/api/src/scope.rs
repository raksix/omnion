//! Tenancy scope helpers shared by the routes that work on organizations and sites.
//!
//! The route guard (`crate::guards::require`) already proved that the caller holds the route's
//! permission in their own scope. These helpers answer the second question: may this account
//! touch *this* organization or site?
//!
//! The rule the tenancy surface runs on: an account with a primary organization works only
//! inside it; a platform-level account (primary organization `null`) may work on any target.
//! Anything else is `403 cross_organization` — the same answer the IAM surface gives.

use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;

/// Refuse work on an organization the caller does not belong to.
pub fn ensure_same_organization(
    current: &CurrentSession,
    organization_id: Option<Uuid>,
) -> Result<(), ApiError> {
    match (current.user.organization_id, organization_id) {
        (Some(own), Some(target)) if own != target => Err(cross_organization()),
        _ => Ok(()),
    }
}

/// Require a platform-level account: only an account without a primary organization may open
/// a new tenant or act across organizations. An organization account is refused even when it
/// holds the permission, because that permission lets it run *its own* tenancy, not the
/// platform's.
pub fn platform_only(current: &CurrentSession) -> Result<(), ApiError> {
    if current.user.organization_id.is_none() {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "platform_only",
        "organizations are managed at the platform level; this account works inside its own \
         organization",
    ))
}

/// The organization an action applies to: the caller's own unless a platform account picks one.
pub fn resolve_organization(
    current: &CurrentSession,
    requested: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    match (current.user.organization_id, requested) {
        (Some(own), Some(target)) if own != target => Err(cross_organization()),
        (Some(own), _) => Ok(own),
        (None, Some(target)) => Ok(target),
        (None, None) => Err(ApiError::bad_request(
            "organization_required",
            "organization_id is required for an account without a primary organization",
        )),
    }
}

/// The `403` handed out when an account reaches outside its own organization.
#[must_use]
pub fn cross_organization() -> ApiError {
    ApiError::forbidden(
        "cross_organization",
        "this account may only work inside its own organization",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_identity::sessions::Session;
    use omnion_identity::users::User;
    use time::OffsetDateTime;

    fn session_for(organization_id: Option<Uuid>) -> CurrentSession {
        CurrentSession {
            user: User {
                id: Uuid::nil(),
                organization_id,
                email: "ada@example.com".to_owned(),
                display_name: "Ada".to_owned(),
                status: "active".to_owned(),
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            session: Session {
                id: Uuid::nil(),
                user_id: Uuid::nil(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                expires_at: OffsetDateTime::UNIX_EPOCH,
                last_seen_at: None,
            },
            token: "token".to_owned(),
        }
    }

    #[test]
    fn organization_accounts_stay_inside_their_organization() {
        let own = Uuid::new_v4();
        let other = Uuid::new_v4();
        let current = session_for(Some(own));

        assert!(ensure_same_organization(&current, Some(own)).is_ok());
        assert!(ensure_same_organization(&current, None).is_ok());
        assert_eq!(
            ensure_same_organization(&current, Some(other))
                .expect_err("another organization is out of scope")
                .code(),
            "cross_organization"
        );
    }

    #[test]
    fn platform_accounts_may_touch_any_organization() {
        let current = session_for(None);
        let other = Uuid::new_v4();
        assert!(ensure_same_organization(&current, Some(other)).is_ok());
    }

    #[test]
    fn only_platform_accounts_open_tenants() {
        assert!(platform_only(&session_for(None)).is_ok());
        assert_eq!(
            platform_only(&session_for(Some(Uuid::new_v4())))
                .expect_err("an organization account may not open a tenant")
                .code(),
            "platform_only"
        );
    }

    #[test]
    fn the_target_organization_follows_the_caller() {
        let own = Uuid::new_v4();
        let other = Uuid::new_v4();

        assert_eq!(
            resolve_organization(&session_for(Some(own)), None).expect("own organization"),
            own
        );
        assert_eq!(
            resolve_organization(&session_for(Some(own)), Some(own)).expect("own organization"),
            own
        );
        assert_eq!(
            resolve_organization(&session_for(None), Some(other)).expect("platform pick"),
            other
        );
        assert_eq!(
            resolve_organization(&session_for(None), None)
                .expect_err("the platform must name a tenant")
                .code(),
            "organization_required"
        );
        assert_eq!(
            resolve_organization(&session_for(Some(own)), Some(other))
                .expect_err("another tenant is out of scope")
                .code(),
            "cross_organization"
        );
    }
}
