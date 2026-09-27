//! The IAM value types: roles, permission entries, scopes and role bindings.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{PermissionsError, Result};

/// Highest priority a role may carry (the Owner role, docs/07-IAM.md §3).
pub const MAX_PRIORITY: i32 = 1000;

/// Lowest priority a role may carry.
pub const MIN_PRIORITY: i32 = 0;

/// Longest inheritance chain a role may sit in, counted in each direction (docs/07-IAM.md §4:
/// "a single-parent chain, at most eight levels deep").
pub const MAX_INHERITANCE_DEPTH: usize = 8;

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

/// Who a role binding attaches to (docs/07-IAM.md §9, §14).
///
/// People, groups and machine identities share one table, so a group membership or a service
/// account grants exactly the way a personal binding does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// A person.
    User(Uuid),
    /// A group (team).
    Group(Uuid),
    /// A machine identity.
    ServiceAccount(Uuid),
}

impl Subject {
    /// Value stored in `role_bindings.subject_type`.
    #[must_use]
    pub fn subject_type(self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Group(_) => "group",
            Self::ServiceAccount(_) => "service_account",
        }
    }

    /// The id of the subject.
    #[must_use]
    pub fn id(self) -> Uuid {
        match self {
            Self::User(id) | Self::Group(id) | Self::ServiceAccount(id) => id,
        }
    }

    /// Rebuild a subject from stored columns.
    pub fn from_parts(subject_type: &str, subject_id: Uuid) -> Result<Self> {
        match subject_type {
            "user" => Ok(Self::User(subject_id)),
            "group" => Ok(Self::Group(subject_id)),
            "service_account" => Ok(Self::ServiceAccount(subject_id)),
            other => Err(PermissionsError::InvalidScope(format!(
                "unknown subject_type {other:?}"
            ))),
        }
    }

    /// Human-readable form for audit metadata.
    #[must_use]
    pub fn describe(self) -> String {
        format!("{}:{}", self.subject_type(), self.id())
    }
}

/// What a request targets — the context a binding is matched against.
///
/// A plain request carries the organization and (when it works inside one) the site. The finer
/// scopes add their own field: `department` and `module` name their target, and `path` is the
/// resource a path glob such as `/blog/*` is compared with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceContext {
    /// Organization the request runs in.
    pub organization_id: Option<Uuid>,
    /// Site the request runs in.
    pub site_id: Option<Uuid>,
    /// Department key the request relates to.
    pub department: Option<String>,
    /// Module key the request relates to.
    pub module: Option<String>,
    /// Resource path the request targets, when it names one.
    pub path: Option<String>,
}

impl ResourceContext {
    /// The context of a plain scope-level request.
    #[must_use]
    pub fn from_scope(scope: Scope) -> Self {
        Self {
            organization_id: scope.organization_id(),
            site_id: scope.site_id(),
            department: None,
            module: None,
            path: None,
        }
    }

    /// The same context with a resource path attached.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Where a role applies (docs/07-IAM.md §6).
///
/// The ladder runs from the platform level down to a single resource: `global` → `organization`
/// → `site` → `department` / `module` → `resource` (a path glob).
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// One department of an organization.
    Department {
        /// The organization.
        organization_id: Uuid,
        /// Department key.
        department: String,
    },
    /// One module, on a site or across a whole organization.
    Module {
        /// Owning organization, when known.
        organization_id: Option<Uuid>,
        /// The site it is limited to, when it is.
        site_id: Option<Uuid>,
        /// Module key (`content`, `analytics`, …).
        module: String,
    },
    /// One resource of a site or organization, named by a glob (`/blog/*`).
    Resource {
        /// Owning organization, when known.
        organization_id: Option<Uuid>,
        /// The site it is limited to, when it is.
        site_id: Option<Uuid>,
        /// Kind of resource (`path` for a URL path glob).
        resource_type: String,
        /// The resource pattern.
        resource_id: String,
    },
}

