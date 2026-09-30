//! The themes this installation bundles (`themes/<key>/omnion.theme.json`).
//!
//! The first-run wizard and the `omnion` CLI both offer this list, so the key a site is set to
//! answers a real question ("which of my themes?") instead of a slug the renderer may not know.
//! Installing third-party themes (the marketplace) extends the list; the renderer keeps falling
//! back to the default theme for a key that is no longer bundled.
//!
//! ## Why this list and not the filesystem
//!
//! `themes/*/omnion.theme.json` is the real catalogue and the gallery mirrors it into the
//! `themes` table at boot. This constant exists for the one caller that has no database yet —
//! the first-run wizard, which asks "which theme?" before anything has been written — and for
//! the CLI, which runs with no API at all.
//!
//! That makes the constant a **second** source, and a second source drifts. So the two are
//! checked against each other instead of trusted: `the_catalog_matches_the_filesystem` reads
//! every `themes/<key>/omnion.theme.json` off disk and asserts the set of keys is exactly this
//! list, and `every_alias_points_at_a_theme_this_build_ships` does the same for the aliases the
//! renderer's registry resolves. A theme added under `themes/` without a line here, or an
//! alias aimed at a theme that was renamed, fails `cargo test` rather than producing a wizard
//! that offers a theme the gallery does not have.
//!
//! The aliases live here for the same reason they live in `apps/web/lib/theme.ts`: the wizard
//! asks a site to choose from a stable set of names, and an installation pinned to `saas` must
//! still find a theme under the key it ships now.

use omnion_identity::DEFAULT_THEME;

/// One bundled theme, ready for a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledTheme {
    /// Manifest key (`themes/<key>`).
    pub key: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One line describing the look.
    pub description: &'static str,
    /// Older or alternate keys that resolve to this theme.
    pub aliases: &'static [&'static str],
}

/// Themes shipped with the platform (docs/03-FRONTEND.md).
pub const BUNDLED_THEMES: &[BundledTheme] = &[
    BundledTheme {
        key: "agency",
        name: "Agency",
        description: "A studio site: an oversized display line, wide gutters and a card grid that breaks the measure.",
        aliases: &["studio", "creative"],
    },
    BundledTheme {
        key: "commerce",
        name: "Commerce",
        description: "A storefront: a compact utility header, product grids and a price set in a tabular figure.",
        aliases: &["restaurant", "shop", "store"],
    },
    BundledTheme {
        key: "corporate",
        name: "Corporate",
        description: "An institutional company site: a full-width masthead, a narrow reading column and section rules.",
        aliases: &["business", "enterprise"],
    },
    BundledTheme {
        key: "documentation",
        name: "Documentation",
        description: "Docs: a sticky table of contents rail, a monospace affordance line and tight line height.",
        aliases: &["docs", "manual"],
    },
    BundledTheme {
        key: "government",
        name: "Government",
        description: "A public-sector portal: a three-part institutional header, high contrast and a visible focus ring.",
        aliases: &["nonprofit", "public", "civic"],
    },
    BundledTheme {
        key: "magazine",
        name: "Magazine",
        description: "An editorial site: a serif display face, a rule above every section and a standfirst.",
        aliases: &["editorial", "news", "blog"],
    },
    BundledTheme {
        key: "minimal",
        name: "Minimal",
        description: "A quiet, typographic theme: one column, generous spacing, light and dark.",
        aliases: &[],
    },
    BundledTheme {
        key: "portfolio",
        name: "Portfolio",
        description: "A personal site: one column, the name set large, and work presented as numbered entries.",
        aliases: &["personal", "freelance"],
    },
    BundledTheme {
        key: "startup",
        name: "Startup",
        description: "A launch site: a centred canvas, an oversized centred headline and a two-step call to action.",
        aliases: &["launch", "landing"],
    },
    BundledTheme {
        key: "tech",
        name: "Tech",
        description: "A product site for software: a dark-canvas hero, a mono accent line and a dense type scale.",
        aliases: &["saas", "startup-soft"],
    },
];

/// The theme a fresh installation renders with.
#[must_use]
pub fn default_theme() -> &'static str {
    DEFAULT_THEME
}

