//! Seeding: the rows the platform itself owns.
//!
//! On boot the service makes the database agree with the code: the permission catalogue is
//! upserted, the six base roles of docs/07-IAM.md §3 exist, and — once accounts exist — at
//! least one Owner binding does too (docs/07-IAM.md §20, "at least one Owner must exist").
//! Everything here is idempotent, so it runs on every start.
//!
//! The base roles form a priority ladder without inheritance links; customers build
//! inheritance chains on top of them (docs/07-IAM.md §4). Base roles keep their default
//! permission sets once created — only the Owner role follows the catalogue, because it is
//! the role that holds everything.

use sqlx::PgPool;
use uuid::Uuid;

use crate::catalogue::{self, CATALOGUE};
use crate::error::{PermissionsError, Result};
use crate::model::{Effect, NewBinding, Scope};
use crate::{bindings, roles};

/// Permission set of a base role.
enum BasePermissions {
    /// Every key of the catalogue (Owner, and in v0 the Administrator that mirrors it — the
    /// platform-level powers reserved to Owner, docs/07-IAM.md §13, become their own keys once
    /// the organization, billing and security surfaces exist).
    All,
    /// An explicit list of keys.
    List(&'static [&'static str]),
}

impl BasePermissions {
    /// The keys this set expands to.
    fn keys(&self) -> Vec<&'static str> {
        match self {
            Self::All => catalogue::keys(),
            Self::List(list) => list.to_vec(),
        }
    }
}

/// A base role shipped with every installation (docs/07-IAM.md §3).
struct BaseRole {
    key: &'static str,
    name: &'static str,
    priority: i32,
    description: &'static str,
    permissions: BasePermissions,
}

/// The base role ladder: Owner 1000 → Member 100.
const BASE_ROLES: &[BaseRole] = &[
    BaseRole {
        key: "owner",
        name: "Owner",
        priority: 1000,
        description: "Full control of the platform, including security and billing.",
        permissions: BasePermissions::All,
    },
    BaseRole {
        key: "administrator",
        name: "Administrator",
        priority: 900,
        description: "Runs the platform day to day.",
        permissions: BasePermissions::All,
    },
    BaseRole {
        key: "manager",
        name: "Manager",
        priority: 700,
        description: "Manages content, media and the team that produces them.",
        permissions: BasePermissions::List(&[
            "content.pages.read",
            "content.pages.create",
            "content.pages.update",
            "content.pages.delete",
            "content.pages.publish",
            "content.pages.schedule",
            "content.pages.restore",
            "media.read",
            "media.upload",
            "media.update",
            "media.delete",
            "media.manage",
            "ai.providers.read",
            "ai.chat",
            "workflows.read",
            "workflows.manage",
            "workflows.run",
            "users.read",
            "users.update",
            "iam.permissions.read",
            "iam.roles.read",
            "iam.bindings.read",
            "plugins.read",
            "deployment.read",
            "deployment.preview",
            "audit.read",
            "organizations.read",
            "sites.read",
            "sites.create",
            "sites.update",
            "domains.manage",
            "webhooks.read",
            "events.read",
        ]),
    },
    BaseRole {
        key: "moderator",
        name: "Moderator",
        priority: 500,
        description: "Keeps content healthy and can publish and restore.",
        permissions: BasePermissions::List(&[
            "content.pages.read",
            "content.pages.update",
            "content.pages.publish",
            "content.pages.schedule",
            "content.pages.restore",
            "media.read",
            "media.update",
            "ai.chat",
            "workflows.read",
            "workflows.run",
            "users.read",
            "audit.read",
            "sites.read",
        ]),
    },
    BaseRole {
        key: "editor",
        name: "Editor",
        priority: 300,
        description: "Writes and publishes content.",
        permissions: BasePermissions::List(&[
            "content.pages.read",
            "content.pages.create",
            "content.pages.update",
            "content.pages.publish",
            "content.pages.schedule",
            "media.read",
            "media.upload",
            "media.update",
            "ai.chat",
            "workflows.read",
            "sites.read",
        ]),
    },
    BaseRole {
        key: "member",
        name: "Member",
        priority: 100,
        description: "Reads content and media.",
        permissions: BasePermissions::List(&["content.pages.read", "media.read"]),
    },
];

/// What [`ensure`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedReport {
    /// Catalogue rows written.
    pub permissions: usize,
    /// Base roles created by this run.
    pub roles_created: usize,
    /// `true` when the Owner role picked up newly catalogued permissions.
    pub owner_synced: bool,
    /// Account that received the fallback Owner binding, when one was written.
    pub owner_bound: Option<Uuid>,
}