impl Scope {
    /// The organization this scope resolves in, when it is scoped to one.
    #[must_use]
    pub fn organization_id(&self) -> Option<Uuid> {
        match self {
            Self::Global => None,
            Self::Organization { organization_id } => Some(*organization_id),
            Self::Site {
                organization_id, ..
            } => *organization_id,
            Self::Department {
                organization_id, ..
            } => Some(*organization_id),
            Self::Module {
                organization_id, ..
            }
            | Self::Resource {
                organization_id, ..
            } => *organization_id,
        }
    }

    /// The site this scope resolves in, when it is scoped to one.
    #[must_use]
    pub fn site_id(&self) -> Option<Uuid> {
        match self {
            Self::Global | Self::Organization { .. } | Self::Department { .. } => None,
            Self::Site { site_id, .. } => Some(*site_id),
            Self::Module { site_id, .. } | Self::Resource { site_id, .. } => *site_id,
        }
    }

    /// Value stored in `role_bindings.scope_type`.
    #[must_use]
    pub fn scope_type(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Organization { .. } => "organization",
            Self::Site { .. } => "site",
            Self::Department { .. } => "department",
            Self::Module { .. } => "module",
            Self::Resource { .. } => "resource",
        }
    }

    /// Value stored in `role_bindings.resource_type`, when the scope carries one.
    #[must_use]
    pub fn resource_type(&self) -> Option<&str> {
        match self {
            Self::Resource { resource_type, .. } => Some(resource_type),
            _ => None,
        }
    }

    /// Value stored in `role_bindings.resource_id`, when the scope carries one.
    #[must_use]
    pub fn resource_id(&self) -> Option<&str> {
        match self {
            Self::Department { department, .. } => Some(department),
            Self::Module { module, .. } => Some(module),
            Self::Resource { resource_id, .. } => Some(resource_id),
            _ => None,
        }
    }

    /// `true` when this scope covers the target of `context`.
    ///
    /// This is the one matcher the resolver, the members tab and the simulator share: broad
    /// scopes apply whenever their level matches, and the finer scopes need their own field —
    /// a department binding never applies to a request that names no department, and a path glob
    /// only applies when the request targets a path that matches it.
    #[must_use]
    pub fn applies_to(&self, context: &ResourceContext) -> bool {
        match self {
            Self::Global => true,
            Self::Organization { organization_id } => {
                context.organization_id == Some(*organization_id)
            }
            Self::Site { site_id, .. } => context.site_id == Some(*site_id),
            Self::Department {
                organization_id,
                department,
            } => {
                context.organization_id == Some(*organization_id)
                    && context.department.as_deref() == Some(department.as_str())
            }
            Self::Module {
                organization_id,
                site_id,
                module,
            } => {
                context.module.as_deref() == Some(module.as_str())
                    && organization_id.is_none_or(|own| context.organization_id == Some(own))
                    && site_id.is_none_or(|own| context.site_id == Some(own))
            }
            Self::Resource {
                organization_id,
                site_id,
                resource_type,
                resource_id,
            } => {
                let target = match resource_type.as_str() {
                    // A path glob applies only to a request that names a path.
                    "path" => context
                        .path
                        .as_deref()
                        .is_some_and(|path| crate::matching::glob_matches(resource_id, path)),
                    // A module-named resource matches the request's module.
                    "module" => context.module.as_deref() == Some(resource_id.as_str()),
                    // Unknown kinds are kept but never match, rather than matching everything.
                    _ => false,
                };
                target
                    && organization_id.is_none_or(|own| context.organization_id == Some(own))
                    && site_id.is_none_or(|own| context.site_id == Some(own))
            }
        }
    }

    /// Human-readable form for audit metadata.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Global => "global".to_owned(),
            Self::Organization { organization_id } => format!("organization:{organization_id}"),
            Self::Site { site_id, .. } => format!("site:{site_id}"),
            Self::Department {
                organization_id,
                department,
            } => format!("department:{department}@{organization_id}"),
            Self::Module {
                module, site_id, ..
            } => match site_id {
                Some(site_id) => format!("module:{module}@site:{site_id}"),
                None => format!("module:{module}"),
            },
            Self::Resource {
                resource_type,
                resource_id,
                site_id,
                ..
            } => match site_id {
                Some(site_id) => format!("resource:{resource_type}:{resource_id}@site:{site_id}"),
                None => format!("resource:{resource_type}:{resource_id}"),
            },
        }
    }

    /// Rebuild a scope from stored columns, validating the shape the schema also enforces.
    pub fn from_parts(
        scope_type: &str,
        organization_id: Option<Uuid>,
        site_id: Option<Uuid>,
        resource_type: Option<&str>,
        resource_id: Option<&str>,
    ) -> Result<Self> {
        match (
            scope_type,
            organization_id,
            site_id,
            resource_type,
            resource_id,
        ) {
            ("global", None, None, None, None) => Ok(Self::Global),
            ("organization", Some(organization_id), None, None, None) => {
                Ok(Self::Organization { organization_id })
            }
            ("site", organization_id, Some(site_id), None, None) => Ok(Self::Site {
                organization_id,
                site_id,
            }),
            ("department", Some(organization_id), None, None, Some(department)) => {
                Ok(Self::Department {
                    organization_id,
                    department: department.to_owned(),
                })
            }
            ("module", organization_id, site_id, None, Some(module))
                if organization_id.is_some() || site_id.is_some() =>
            {
                Ok(Self::Module {
                    organization_id,
                    site_id,
                    module: module.to_owned(),
                })
            }
            ("resource", organization_id, site_id, Some(resource_type), Some(resource_id))
                if organization_id.is_some() || site_id.is_some() =>
            {
                Ok(Self::Resource {
                    organization_id,
                    site_id,
                    resource_type: resource_type.to_owned(),
                    resource_id: resource_id.to_owned(),
                })
            }
            (other, organization_id, site_id, resource_type, resource_id) => {
                Err(PermissionsError::InvalidScope(format!(
                    "scope_type={other:?} organization_id={organization_id:?} \
                     site_id={site_id:?} resource_type={resource_type:?} \
                     resource_id={resource_id:?}"
                )))
            }
        }
    }
}

