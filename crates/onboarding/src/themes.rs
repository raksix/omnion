//! The themes this installation bundles (`themes/<key>/omnion.theme.json`).
//!
//! The first-run wizard and the `omnion` CLI both offer this list, so the key a site is set to
//! answers a real question ("which of my themes?") instead of a slug the renderer may not know.
//! Installing third-party themes (the marketplace) extends the list; the renderer keeps falling
//! back to the default theme for a key that is no longer bundled.

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
}

/// Themes shipped with the platform (docs/03-FRONTEND.md).
pub const BUNDLED_THEMES: &[BundledTheme] = &[BundledTheme {
    key: "minimal",
    name: "Minimal",
    description: "A quiet, typographic theme: one column, generous spacing, light and dark.",
}];

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
    BUNDLED_THEMES.iter().find(|theme| theme.key == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
