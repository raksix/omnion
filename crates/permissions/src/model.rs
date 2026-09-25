//! The IAM value types: roles, permission entries, scopes and role bindings.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{PermissionsError, Result};

/// Highest priority a role may carry (the Owner role, docs/07-IAM.md §3).
pub const MAX_PRIORITY: i32 = 1000;

/// Lowest priority a role may carry.
pub const MIN_PRIORITY: i32 = 0;

/// Longest accepted role key.
const MAX_ROLE_KEY_LENGTH: usize = 64;

/// Longest accepted role name.
const MAX_ROLE_NAME_LENGTH: usize = 80;

/// Whether a role entry grants or refuses a permission (docs/07-IAM.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Explicit allow.
    Allow,
    /// Explicit deny.
    Deny,
}

impl Effect {
    /// Value stored in `role_permissions.effect`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// Parse the stored value.
    pub fn from_stored(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(PermissionsError::InvalidScope(format!(
                "unknown effect {other:?}"
            ))),
        }
    }
}

/// A role (docs/07-IAM.md §1, §3, §4).
///
/// `organization_id == None` marks a platform role — the six base roles every installation
/// ships with; an organization id marks a role the customer created.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Role {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization (`None` = platform role).
    pub organization_id: Option<Uuid>,
    /// Stable key, unique within the scope.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the role is for.
    pub description: String,
    /// Position in the hierarchy (docs/07-IAM.md §3): higher wins.
    pub priority: i32,
    /// Role this one inherits from.
    pub inherits_role_id: Option<Uuid>,
    /// Whether inherited permissions apply.
    pub inherit_permissions: bool,
    /// Platform-managed roles cannot be edited by customers.
    pub is_system: bool,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// One entry of a role's permission set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePermission {
    /// Permission key from the catalogue.
    pub key: String,
    /// Allow or deny.
    pub effect: Effect,
}

/// A permission entry as handed to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePermissionInput {
    /// Permission key from the catalogue.
    pub key: String,
    /// Allow or deny.
    pub effect: Effect,
}

/// A role together with its own permission entries — the unit the resolver works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignment {
    /// The role.
    pub role: Role,
    /// The role's own entries (not inherited ones).
    pub permissions: Vec<RolePermission>,
}

impl RoleAssignment {
    /// The role's own entry for `key`, when it has one.
    #[must_use]
    pub fn entry(&self, key: &str) -> Option<Effect> {
        self.permissions
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.effect)
    }
}

/// Where a role applies (docs/07-IAM.md §6).
///
/// v0 carries the three scopes the schema enforces; department/module/resource scopes arrive
/// with the rest of the IAM model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Platform level — applies everywhere.
    Global,
    /// An organization (and every site inside it).
    Organization {
        /// The organization.
        organization_id: Uuid,
    },
    /// One site of an organization.
    Site {
        /// Owning organization, when known.
        organization_id: Option<Uuid>,
        /// The site.
        site_id: Uuid,
    },
}

impl Scope {
    /// The organization this scope resolves in, when it is scoped to one.
    #[must_use]
    pub fn organization_id(self) -> Option<Uuid> {
        match self {
            Self::Global => None,
            Self::Organization { organization_id } => Some(organization_id),
            Self::Site {
                organization_id, ..
            } => organization_id,
        }
    }

    /// The site this scope resolves in, when it is scoped to one.
    #[must_use]
    pub fn site_id(self) -> Option<Uuid> {
        match self {
            Self::Global | Self::Organization { .. } => None,
            Self::Site { site_id, .. } => Some(site_id),
        }
    }

    /// Value stored in `role_bindings.scope_type`.
    #[must_use]
    pub fn scope_type(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Organization { .. } => "organization",
            Self::Site { .. } => "site",
        }
    }

    /// Human-readable form for audit metadata.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Global => "global".to_owned(),
            Self::Organization { organization_id } => format!("organization:{organization_id}"),
            Self::Site { site_id, .. } => format!("site:{site_id}"),
        }
    }

    /// Rebuild a scope from stored columns, validating the shape the schema also enforces.
    pub fn from_parts(
        scope_type: &str,
        organization_id: Option<Uuid>,
        site_id: Option<Uuid>,
    ) -> Result<Self> {
        match (scope_type, organization_id, site_id) {
            ("global", None, None) => Ok(Self::Global),
            ("organization", Some(organization_id), None) => {
                Ok(Self::Organization { organization_id })
            }
            ("site", organization_id, Some(site_id)) => Ok(Self::Site {
                organization_id,
                site_id,
            }),
            (other, organization_id, site_id) => Err(PermissionsError::InvalidScope(format!(
                "scope_type={other:?} organization_id={organization_id:?} site_id={site_id:?}"
            ))),
        }
    }
}

/// A role bound to an account at a scope (docs/07-IAM.md §6, §9, §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleBinding {
    /// Primary key.
    pub id: Uuid,
    /// The role.
    pub role_id: Uuid,
    /// The account.
    pub user_id: Uuid,
    /// Where the role applies.
    pub scope: Scope,
    /// Who granted it (`None` = the platform).
    pub granted_by: Option<Uuid>,
    /// When the binding stops applying (temporary roles).
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

