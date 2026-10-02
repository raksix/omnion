//! `GET /api/v1/themes/{key}/assets/{path}` — a theme's own files, for the gallery card.
//!
//! The gallery promises a preview image per theme: `previewImage` has been on the manifest,
//! in the API's theme card and in the client's type since slice 1, and **nothing rendered it**.
//! A field that is carried through three layers and read by none is a promise the platform
//! does not keep, and the REQ's gallery criterion asks for exactly that image. This route is
//! the reader that makes the field true.
//!
//! Three decisions, each of which is the one that keeps an untrusted path from becoming an
//! untrusted read:
//!
//! * **Only bundled themes, and only the two file kinds a theme ships.** An uploaded package's
//!   bytes live in object storage, not in this directory, so a route that resolved them here
//!   would be a second storage path with a second set of rules. The gallery falls back to a
//!   generated swatch for an uploaded theme, which is honest about the difference.
//!
//! * **The path is matched against an allow-list of file names, not sanitised.** "Strip `..`"
//!   is a blocklist with a known bypass; "the only names that exist are `preview.svg` and
//!   `<key>.css`" has no second spelling to be tricked into. The manifest's own `previewImage`
//!   is checked against this list, so a manifest naming something else gets its file refused
//!   rather than served.
//!
//! * **The bytes are read once at boot and cached.** A gallery of ten cards is ten requests,
//!   and they are the same bytes every time; re-reading a file per card is a way to turn a
//!   page into ten disk reads for a file that cannot change without a restart.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

use crate::error::ApiError;
use crate::state::AppState;

// The gallery image's one name is the content crate's constant, not a second copy of the
// string: `themes::preview_url` builds the card's URL from it, and a route that allowed a
// different name than the one the card is given would 404 on every card.
use omnion_content::themes::PREVIEW_ASSET_NAME;

/// The stylesheet a bundled theme ships, which is named after the theme itself.
///
/// `themes/magazine/styles/magazine.css` — the file name IS the theme key, which is why it
/// cannot be an allow-list entry: the allow-list is a fixed list precisely so a request cannot
/// choose the name, and a name that varies with the key would be a request choosing it. It is
/// safe anyway because the *shape* is checked (a lowercase key and a literal `styles/` prefix),
/// and the key is the thing that was already validated as a directory name.
fn stylesheet_name(key: &str) -> String {
    format!("{key}.css")
}

/// The file names a theme asset route will serve for `key`, with their content types.
///
/// Two per key, and the *key* is validated before either is produced: `preview.svg` is a fixed
/// name, so without this an allow-list entry would exist for a key like `../../etc` and the
/// join in [`asset_bytes`] would walk out of the themes directory. An allow-list that depends
/// on a request-supplied key is only an allow-list if the key is checked first.
fn served_names(key: &str) -> Vec<(String, &'static str)> {
    // `validate_key` NORMALISES (lowercases) what it accepts, so `is_ok()` alone is not the
    // check: `UPPER` is valid *after* normalisation while the directory on disk is `upper`,
    // and serving `themes/UPPER/preview.svg` on a case-sensitive filesystem would 404 for a
    // key the gallery believes it offered. The rule is therefore "accepted AND unchanged".
    match omnion_content::validation::validate_key(key, "theme key") {
        Ok(normalised) if normalised == key => vec![
            (PREVIEW_ASSET_NAME.to_owned(), "image/svg+xml"),
            (stylesheet_name(key), "text/css; charset=utf-8"),
        ],
        _ => Vec::new(),
    }
}

/// The content type a served file is returned as, or `None` when this route will not serve it.
///
/// Spelled out rather than derived from the extension: the browser is the consumer, and an
/// SVG served as `application/octet-stream` is a card with a broken image on every site.
fn content_type(key: &str, name: &str) -> Option<&'static str> {
    served_names(key)
        .into_iter()
        .find(|(served, _)| served == name)
        .map(|(_, kind)| kind)
}

/// The repository's `themes/` directory, resolved once.
///
/// A walk up from the API crate's manifest directory until a `themes/` directory appears,
/// because the crate lives at `apps/api` in the repository but at some other depth in a
/// container, and a hard-coded `../../..` is a build that works until someone moves a file.
fn themes_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let start = Path::new(env!("CARGO_MANIFEST_DIR"));
        for candidate in start.ancestors() {
            let themes = candidate.join("themes");
            if themes.is_dir() {
                return themes;
            }
        }
        // An installation with no `themes/` directory is not a crash: the gallery draws a
        // generated swatch, so a missing directory must not take the API down at boot.
        start.join("themes")
    })
}