/// Upsert the permission catalogue. Returns the number of rows written.
pub async fn seed_catalogue(pool: &PgPool) -> Result<usize> {
    for entry in CATALOGUE {
        sqlx::query(
            "insert into permissions (key, category, description) values ($1, $2, $3) \
             on conflict (key) do update set category = excluded.category, \
             description = excluded.description",
        )
        .bind(entry.key)
        .bind(entry.category)
        .bind(entry.description)
        .execute(pool)
        .await?;
    }

    Ok(CATALOGUE.len())
}

/// Create the base roles that are missing and keep the Owner role complete.
///
/// Returns `(roles created, owner gained permissions)`.
pub async fn seed_base_roles(pool: &PgPool) -> Result<(usize, bool)> {
    let mut created = 0;
    let mut owner_gained = false;

    for base in BASE_ROLES {
        let keys = base.permissions.keys();
        for key in &keys {
            // A typo in the table below must fail at boot, not silently grant nothing.
            catalogue::expect_known(key)?;
        }

        let role = match roles::find_role_by_key(pool, None, base.key).await? {
            Some(existing) => existing,
            None => match roles::insert_system_role(
                pool,
                base.key,
                base.name,
                base.description,
                base.priority,
            )
            .await
            {
                Ok(role) => {
                    for key in &keys {
                        roles::add_entry(pool, role.id, key, Effect::Allow).await?;
                    }
                    created += 1;
                    role
                }
                // Another instance is seeding at the same moment (two processes booting, or two
                // test suites in parallel): its row is as good as ours, so take it.
                Err(PermissionsError::RoleKeyTaken) => {
                    roles::find_role_by_key(pool, None, base.key)
                        .await?
                        .ok_or(PermissionsError::RoleKeyTaken)?
                }
                Err(other) => return Err(other),
            },
        };

        if base.key == "owner" {
            for entry in CATALOGUE {
                owner_gained |= roles::ensure_allow_entry(pool, role.id, entry.key).await?;
            }
        }
    }

    Ok((created, owner_gained))
}

