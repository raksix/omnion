//! The permission catalogue — every permission the platform knows.
//!
//! One row per permission, seeded into the `permissions` table on boot so role entries can
//! hold a foreign key. Keys follow `domain.action` (docs/07-IAM.md §2) and this list is the
//! single source of truth: a role entry, a route guard or a seed that references a key
//! outside the catalogue is a bug, and the store rejects it.
//!
//! Categories group the keys for the admin UI (`content`, `media`, `users`, `plugins`,
//! `deployment`, `iam`, `audit`, `tenancy`, `webhooks`, `events`).

/// A single permission the platform understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionDef {
    /// Stable key, e.g. `content.pages.publish`.
    pub key: &'static str,
    /// UI grouping, e.g. `content`.
    pub category: &'static str,
    /// What the permission allows, in product language.
    pub description: &'static str,
}

/// Every permission of the platform (docs/07-IAM.md §2 plus the IAM and audit surfaces the
/// API already exposes).
pub const CATALOGUE: &[PermissionDef] = &[
    // Content (docs/07-IAM.md §2 "Content").
    PermissionDef {
        key: "content.pages.read",
        category: "content",
        description: "Read pages and their revisions",
    },
    PermissionDef {
        key: "content.pages.create",
        category: "content",
        description: "Create pages",
    },
    PermissionDef {
        key: "content.pages.update",
        category: "content",
        description: "Edit page content",
    },
    PermissionDef {
        key: "content.pages.delete",
        category: "content",
        description: "Delete pages",
    },
    PermissionDef {
        key: "content.pages.publish",
        category: "content",
        description: "Publish or unpublish pages",
    },
    PermissionDef {
        key: "content.pages.schedule",
        category: "content",
        description: "Schedule publication",
    },
    PermissionDef {
        key: "content.pages.restore",
        category: "content",
        description: "Restore an earlier revision",
    },
    // Media.
    PermissionDef {
        key: "media.read",
        category: "media",
        description: "Read the media library",
    },
    PermissionDef {
        key: "media.upload",
        category: "media",
        description: "Upload files",
    },
    PermissionDef {
        key: "media.update",
        category: "media",
        description: "Edit media metadata",
    },
    PermissionDef {
        key: "media.delete",
        category: "media",
        description: "Delete files",
    },
    PermissionDef {
        key: "media.manage",
        category: "media",
        description: "Manage folders and storage settings",
    },
    // AI Hub (docs/06-AI-HUB.md §1, §7): connecting providers is an administrator-level power,
    // while using the platform's AI chat is an everyday one — the AI Hub's own permission sets
    // (§8) build on these keys in later phases.
    PermissionDef {
        key: "ai.providers.read",
        category: "ai",
        description: "Read AI providers and the model registry",
    },
    PermissionDef {
        key: "ai.providers.manage",
        category: "ai",
        description: "Connect and configure AI providers",
    },
    PermissionDef {
        key: "ai.chat",
        category: "ai",
        description: "Use the platform's AI chat",
    },
    // Workflows (docs/requests/REQ-003): the automation surface — definitions, their runs and
    // the steps a run left behind.
    PermissionDef {
        key: "workflows.read",
        category: "workflows",
        description: "Read workflows and their run history",
    },
    PermissionDef {
        key: "workflows.manage",
        category: "workflows",
        description: "Create, edit and remove workflows",
    },
    PermissionDef {
        key: "workflows.run",
        category: "workflows",
        description: "Start and cancel workflow runs",
    },
    // Users.
    PermissionDef {
        key: "users.read",
        category: "users",
        description: "Read accounts",
    },
    PermissionDef {
        key: "users.create",
        category: "users",
        description: "Invite and create accounts",
    },
    PermissionDef {
        key: "users.update",
        category: "users",
        description: "Edit accounts",
    },
    PermissionDef {
        key: "users.delete",
        category: "users",
        description: "Delete accounts",
    },
    PermissionDef {
        key: "users.impersonate",
        category: "users",
        description: "Sign in as another account",
    },
    // Plugins.
    PermissionDef {
        key: "plugins.read",
        category: "plugins",
        description: "Browse installed plugins",
    },
    PermissionDef {
        key: "plugins.install",
        category: "plugins",
        description: "Install plugins",
    },
    PermissionDef {
        key: "plugins.update",
        category: "plugins",
        description: "Update plugins",
    },
    PermissionDef {
        key: "plugins.disable",
        category: "plugins",
        description: "Enable or disable plugins",
    },
    PermissionDef {
        key: "plugins.uninstall",
        category: "plugins",
        description: "Remove plugins",
    },
    // Deployment.
    PermissionDef {
        key: "deployment.read",
        category: "deployment",
        description: "Read deployment state and history",
    },
    PermissionDef {
        key: "deployment.preview",
        category: "deployment",
        description: "Create preview environments",
    },
    PermissionDef {
        key: "deployment.deploy",
        category: "deployment",
        description: "Deploy to an environment",
    },
    PermissionDef {
        key: "deployment.rollback",
        category: "deployment",
        description: "Roll a deployment back",
    },
    // Identity and access management.
    PermissionDef {
        key: "iam.permissions.read",
        category: "iam",
        description: "Read the permission catalogue",
    },
    PermissionDef {
        key: "iam.roles.read",
        category: "iam",
        description: "Read roles and their permission sets",
    },
    PermissionDef {
        key: "iam.roles.manage",
        category: "iam",
        description: "Create roles and change their permission sets",
    },
    PermissionDef {
        key: "iam.bindings.read",
        category: "iam",
        description: "Read role assignments",
    },
    PermissionDef {
        key: "iam.bindings.manage",
        category: "iam",
        description: "Assign and revoke roles",
    },
    // Audit.
    PermissionDef {
        key: "audit.read",
        category: "audit",
        description: "Read the audit trail",
    },
    // Tenancy (docs/01-VISION.md §10, docs/07-IAM.md §7): organizations, their sites and the
    // domains that address them. `organizations.manage` covers opening and editing tenants;
    // the platform surface stays with the Owner/Administrator ladder.
    PermissionDef {
        key: "organizations.read",
        category: "tenancy",
        description: "Read organizations",
    },
    PermissionDef {
        key: "organizations.manage",
        category: "tenancy",
        description: "Create and edit organizations",
    },
    PermissionDef {
        key: "sites.read",
        category: "tenancy",
        description: "Read sites and their domains",
    },
    PermissionDef {
        key: "sites.create",
        category: "tenancy",
        description: "Create sites",
    },
    PermissionDef {
        key: "sites.update",
        category: "tenancy",
        description: "Edit sites",
    },
    PermissionDef {
        key: "sites.delete",
        category: "tenancy",
        description: "Delete sites",
    },
    PermissionDef {
        key: "domains.manage",
        category: "tenancy",
        description: "Add, remove and promote site domains",
    },
    // Events and webhooks (docs/01-VISION.md §13, phase P12). Reading the bus is one power;
    // pointing the platform's events at an outside URL is a stronger one, because that URL
    // receives the organization's data.
    PermissionDef {
        key: "webhooks.read",
        category: "webhooks",
        description: "Read webhook endpoints and their delivery history",
    },
    PermissionDef {
        key: "webhooks.manage",
        category: "webhooks",
        description: "Connect, change, test and remove webhook endpoints",
    },
    PermissionDef {
        key: "events.read",
        category: "events",
        description: "Read the platform's event feed",
    },
    // Search (docs/requests/REQ-002). `search.read` is the box itself — every signed-in
    // account holds it, and the results are still narrowed by organization and by each
    // provider's own read permission; `search.manage` is index maintenance, not searching.
    PermissionDef {
        key: "search.read",
        category: "search",
        description: "Search the platform's indexed content",
    },
    PermissionDef {
        key: "search.manage",
        category: "search",
        description: "Rebuild and configure the search index",
    },
    // Analytics (docs/requests/REQ-007). Reading the numbers is one power; changing what a site
    // collects — and therefore what it promises its visitors — is a stronger one; exporting raw
    // rows is a third, because a report a site's operator can hand out is still the site's data.
    PermissionDef {
        key: "analytics.read",
        category: "analytics",
        description: "Read analytics reports, settings and the tracking snippet",
    },
    PermissionDef {
        key: "analytics.export",
        category: "analytics",
        description: "Export analytics reports",
    },
    PermissionDef {
        key: "analytics.goals.manage",
        category: "analytics",
        description: "Create and edit conversion goals",
    },
    PermissionDef {
        key: "analytics.settings.manage",
        category: "analytics",
        description: "Change tracking, privacy and retention settings",
    },
];