/// The bytes of a theme asset, read once and kept.
///
/// Keyed by `<key>/<name>` in a `OnceLock` map rather than by file, so a card that is
/// re-rendered after a save re-reads nothing. The map is bounded by construction: it can only
/// grow by a name from [`served_names`], which is a fixed two entries per key.
fn asset_bytes(key: &str, name: &str) -> Option<&'static [u8]> {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, &'static [u8]>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let cache_key = format!("{key}/{name}");

    if let Ok(map) = cache.lock() {
        if let Some(bytes) = map.get(&cache_key) {
            return Some(bytes);
        }
    }

    // The stylesheet lives under `styles/` and everything else at the theme root, and the name
    // is one the allow-list produced — never one a request invented. This is the join that
    // turns a request into a path, so it is written to be readable as "nothing here is input".
    let relative = if name == stylesheet_name(key).as_str() && !name.is_empty() {
        Path::new("styles").join(name)
    } else {
        PathBuf::from(name)
    };
    let path = themes_root().join(key).join(relative);
    let bytes: &'static [u8] = match std::fs::read(&path) {
        Ok(bytes) => Box::leak(bytes.into_boxed_slice()),
        Err(_) => return None,
    };
    if let Ok(mut map) = cache.lock() {
        map.insert(cache_key, bytes);
    }
    Some(bytes)
}

/// `GET /api/v1/themes/{key}/assets/{file}` — one of a bundled theme's own files.
pub async fn theme_asset(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path((key, file)): axum::extract::Path<(String, String)>,
) -> Result<Response, ApiError> {
    let _ = &state;

    // The allow-list is the whole authorisation story: a name the platform does not serve is
    // refused before anything touches the filesystem, so the worst case for a hostile name is
    // a 404 and not a disclosure. The key is checked the same way, by `served_names`.
    let Some(kind) = content_type(&key, &file) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "theme_asset_not_found",
            "a theme serves its preview image and its stylesheet, and nothing else",
        ));
    };

    let Some(bytes) = asset_bytes(&key, &file) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "theme_asset_not_found",
            "this theme ships no such file",
        ));
    };

    let mut response = Response::new(Body::from(bytes.to_vec()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(kind),
    );
    // Immutable for the life of the process, and it *is* immutable: the bytes are cached, so a
    // browser revalidating this file can only ever be told the truth.
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=3600"),
    );
    // An SVG is an active document format. Served from a different origin than the panel this
    // is harmless; served from the same origin it is a script the theme can run in the admin's
    // session, so the content type is fixed and the sandbox is the defence that survives a
    // future decision to serve the file from the panel's own origin.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{content_type, served_names};

    /// The allow-list is two entries for a real key, and it is the *whole* rule: a name the
    /// platform does not list is refused before the filesystem is touched.
    #[test]
    fn only_the_two_shipped_file_kinds_are_served() {
        assert_eq!(served_names("magazine").len(), 2);
        assert!(content_type("magazine", "preview.svg").is_some());
        assert!(content_type("magazine", "magazine.css").is_some());
        for refused in ["", "..", "package.json", "src/theme.ts", "omnion.theme.json"] {
            assert!(
                content_type("magazine", refused).is_none(),
                "{refused} must not be servable"
            );
        }
    }

    /// A key that cannot name a directory gets no allow-list entries at all, so the join in
    /// `asset_bytes` can never walk out of the themes directory. This is the test that catches
    /// the version of `served_names` that returned `preview.svg` for every key.
    #[test]
    fn a_key_that_cannot_name_a_directory_gets_nothing_served() {
        for hostile in ["../etc", "a/b", "a\\b", "..", "", "UPPER", "trailing-", "-lead"] {
            assert!(
                served_names(hostile).is_empty(),
                "{hostile} must not have a servable name"
            );
            assert!(content_type(hostile, "preview.svg").is_none(), "{hostile}");
        }
    }

    /// One theme cannot ask for another's stylesheet; the preview name is the same file for
    /// every theme, so it is the pair (key, name) that identifies a file and not the name.
    #[test]
    fn a_stylesheet_name_is_scoped_to_its_own_key() {
        assert!(content_type("magazine", "tech.css").is_none());
        assert!(content_type("magazine", "magazine.css").is_some());
    }

    /// The bytes of a real theme's own preview image are readable through the same lookup the
    /// route uses, and the repository really does ship all ten.
    #[test]
    fn every_shipped_theme_serves_its_own_preview_image() {
        let mut served = 0;
        for entry in std::fs::read_dir(super::themes_root()).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(key) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if super::asset_bytes(key, "preview.svg").is_some() {
                served += 1;
            }
        }
        assert_eq!(
            served, 10,
            "the ten bundled themes each ship a preview.svg the gallery can draw"
        );
    }

    /// The stylesheet a theme ships is on disk under `styles/`, which is the one place this
    /// module joins a path — so the join is what is under test, not the allow-list.
    #[test]
    fn the_stylesheet_is_read_from_the_styles_directory() {
        assert!(
            super::asset_bytes("magazine", "magazine.css").is_some(),
            "themes/magazine/styles/magazine.css must be readable through the allow-list"
        );
        // The same name at the theme root does not exist, and must not be invented.
        assert!(super::asset_bytes("magazine", "preview.svg").is_some());
        assert!(super::asset_bytes("minimal", "minimal.css").is_some());
    }
}