impl RoleBinding {
    /// `true` when the binding still applies at `now`.
    #[must_use]
    pub fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expires| expires > now)
    }
}

/// A role to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRole {
    /// Owning organization (platform roles are created by the seed, not here).
    pub organization_id: Uuid,
    /// Stable key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the role is for.
    pub description: String,
    /// Hierarchy position.
    pub priority: i32,
    /// Optional parent role to inherit from.
    pub inherits_role_id: Option<Uuid>,
}

/// A binding to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBinding {
    /// The role.
    pub role_id: Uuid,
    /// The account.
    pub user_id: Uuid,
    /// Where the role applies.
    pub scope: Scope,
    /// Who grants it.
    pub granted_by: Option<Uuid>,
    /// Optional expiry (temporary roles).
    pub expires_at: Option<OffsetDateTime>,
}

/// Allow/deny counts of a role's permission set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PermissionSummary {
    /// Explicit allows.
    pub allowed: i64,
    /// Explicit denies.
    pub denied: i64,
}

/// Validate a role key: lowercase slug, digits and dashes (`marketing-manager`).
pub fn validate_role_key(key: &str) -> Result<String> {
    let key = key.trim().to_lowercase();
    let shaped = !key.is_empty()
        && key.len() <= MAX_ROLE_KEY_LENGTH
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(key);
    }
    Err(PermissionsError::InvalidRoleKey(key))
}

/// Validate a role name: non-empty and reasonably short.
pub fn validate_role_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_ROLE_NAME_LENGTH {
        return Err(PermissionsError::InvalidRoleName(name));
    }
    Ok(name)
}

/// Validate a role priority.
pub fn validate_priority(priority: i32) -> Result<i32> {
    if (MIN_PRIORITY..=MAX_PRIORITY).contains(&priority) {
        return Ok(priority);
    }
    Err(PermissionsError::InvalidPriority {
        min: MIN_PRIORITY,
        max: MAX_PRIORITY,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_keys_are_slugs() {
        assert_eq!(
            validate_role_key("  Marketing-Manager ").expect("valid"),
            "marketing-manager"
        );
        for bad in [
            "",
            "-lead",
            "lead-",
            "lead_manager",
            "lead manager",
            "Lead!",
        ] {
            assert!(validate_role_key(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_role_key(&"a".repeat(MAX_ROLE_KEY_LENGTH + 1)).is_err());
    }

    #[test]
    fn role_names_and_priorities_are_bounded() {
        assert_eq!(validate_role_name("  Editor ").expect("valid"), "Editor");
        assert!(validate_role_name("   ").is_err());
        assert!(validate_role_name(&"n".repeat(MAX_ROLE_NAME_LENGTH + 1)).is_err());

        assert_eq!(validate_priority(1000).expect("valid"), 1000);
        assert_eq!(validate_priority(0).expect("valid"), 0);
        assert!(validate_priority(-1).is_err());
        assert!(validate_priority(1001).is_err());
    }

    #[test]
    fn effects_round_trip_through_storage() {
        for effect in [Effect::Allow, Effect::Deny] {
            assert_eq!(
                Effect::from_stored(effect.as_str()).expect("stored value must parse"),
                effect
            );
        }
        assert!(Effect::from_stored("maybe").is_err());
    }

    #[test]
    fn scopes_round_trip_through_columns() {
        let organization_id = Uuid::new_v4();
        let site_id = Uuid::new_v4();

        let cases = [
            Scope::Global,
            Scope::Organization { organization_id },
            Scope::Site {
                organization_id: Some(organization_id),
                site_id,
            },
            Scope::Site {
                organization_id: None,
                site_id,
            },
        ];
        for scope in cases {
            let rebuilt =
                Scope::from_parts(scope.scope_type(), scope.organization_id(), scope.site_id())
                    .expect("valid shape");
            assert_eq!(rebuilt, scope);
        }

        assert!(
            Scope::from_parts("organization", None, None).is_err(),
            "an organization scope needs an organization"
        );
        assert!(
            Scope::from_parts("site", Some(organization_id), None).is_err(),
            "a site scope needs a site"
        );
        assert!(Scope::from_parts("galaxy", None, None).is_err());
    }

    #[test]
    fn binding_expiry_is_evaluated_against_now() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut binding = RoleBinding {
            id: Uuid::nil(),
            role_id: Uuid::nil(),
            user_id: Uuid::nil(),
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
            revoked_at: None,
            created_at: now,
        };
        assert!(binding.is_active_at(now), "a plain binding is active");

        binding.expires_at = Some(now - time::Duration::seconds(1));
        assert!(
            !binding.is_active_at(now),
            "an expired binding is not active"
        );

        binding.expires_at = Some(now + time::Duration::seconds(1));
        assert!(binding.is_active_at(now));

        binding.revoked_at = Some(now);
        assert!(
            !binding.is_active_at(now),
            "a revoked binding is not active"
        );
    }
}