/// A role bound to a subject at a scope (docs/07-IAM.md §6, §9, §14, §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleBinding {
    /// Primary key.
    pub id: Uuid,
    /// The role.
    pub role_id: Uuid,
    /// Who the role is bound to.
    pub subject: Subject,
    /// The account, when the subject is a person (kept for one release: expand-then-contract).
    pub user_id: Option<Uuid>,
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

    /// `true` when the binding carried a temporary window that has run out.
    #[must_use]
    pub fn is_expired_at(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_some_and(|expires| expires <= now)
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

/// A binding to create for any kind of subject (person, group or machine identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubjectBinding {
    /// The role.
    pub role_id: Uuid,
    /// Who receives the role.
    pub subject: Subject,
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

/// One stored version of a role (docs/07-IAM.md §17).
///
/// A version is the role's own fields plus its permission set as they stood at one moment; the
/// history tab reads consecutive versions and draws their diff.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RoleVersion {
    /// Primary key.
    pub id: Uuid,
    /// The role this version belongs to.
    pub role_id: Uuid,
    /// One-based version number, counted per role.
    pub version: i32,
    /// Role name at this version.
    pub name: String,
    /// Role description at this version.
    pub description: String,
    /// Priority at this version.
    pub priority: i32,
    /// Parent link at this version.
    pub inherits_role_id: Option<Uuid>,
    /// Whether inheritance was on.
    pub inherit_permissions: bool,
    /// The permission set, as `[{"key": …, "effect": …}]`.
    pub permissions: serde_json::Value,
    /// What kind of change produced this version (`created`, `updated`, `permissions`, …).
    pub change: String,
    /// Who made the change (`None` = the platform or the seed).
    pub changed_by: Option<Uuid>,
    /// When the version was written.
    pub created_at: OffsetDateTime,
}

