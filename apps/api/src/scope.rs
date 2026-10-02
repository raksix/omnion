//! Tenancy scope helpers shared by the routes that work on organizations and sites.
//!
//! The route guard (`crate::guards::require`) already proved that the caller holds the route's
//! permission in their own scope. These helpers answer the second question: may this account
//! touch *this* organization or site?
//!
//! The rule the tenancy surface runs on: an account with a primary organization works only
//! inside it; a platform-level account (primary organization `null`) may work on any target.
//! Anything else is `403 cross_organization` — the same answer the IAM surface gives.

use omnion_identity::organizations::Organization;
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

/// Refuse a write to an organization that is not `active` (REQ-005, slice 3).
///
/// This is the *behaviour* half of the status column: a suspended organization keeps every read
/// — the operator has to be able to inspect the tenant it just froze, and so does the tenant —
/// but refuses every write, by name, with the reason in the body. Reads are deliberately not
/// routed through here: a screen that has to be hidden because its tenant is frozen is a screen
/// an operator cannot use to find out *why*.
///
/// The exception is the status change itself (`is_status_change`), because a tenant that cannot
/// be reactivated can never be reactivated — the control that undoes the freeze cannot be
/// subject to the freeze. Getting that backwards produces a tenant that is permanently stuck
/// and a panel whose only visible option is the one that fails.
pub fn ensure_writable(organization: &Organization) -> Result<(), ApiError> {
    if organization.accepts_writes() {
        return Ok(());
    }
    Err(ApiError::new(
        axum::http::StatusCode::CONFLICT,
        "organization_not_writable",
        omnion_identity::organizations::write_refusal(organization),
    )
    .with_details(serde_json::json!({
        "organization_id": organization.id,
        "organization_slug": organization.slug,
        "status": organization.status,
        "writes": false,
        "reads": true,
    })))
}

/// `true` when a change only moves the organization between statuses.
///
/// Used by the update route to keep a suspended tenant escapable: a rename of a frozen tenant
/// is refused, but setting its status back to `active` is the one write that must get through.
#[must_use]
pub fn is_status_change(status: Option<&str>) -> bool {
    status.is_some()
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
///
/// The fourth arm is the one the panel has to be able to *act* on. An account without a primary
/// organization holds no tenant of its own, so a write that does not name one has no subject:
/// answering `400 organization_required` with the sentence alone leaves the panel printing
/// "organization_id is required" in a banner above a form that has no organization control on
/// it. `field: "organization_id"` is what lets a client put a picker next to the failing field
/// instead of guessing, and it is the difference between a refusal that is *actionable* and one
/// that is merely correct.
pub fn resolve_organization(
    current: &CurrentSession,
    requested: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    match (current.user.organization_id, requested) {
        (Some(own), Some(target)) if own != target => Err(cross_organization()),
        (Some(own), _) => Ok(own),
        (None, Some(target)) => Ok(target),
        (None, None) => Err(organization_required()),
    }
}

/// The `400` handed out when an account without a primary organization writes without naming a
/// tenant. Shared so every route that resolves a scope answers with the same code, the same
/// message and the same `field` — a walk that catches one route answering it and passes while a
/// sibling answers a bare sentence has proved the shape, not the rule.
#[must_use]
pub fn organization_required() -> ApiError {
    ApiError::bad_request(
        "organization_required",
        "organization_id is required for an account without a primary organization",
    )
    .with_details(serde_json::json!({
        "field": "organization_id",
        "reason": "no_primary_organization",
    }))
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
                absolute_expires_at: None,
                device_id: None,
                auth_methods: Vec::new(),
                revoked_at: None,
                revoke_reason: None,
                step_up_at: None,
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

    #[test]
    fn a_missing_tenant_names_the_field_a_client_has_to_send() {
        // The criterion is "a 400 naming the field", and "naming" is the part that is easy to
        // leave out: a bare sentence contains the string `organization_id` too, so an assertion
        // on `message.contains("organization_id")` passes against an error a client cannot act
        // on. The structured `field` is the contract.
        let refusal = resolve_organization(&session_for(None), None)
            .expect_err("a platform account has to name a tenant");

        assert_eq!(refusal.code(), "organization_required");
        assert_eq!(refusal.status(), axum::http::StatusCode::BAD_REQUEST);
        let details = refusal
            .details()
            .unwrap_or_else(|| panic!("a missing tenant names the field it has to send"));
        assert_eq!(details["field"], "organization_id");
        assert_eq!(details["reason"], "no_primary_organization");

        // The two failures a client renders differently: a missing tenant is *its* problem to
        // fix (ask for one), while reaching into another tenant is a refusal it must not offer
        // a way around. If the second ever carried a `field` too, a client could be talked into
        // showing a picker that only ever produces this same error.
        let out_of_scope =
            resolve_organization(&session_for(Some(Uuid::new_v4())), Some(Uuid::new_v4()))
                .expect_err("another tenant is out of scope");
        assert_eq!(out_of_scope.code(), "cross_organization");
        assert_eq!(out_of_scope.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(
            out_of_scope.details().is_none(),
            "cross_organization names no field to fix: {:?}",
            out_of_scope.details()
        );
    }

    fn organization_named(name: &str, status: &str) -> Organization {
        Organization {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            slug: name.to_lowercase(),
            status: status.to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn only_an_active_organization_accepts_writes() {
        assert!(ensure_writable(&organization_named("Acme", "active")).is_ok());

        // Reads are *not* the rule's business: the guard takes no read path at all, and the two
        // frozen statuses are exactly as unfrozen to a `get` as `active` is.
        for status in ["suspended", "archived"] {
            let organization = organization_named("Acme", status);
            let refusal =
                ensure_writable(&organization).expect_err("a frozen organization refuses writes");
            assert_eq!(refusal.code(), "organization_not_writable");
            assert_eq!(refusal.status(), axum::http::StatusCode::CONFLICT);
            // The details are what a panel reads: the status it has to render, and the flag that
            // tells it reads still work. `ApiError` has no `Display`, which is the point of a
            // structured error — assert on the structure, not on a formatted string.
            let details = refusal.details().expect("the refusal carries details");
            assert_eq!(details["status"], status);
            assert_eq!(details["writes"], false);
            assert_eq!(details["reads"], true);
            assert_eq!(details["organization_slug"], "acme");
        }
    }

    #[test]
    fn the_status_change_is_never_itself_refused() {
        // `is_status_change` is the escape hatch: without it a suspended tenant can never be
        // reactivated, because the guard would sit in front of the only control that undoes it.
        assert!(
            is_status_change(Some("active")),
            "reactivating is a status change"
        );
        assert!(
            is_status_change(Some("suspended")),
            "suspending is a status change"
        );
        assert!(
            !is_status_change(None),
            "a change that carries no status is a plain write and must be refused"
        );
    }
}