/// Look a permission up by key.
#[must_use]
pub fn get(key: &str) -> Option<&'static PermissionDef> {
    CATALOGUE.iter().find(|entry| entry.key == key)
}

/// `true` when the key exists in the catalogue.
#[must_use]
pub fn is_known(key: &str) -> bool {
    get(key).is_some()
}

/// Every key, in catalogue order.
#[must_use]
pub fn keys() -> Vec<&'static str> {
    CATALOGUE.iter().map(|entry| entry.key).collect()
}

/// Request a permission key and fail when it is outside the catalogue.
pub fn expect_known(key: &str) -> Result<&'static PermissionDef, crate::error::PermissionsError> {
    get(key).ok_or_else(|| crate::error::PermissionsError::UnknownPermission(key.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn keys_are_unique_and_well_formed() {
        let mut seen = BTreeSet::new();
        for entry in CATALOGUE {
            assert!(seen.insert(entry.key), "duplicate key {}", entry.key);
            assert!(
                entry.key.split('.').count() >= 2,
                "{} must be domain.action",
                entry.key
            );
            assert!(
                entry
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'),
                "{} must be lowercase ASCII",
                entry.key
            );
            assert!(
                !entry.description.trim().is_empty(),
                "{} needs a description",
                entry.key
            );
            assert!(
                !entry.category.trim().is_empty(),
                "{} needs a category",
                entry.key
            );
        }
    }

    #[test]
    fn every_documented_category_is_present() {
        let categories: BTreeSet<&str> = CATALOGUE.iter().map(|entry| entry.category).collect();
        for expected in [
            "content",
            "media",
            "ai",
            "workflows",
            "users",
            "plugins",
            "deployment",
            "iam",
            "audit",
            "tenancy",
            "search",
            "analytics",
        ] {
            assert!(categories.contains(expected), "missing category {expected}");
        }
    }

    #[test]
    fn the_tenancy_family_is_catalogued() {
        // docs/01-VISION.md §10 (multi-site vs multi-tenant) and docs/07-IAM.md §7: sites and
        // the domains that address them are first-class, so the keys that guard them are too.
        for key in [
            "organizations.read",
            "organizations.manage",
            "sites.read",
            "sites.create",
            "sites.update",
            "sites.delete",
            "domains.manage",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
        }

        for key in [
            "organizations.manage",
            "sites.create",
            "sites.update",
            "sites.delete",
            "domains.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("tenancy"),
                "{key} belongs to the tenancy category"
            );
        }
    }

    #[test]
    fn the_documented_permission_families_are_covered() {
        // docs/07-IAM.md §2 lists these exact families; they are the contract the admin UI
        // (and later the AI agent permission sets) build on.
        for family in [
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
            "users.read",
            "users.create",
            "users.update",
            "users.delete",
            "users.impersonate",
            "plugins.read",
            "plugins.install",
            "plugins.update",
            "plugins.disable",
            "plugins.uninstall",
            "deployment.read",
            "deployment.preview",
            "deployment.deploy",
            "deployment.rollback",
        ] {
            assert!(is_known(family), "{family} must be in the catalogue");
        }
    }

    #[test]
    fn the_workflow_family_is_catalogued() {
        // P09: the automation surface is guarded by three keys — read, manage and run — so a
        // role can be trusted to trigger a workflow without letting it rewrite definitions.
        for key in ["workflows.read", "workflows.manage", "workflows.run"] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("workflows"),
                "{key} belongs to the workflows category"
            );
        }
    }

    #[test]
    fn the_ai_family_is_catalogued() {
        // P11 (docs/06-AI-HUB.md §1): connecting a provider and using the chat are separate
        // keys, so an operator can let a team talk to the platform's AI without letting them
        // point it at another endpoint.
        for key in ["ai.providers.read", "ai.providers.manage", "ai.chat"] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("ai"),
                "{key} belongs to the ai category"
            );
        }
    }

    #[test]
    fn the_analytics_family_is_catalogued() {
        // REQ-007: seeing what a site measures, deciding what it may measure, exporting the
        // numbers and owning its conversion goals are four separate powers.
        for key in [
            "analytics.read",
            "analytics.export",
            "analytics.goals.manage",
            "analytics.settings.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("analytics"),
                "{key} belongs to the analytics category"
            );
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(get("content.pages.explode").is_none());
        assert!(expect_known("nope").is_err());
        assert!(expect_known("content.pages.read").is_ok());
        assert_eq!(keys().len(), CATALOGUE.len());
    }
}