impl RoleVersion {
    /// The permission set parsed back into entries.
    #[must_use]
    pub fn entries(&self) -> Vec<RolePermission> {
        self.permissions
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let key = item.get("key")?.as_str()?.to_owned();
                        let effect = Effect::from_stored(item.get("effect")?.as_str()?).ok()?;
                        Some(RolePermission { key, effect })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One permission entry that moved between two versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChange {
    /// Permission key.
    pub key: String,
    /// Effect before the change.
    pub from: Option<Effect>,
    /// Effect after the change.
    pub to: Option<Effect>,
}

/// What changed between two permission sets (the matrix's diff preview and the history tab).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleDiff {
    /// Keys the newer set adds.
    pub added: Vec<RolePermission>,
    /// Keys whose effect flipped.
    pub changed: Vec<PermissionChange>,
    /// Keys the newer set drops.
    pub removed: Vec<RolePermission>,
}

impl RoleDiff {
    /// `true` when the two sets are equal.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// How many entries the diff touches.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }
}

/// How a role update changes the parent link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParentChange {
    /// Leave the link alone.
    #[default]
    Keep,
    /// Point the role at a new parent.
    Set(Uuid),
    /// Detach the role from its parent.
    Clear,
}

/// A partial update of a role (the fields a caller left out are kept).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleUpdate {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New priority.
    pub priority: Option<i32>,
    /// What to do with the parent link.
    pub parent: ParentChange,
    /// Whether inherited permissions apply.
    pub inherit_permissions: Option<bool>,
}

