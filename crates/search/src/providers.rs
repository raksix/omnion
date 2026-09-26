//! The registry of search providers.
//!
//! One entry per searchable domain of the platform: the key the index stores rows under, the
//! document type those rows carry, the permission a caller needs to see them, and where a hit
//! lives in the panel. Adding a domain is one entry here plus one arm in
//! [`crate::indexer::reindex`] — the registry is the single source, and the indexer's tests
//! hold it to that.
//!
//! Providers for domains that do not exist yet are **not** listed: a palette section whose
//! rows have nowhere to go is worse than a section that is not there. Posts, plugins, themes
//! and orders register from their own requests the day their modules ship.
//!
//! `search.read` is deliberately absent here: reading one's way into a provider is not a
//! permission the platform grants per provider — it is the domain's own read key. An editor
//! without `users.read` gets no user rows because the query filters that provider out, exactly
//! as if they had opened the users screen and been refused.

/// One searchable domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    /// Stable key, lower-case (`pages`); the document rows carry it.
    pub key: &'static str,
    /// Display title the palette groups hits under (`Pages`).
    pub title: &'static str,
    /// Document type stored in `search_documents.entity_type` (`page`).
    pub entity_type: &'static str,
    /// Read permission of the domain (docs/07-IAM.md); results without it are filtered out.
    pub permission: &'static str,
    /// One line describing what a hit of this provider is.
    pub hint: &'static str,
    /// Panel route a hit opens; slice 2 adds the precise deep links.
    pub route: &'static str,
}

/// Every provider the platform indexes today, in the order the palette lists them.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        key: "pages",
        title: "Pages",
        entity_type: "page",
        permission: "content.pages.read",
        hint: "Titles, summaries and status of the content of your sites",
        route: "/pages",
    },
    ProviderSpec {
        key: "media",
        title: "Media",
        entity_type: "media",
        permission: "media.read",
        hint: "File names in the media libraries of your sites",
        route: "/media",
    },
    ProviderSpec {
        key: "users",
        title: "Users",
        entity_type: "user",
        permission: "users.read",
        hint: "The accounts of your organization",
        route: "/settings/users",
    },
    ProviderSpec {
        key: "sites",
        title: "Sites",
        entity_type: "site",
        permission: "sites.read",
        hint: "The sites of your organization",
        route: "/sites",
    },
    // The three providers of slice 3. Their documents carry the screen their target lives on
    // (an audit entry opens the page it touched, a translation opens its page's editor), which is
    // what keeps a row in the results set clickable; `/search` narrowed to the provider is the
    // one honest home for activity, because a dedicated trail screen arrives with the audit work
    // (REQ-012/REQ-039).
    ProviderSpec {
        key: "logs",
        title: "Activity",
        entity_type: "log",
        permission: "audit.read",
        hint: "Audit entries and recorded events, linked to the thing they touched",
        route: "/search",
    },
    ProviderSpec {
        key: "translations",
        title: "Translations",
        entity_type: "translation",
        permission: "content.pages.read",
        hint: "Translated fields of your pages, linked to the page they belong to",
        route: "/pages",
    },
    ProviderSpec {
        key: "settings",
        title: "Settings",
        entity_type: "setting",
        permission: "search.read",
        hint: "The key/value settings of your organization",
        route: "/settings/search",
    },
];

/// Look one provider up by its key.
#[must_use]
pub fn provider(key: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|spec| spec.key == key)
}

/// Look one provider up by the entity type its documents carry (`page` → `pages`).
#[must_use]
pub fn provider_for_entity_type(entity_type: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS
        .iter()
        .find(|spec| spec.entity_type == entity_type)
}

/// Keys of every registered provider, in palette order.
#[must_use]
pub fn provider_keys() -> Vec<&'static str> {
    PROVIDERS.iter().map(|spec| spec.key).collect()
}

/// `true` when `name` names a provider or an entity type the registry knows.
#[must_use]
pub fn is_known_type(name: &str) -> bool {
    provider(name).is_some() || provider_for_entity_type(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_types_are_unique_and_lower_case() {
        let mut keys: Vec<&str> = provider_keys();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "provider keys must be unique");

        let mut types: Vec<&str> = PROVIDERS.iter().map(|spec| spec.entity_type).collect();
        let total = types.len();
        types.sort_unstable();
        types.dedup();
        assert_eq!(types.len(), total, "entity types must be unique");

        for spec in PROVIDERS {
            for value in [spec.key, spec.entity_type] {
                assert!(
                    value
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{value} must be a lower-case handle"
                );
            }
        }
    }

    #[test]
    fn every_provider_names_a_permission_and_a_route() {
        for spec in PROVIDERS {
            assert!(
                spec.permission.contains('.'),
                "{} must name a dotted permission key",
                spec.key
            );
            assert!(
                spec.route.starts_with('/'),
                "{} needs a panel route",
                spec.key
            );
            assert!(!spec.title.is_empty() && !spec.hint.is_empty());
        }
    }

    #[test]
    fn lookup_by_key_and_by_entity_type() {
        assert!(provider("pages").is_some());
        assert!(provider("nope").is_none());
        assert_eq!(
            provider_for_entity_type("page").map(|spec| spec.key),
            Some("pages")
        );
        assert!(is_known_type("page"));
        assert!(is_known_type("pages"));
        assert!(!is_known_type("unicorn"));
    }
}