/// Give one account the platform Owner role (idempotent).
pub async fn bind_owner(pool: &PgPool, user_id: Uuid) -> Result<Option<Uuid>> {
    let owner = roles::find_role_by_key(pool, None, "owner")
        .await?
        .ok_or(crate::error::PermissionsError::RoleNotFound)?;

    let binding = bindings::grant_if_missing(
        pool,
        NewBinding {
            role_id: owner.id,
            user_id,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await?;

    Ok(binding.map(|binding| binding.id))
}

/// Guarantee the "at least one Owner" invariant.
///
/// When no live Owner binding exists and accounts do, the earliest active account receives it:
/// an installation that bootstrapped before roles existed must not end up locked out of its own
/// IAM surface. Returns the account that was bound.
pub async fn ensure_owner_binding(pool: &PgPool) -> Result<Option<Uuid>> {
    let has_owner: bool = sqlx::query_scalar(
        "select exists ( \
             select 1 from role_bindings b \
             join roles r on r.id = b.role_id \
             where r.key = 'owner' and r.organization_id is null \
               and b.revoked_at is null and (b.expires_at is null or b.expires_at > now()))",
    )
    .fetch_one(pool)
    .await?;

    if has_owner {
        return Ok(None);
    }

    let earliest: Option<Uuid> = sqlx::query_scalar(
        "select id from users where status = 'active' order by created_at asc, id asc limit 1",
    )
    .fetch_optional(pool)
    .await?;

    let Some(user_id) = earliest else {
        return Ok(None);
    };

    bind_owner(pool, user_id).await?;
    Ok(Some(user_id))
}

/// Make the IAM tables agree with the code (catalogue, base roles, Owner invariant).
pub async fn ensure(pool: &PgPool) -> Result<SeedReport> {
    let permissions = seed_catalogue(pool).await?;
    let (roles_created, owner_synced) = seed_base_roles(pool).await?;
    let owner_bound = ensure_owner_binding(pool).await?;

    Ok(SeedReport {
        permissions,
        roles_created,
        owner_synced,
        owner_bound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn base_roles_are_unique_and_ordered() {
        let mut keys = BTreeSet::new();
        let mut priorities = BTreeSet::new();
        for base in BASE_ROLES {
            assert!(keys.insert(base.key), "duplicate base role {}", base.key);
            assert!(
                priorities.insert(base.priority),
                "duplicate priority {}",
                base.priority
            );
            assert!(!base.name.trim().is_empty());
            assert!(!base.description.trim().is_empty());
        }

        // docs/07-IAM.md §3: Owner 1000, Administrator 900, Manager 700, Moderator 500,
        // Editor 300, Member 100.
        let ladder: Vec<(i32, &str)> = BASE_ROLES
            .iter()
            .map(|base| (base.priority, base.key))
            .collect();
        assert_eq!(
            ladder,
            vec![
                (1000, "owner"),
                (900, "administrator"),
                (700, "manager"),
                (500, "moderator"),
                (300, "editor"),
                (100, "member"),
            ]
        );
    }

    #[test]
    fn every_seeded_permission_is_in_the_catalogue() {
        for base in BASE_ROLES {
            for key in base.permissions.keys() {
                assert!(
                    catalogue::is_known(key),
                    "base role {} references unknown permission {key}",
                    base.key
                );
            }
        }
    }

    #[test]
    fn the_owner_and_administrator_roles_hold_the_whole_catalogue() {
        for base in BASE_ROLES {
            let keys = base.permissions.keys();
            if matches!(base.permissions, BasePermissions::All) {
                assert_eq!(
                    keys.len(),
                    CATALOGUE.len(),
                    "{} must expand to the whole catalogue",
                    base.key
                );
            }
        }

        let owner = BASE_ROLES
            .iter()
            .find(|base| base.key == "owner")
            .expect("the owner role is part of the ladder");
        assert!(
            owner.permissions.keys().contains(&"iam.roles.manage"),
            "the owner role must be able to manage roles"
        );
    }

    #[test]
    fn the_tenancy_keys_reach_the_operational_roles() {
        // P04: the site switcher and site settings need `sites.read`; the manager ladder edits
        // sites and their domains; the member role stays content-and-media read-only.
        let keys_of = |key: &str| {
            BASE_ROLES
                .iter()
                .find(|base| base.key == key)
                .expect("base role exists")
                .permissions
                .keys()
        };

        for role in ["manager", "moderator", "editor"] {
            assert!(keys_of(role).contains(&"sites.read"), "{role} reads sites");
        }
        for key in [
            "organizations.read",
            "sites.read",
            "sites.create",
            "sites.update",
            "domains.manage",
        ] {
            assert!(keys_of("manager").contains(&key), "manager holds {key}");
        }
        assert!(
            !keys_of("member").contains(&"sites.read"),
            "members stay on content and media"
        );
    }

    #[test]
    fn the_workflow_keys_reach_the_operational_roles() {
        // P09: workflows are written and started by the manager ladder; a moderator may start
        // one without editing definitions; an editor reads them; a member sees none.
        let keys_of = |key: &str| {
            BASE_ROLES
                .iter()
                .find(|base| base.key == key)
                .expect("base role exists")
                .permissions
                .keys()
        };

        for key in ["workflows.read", "workflows.manage", "workflows.run"] {
            assert!(keys_of("manager").contains(&key), "manager holds {key}");
        }
        assert!(keys_of("moderator").contains(&"workflows.run"));
        assert!(!keys_of("moderator").contains(&"workflows.manage"));
        assert!(keys_of("editor").contains(&"workflows.read"));
        assert!(!keys_of("editor").contains(&"workflows.run"));
        assert!(!keys_of("member").contains(&"workflows.read"));
    }

    #[test]
    fn the_ai_keys_reach_the_operational_roles() {
        // P11: everyone who writes content can talk to the platform's AI; connecting providers
        // stays with the administrator ladder (Owner/Administrator hold the whole catalogue) and
        // the manager role, which runs the platform day to day.
        let keys_of = |key: &str| {
            BASE_ROLES
                .iter()
                .find(|base| base.key == key)
                .expect("base role exists")
                .permissions
                .keys()
        };

        assert!(keys_of("manager").contains(&"ai.providers.read"));
        assert!(keys_of("manager").contains(&"ai.chat"));
        assert!(!keys_of("manager").contains(&"ai.providers.manage"));

        for role in ["moderator", "editor"] {
            assert!(keys_of(role).contains(&"ai.chat"), "{role} uses the chat");
            assert!(
                !keys_of(role).contains(&"ai.providers.read"),
                "{role} does not read the provider settings"
            );
        }
        assert!(!keys_of("member").contains(&"ai.chat"));
    }

    #[test]
    fn the_lower_roles_do_not_hold_management_permissions() {
        let member = BASE_ROLES
            .iter()
            .find(|base| base.key == "member")
            .expect("member exists");
        let keys = member.permissions.keys();
        assert_eq!(keys, vec!["content.pages.read", "media.read"]);
        assert!(!keys.contains(&"iam.roles.manage"));
        assert!(!keys.contains(&"users.delete"));

        let editor = BASE_ROLES
            .iter()
            .find(|base| base.key == "editor")
            .expect("editor exists");
        let keys = editor.permissions.keys();
        assert!(
            !keys.contains(&"content.pages.delete"),
            "editors do not delete"
        );
        assert!(
            !keys.contains(&"users.read"),
            "editors do not read accounts"
        );
    }
}