/// The outcome of a matrix save: what the role holds now and how it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSaveOutcome {
    /// The role after the save.
    pub role: Role,
    /// The permission set as written.
    pub entries: Vec<RolePermission>,
    /// The diff against what the role held before.
    pub diff: RoleDiff,
    /// The version number the save wrote.
    pub version: i32,
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
            Scope::Department {
                organization_id,
                department: "marketing".to_owned(),
            },
            Scope::Module {
                organization_id: Some(organization_id),
                site_id: None,
                module: "content".to_owned(),
            },
            Scope::Resource {
                organization_id: Some(organization_id),
                site_id: Some(site_id),
                resource_type: "path".to_owned(),
                resource_id: "/blog/*".to_owned(),
            },
        ];
        for scope in cases {
            let rebuilt = Scope::from_parts(
                scope.scope_type(),
                scope.organization_id(),
                scope.site_id(),
                scope.resource_type(),
                scope.resource_id(),
            )
            .expect("valid shape");
            assert_eq!(rebuilt, scope);
        }

        assert!(
            Scope::from_parts("organization", None, None, None, None).is_err(),
            "an organization scope needs an organization"
        );
        assert!(
            Scope::from_parts("site", Some(organization_id), None, None, None).is_err(),
            "a site scope needs a site"
        );
        assert!(
            Scope::from_parts("department", Some(organization_id), None, None, None).is_err(),
            "a department scope needs the department key"
        );
        assert!(
            Scope::from_parts("module", None, None, None, Some("content")).is_err(),
            "a module scope needs an organization or a site"
        );
        assert!(Scope::from_parts("galaxy", None, None, None, None).is_err());
    }

    #[test]
    fn scopes_match_their_context() {
        let organization_id = Uuid::new_v4();
        let site_id = Uuid::new_v4();
        let other_site = Uuid::new_v4();

        let context = ResourceContext {
            organization_id: Some(organization_id),
            site_id: Some(site_id),
            department: Some("marketing".to_owned()),
            module: Some("content".to_owned()),
            path: Some("/blog/hello-world".to_owned()),
        };

        assert!(Scope::Global.applies_to(&context));
        assert!(
            Scope::Organization { organization_id }.applies_to(&context),
            "the organization matches"
        );
        assert!(
            !Scope::Organization {
                organization_id: Uuid::new_v4()
            }
            .applies_to(&context),
            "another organization does not"
        );
        assert!(
            Scope::Site {
                organization_id: None,
                site_id
            }
            .applies_to(&context)
        );
        assert!(
            !Scope::Site {
                organization_id: None,
                site_id: other_site
            }
            .applies_to(&context),
            "another site does not"
        );
        assert!(
            Scope::Department {
                organization_id,
                department: "marketing".to_owned()
            }
            .applies_to(&context)
        );
        assert!(
            !Scope::Department {
                organization_id,
                department: "sales".to_owned()
            }
            .applies_to(&context),
            "another department does not"
        );
        assert!(
            Scope::Module {
                organization_id: Some(organization_id),
                site_id: None,
                module: "content".to_owned()
            }
            .applies_to(&context)
        );
        assert!(
            !Scope::Module {
                organization_id: Some(organization_id),
                site_id: None,
                module: "analytics".to_owned()
            }
            .applies_to(&context),
            "another module does not"
        );
        assert!(
            Scope::Resource {
                organization_id: Some(organization_id),
                site_id: Some(site_id),
                resource_type: "path".to_owned(),
                resource_id: "/blog/*".to_owned()
            }
            .applies_to(&context),
            "the path glob matches"
        );
        assert!(
            !Scope::Resource {
                organization_id: Some(organization_id),
                site_id: Some(site_id),
                resource_type: "path".to_owned(),
                resource_id: "/legal/*".to_owned()
            }
            .applies_to(&context),
            "another path does not"
        );
    }

    #[test]
    fn fine_scopes_never_apply_without_their_context() {
        let organization_id = Uuid::new_v4();
        let plain = ResourceContext {
            organization_id: Some(organization_id),
            ..ResourceContext::default()
        };

        assert!(
            !Scope::Department {
                organization_id,
                department: "marketing".to_owned()
            }
            .applies_to(&plain),
            "a department binding needs the department"
        );
        assert!(
            !Scope::Module {
                organization_id: Some(organization_id),
                site_id: None,
                module: "content".to_owned()
            }
            .applies_to(&plain),
            "a module binding needs the module"
        );
        assert!(
            !Scope::Resource {
                organization_id: Some(organization_id),
                site_id: None,
                resource_type: "path".to_owned(),
                resource_id: "/blog/*".to_owned()
            }
            .applies_to(&plain),
            "a path binding needs the path"
        );
        assert!(
            !Scope::Resource {
                organization_id: Some(organization_id),
                site_id: None,
                resource_type: "unknown-kind".to_owned(),
                resource_id: "*".to_owned()
            }
            .applies_to(&ResourceContext {
                organization_id: Some(organization_id),
                path: Some("/anything".to_owned()),
                ..ResourceContext::default()
            }),
            "an unknown resource kind never matches, a bare wildcard included"
        );
    }

    #[test]
    fn binding_expiry_is_evaluated_against_now() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut binding = RoleBinding {
            id: Uuid::nil(),
            role_id: Uuid::nil(),
            subject: Subject::User(Uuid::nil()),
            user_id: Some(Uuid::nil()),
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
            revoked_at: None,
            created_at: now,
        };
        assert!(binding.is_active_at(now), "a plain binding is active");
        assert!(!binding.is_expired_at(now), "nothing has run out yet");

        binding.expires_at = Some(now - time::Duration::seconds(1));
        assert!(
            !binding.is_active_at(now),
            "an expired binding is not active"
        );
        assert!(
            binding.is_expired_at(now),
            "it is reported as run out, not as revoked"
        );

        binding.expires_at = Some(now + time::Duration::seconds(1));
        assert!(binding.is_active_at(now));
        assert!(!binding.is_expired_at(now));

        binding.revoked_at = Some(now);
        assert!(
            !binding.is_active_at(now),
            "a revoked binding is not active"
        );
        assert!(
            !binding.is_expired_at(now),
            "a revoked binding is not merely expired"
        );
    }

    #[test]
    fn subjects_round_trip_through_storage() {
        let id = Uuid::new_v4();
        for subject in [
            Subject::User(id),
            Subject::Group(id),
            Subject::ServiceAccount(id),
        ] {
            assert_eq!(
                Subject::from_parts(subject.subject_type(), subject.id()).expect("valid"),
                subject
            );
        }
        assert!(Subject::from_parts("robot", id).is_err());
    }
}