/// Look a bundled theme up by key; the key is normalized the way [`omnion_identity::validate_theme`]
/// normalizes it, so `Minimal` and ` minimal ` find the same theme.
#[must_use]
pub fn find(key: &str) -> Option<&'static BundledTheme> {
    let wanted = key.trim().to_lowercase();
    BUNDLED_THEMES
        .iter()
        .find(|theme| theme.key == wanted || theme.aliases.contains(&wanted.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// The repository root, found by walking up from this file's `CARGO_MANIFEST_DIR`.
    ///
    /// The themes are at the repository root, not inside this crate, and a test that hard-codes
    /// a relative path breaks the moment the crate is built from anywhere but the root — which
    /// is what a `cargo test -p omnion-onboarding` from a worktree does.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|dir| dir.join("themes").is_dir() && dir.join("pnpm-workspace.yaml").is_file())
            .map_or_else(
                || panic!("no repository root above {}", env!("CARGO_MANIFEST_DIR")),
                Path::to_path_buf,
            )
    }

    /// Every `themes/<key>/omnion.theme.json` on disk.
    fn shipped_manifests() -> Vec<(String, serde_json::Value)> {
        let root = repo_root().join("themes");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&root).expect("the themes directory ships") {
            let dir = entry.expect("a readable themes entry").path();
            if !dir.is_dir() {
                continue;
            }
            let manifest = dir.join("omnion.theme.json");
            if !manifest.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|error| panic!("{}: {error}", manifest.display()));
            let value: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: {error}", manifest.display()));
            found.push((
                dir.file_name().unwrap().to_string_lossy().into_owned(),
                value,
            ));
        }
        found
    }

    #[test]
    fn the_default_theme_is_bundled() {
        assert!(
            find(default_theme()).is_some(),
            "the default theme {} must be a bundled theme",
            default_theme()
        );
    }

    #[test]
    fn lookups_normalize_the_key() {
        assert_eq!(find(" Minimal ").expect("bundled").key, "minimal");
        assert!(find("starter").is_none());
        assert!(
            !BUNDLED_THEMES.is_empty(),
            "an installation always bundles at least one theme"
        );
    }

    #[test]
    fn an_alias_resolves_to_its_theme() {
        // The owner's brief names themes by keys this build does not ship (`editorial`,
        // `restaurant`, `saas`, `nonprofit`, `docs`). An installation pinned to one of those
        // has to keep rendering, so the alias is the compatibility promise and it is tested
        // rather than assumed.
        for (alias, expected) in [
            ("editorial", "magazine"),
            ("restaurant", "commerce"),
            ("saas", "tech"),
            ("nonprofit", "government"),
            ("docs", "documentation"),
        ] {
            assert_eq!(
                find(alias).map(|theme| theme.key),
                Some(expected),
                "alias {alias} must resolve to {expected}"
            );
        }
    }

    /// The constant and the filesystem are two sources; this is what keeps them one.
    #[test]
    fn the_catalog_matches_the_filesystem() {
        let shipped: BTreeSet<String> = shipped_manifests()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let listed: BTreeSet<String> = BUNDLED_THEMES
            .iter()
            .map(|theme| theme.key.to_owned())
            .collect();

        assert_eq!(
            shipped, listed,
            "themes/ and BUNDLED_THEMES disagree: a theme added under themes/ needs a line in \
             crates/onboarding/src/themes.rs, and one removed there needs its directory deleted. \
             A wizard that offers a theme the gallery does not have is worse than one that \
             offers fewer."
        );
        assert_eq!(
            shipped.len(),
            10,
            "docs/03-FRONTEND.md ships ten themes; the catalogue says {}",
            shipped.len()
        );
    }

    #[test]
    fn every_key_and_alias_is_distinct() {
        // Two themes answering to the same alias means the resolution order decides which one
        // an existing site gets, and that order is an accident of array position.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for theme in BUNDLED_THEMES {
            for name in std::iter::once(theme.key).chain(theme.aliases.iter().copied()) {
                assert!(
                    seen.insert(name),
                    "{name} is claimed by more than one theme in BUNDLED_THEMES"
                );
            }
        }
    }

    #[test]
    fn a_manifest_agrees_with_its_catalogue_entry() {
        for (dir, manifest) in shipped_manifests() {
            let listed =
                find(&dir).unwrap_or_else(|| panic!("themes/{dir} has no entry in BUNDLED_THEMES"));
            assert_eq!(
                manifest["key"].as_str(),
                Some(listed.key),
                "themes/{dir}/omnion.theme.json declares a different key than the catalogue"
            );
            assert_eq!(
                manifest["name"].as_str(),
                Some(listed.name),
                "themes/{dir} display name differs between the manifest and the catalogue"
            );
            assert_eq!(
                manifest["description"].as_str(),
                Some(listed.description),
                "themes/{dir} description differs between the manifest and the catalogue"
            );
            let declared: Vec<&str> = manifest["aliases"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                declared, listed.aliases,
                "themes/{dir} alias list differs between the manifest and the catalogue"
            );
        }
    }

    /// Acceptance 2: every bundled manifest validates against the v2 shape and declares both
    /// modes.
    #[test]
    fn every_manifest_is_v2_and_declares_both_modes() {
        for (dir, manifest) in shipped_manifests() {
            let shape = omnion_content::themes::manifest_shape(&manifest)
                .unwrap_or_else(|error| panic!("themes/{dir}/omnion.theme.json: {error}"));
            assert_eq!(
                shape.modes,
                vec!["light", "dark"],
                "themes/{dir} must declare both colour modes — a theme with one is a theme \
                 whose dark mode is somebody else's guess"
            );
            assert!(
                shape.slots >= 8,
                "themes/{dir} declares {} layout slots; the Theme Builder's picker offers eight",
                shape.slots
            );
            assert!(
                shape.tokens >= 5,
                "themes/{dir} declares {} tokens; the customize screen needs a palette to edit",
                shape.tokens
            );
            for required in ["settingsSchema", "compatibility", "previewImage"] {
                assert!(
                    manifest.get(required).is_some(),
                    "themes/{dir} is missing the v2 field {required}"
                );
            }
        }
    }
}
