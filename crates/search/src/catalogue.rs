//! The registry of searchable sources.
//!
//! Every panel surface that can be searched registers a [`SourceSpec`] here: a stable key, the
//! human title the palette groups results under, the permission a caller must hold to see hits
//! from it, and where a hit lives in the panel.
//!
//! The rule this registry encodes: **a source is only registered once its screen exists.** The
//! palette never shows a group whose results have nowhere to go — the owner's brief asks the
//! search box to find "everything", and "everything" grows as the platform grows (users arrive
//! with the IAM screens of REQ-006, orders with the commerce engine, logs with the audit
//! centre…). Adding a source is one entry here plus one `search_*` function in [`crate::sources`].
//!
//! Permission filtering is per source, not per endpoint: a caller sees exactly the groups the
//! IAM graph grants them, and a group they cannot read is reported as skipped with the reason.

/// One searchable surface of the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpec {
    /// Stable key, lower-case (`pages`). It is the group id in the API and the palette.
    pub key: &'static str,
    /// Title the palette groups hits under (`Pages`).
    pub title: &'static str,
    /// Permission a caller must hold for this source's hits to be listed (docs/07-IAM.md).
    pub permission: &'static str,
    /// One line describing what a hit of this source is, shown under a group's name.
    pub hint: &'static str,
}

/// Every source the platform can search today, in the order the palette lists them.
pub const SEARCH_SOURCES: &[SourceSpec] = &[
    SourceSpec {
        key: "pages",
        title: "Pages",
        permission: "content.pages.read",
        hint: "Titles, summaries and slugs of the content of your sites",
    },
    SourceSpec {
        key: "media",
        title: "Media",
        permission: "media.read",
        hint: "File names in the media libraries of your sites",
    },
    SourceSpec {
        key: "sites",
        title: "Sites",
        permission: "sites.read",
        hint: "The sites of your organization",
    },
];

/// Look one source up by its key.
#[must_use]
pub fn source(key: &str) -> Option<&'static SourceSpec> {
    SEARCH_SOURCES.iter().find(|spec| spec.key == key)
}

/// Keys of every registered source, in palette order.
#[must_use]
pub fn source_keys() -> Vec<&'static str> {
    SEARCH_SOURCES.iter().map(|spec| spec.key).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_lower_case() {
        let mut keys: Vec<&str> = source_keys();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "source keys must be unique");
        for key in keys {
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{key} must be a lower-case handle"
            );
        }
    }

    #[test]
    fn every_source_carries_a_permission() {
        for spec in SEARCH_SOURCES {
            assert!(
                spec.permission.contains('.'),
                "{} must name a dotted permission key",
                spec.key
            );
            assert!(!spec.title.is_empty() && !spec.hint.is_empty());
        }
    }

    #[test]
    fn lookup_by_key() {
        assert!(source("pages").is_some());
        assert!(source("nope").is_none());
    }
}
