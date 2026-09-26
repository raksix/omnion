//! What the media library accepts: file names, content types and object keys.
//!
//! Uploads arrive from outside, so every part of a row that reaches the database or a response
//! header is derived here: the file name is reduced to a safe, bounded form, the content type is
//! normalised, and the object key is built from ids — never from client input.

use uuid::Uuid;

use crate::error::MediaError;
use crate::model::MAX_FILENAME_LENGTH;

/// Content types a browser may render inline from the platform's own origin.
///
/// Everything else is served as a download (`application/octet-stream`) with a `nosniff` header:
/// a markup document uploaded as "the site logo" must never become a script on the panel's own
/// origin. SVG stays out of this list on purpose — it can carry script.
pub const INLINE_CONTENT_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "video/mp4",
    "video/webm",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "application/pdf",
    "text/plain",
];

/// How one stored content type is served to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServePlan {
    /// Content type the response carries.
    pub content_type: &'static str,
    /// `inline` when the browser may render the file, `attachment` otherwise.
    pub disposition: &'static str,
}

/// Decide how a stored content type is served.
#[must_use]
pub fn serve_plan(stored: &str) -> ServePlan {
    match INLINE_CONTENT_TYPES
        .iter()
        .find(|candidate| **candidate == stored)
    {
        Some(allowed) => ServePlan {
            content_type: allowed,
            disposition: "inline",
        },
        None => ServePlan {
            content_type: "application/octet-stream",
            disposition: "attachment",
        },
    }
}

/// Reduce a client-supplied file name to the part the library keeps.
///
/// Directories are dropped, control characters and symbols become dashes, a leading dot is
/// removed (no hidden files), and the result is capped at [`MAX_FILENAME_LENGTH`] characters
/// while keeping its extension.
pub fn sanitize_filename(raw: &str) -> Result<String, MediaError> {
    let last_segment = raw.rsplit(['/', '\\']).next().unwrap_or_default().trim();

    let mut reduced = String::with_capacity(last_segment.len());
    let mut last_was_dash = false;
    for character in last_segment.chars() {
        let keep = character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_');
        if keep {
            reduced.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            reduced.push('-');
            last_was_dash = true;
        }
    }

    let reduced = reduced.trim_matches(['-', '.'].as_slice()).to_owned();
    if reduced.is_empty() {
        return Err(MediaError::InvalidFilename(
            "the file name carries no usable characters".to_owned(),
        ));
    }

    if reduced.chars().count() <= MAX_FILENAME_LENGTH {
        return Ok(reduced);
    }

    let (stem, extension) = split_extension(&reduced);
    let keep = MAX_FILENAME_LENGTH.saturating_sub(extension.len() + 1);
    let stem: String = stem.chars().take(keep).collect();
    Ok(format!("{stem}.{extension}"))
}

/// Normalise a content type: lower-case `type/subtype`, both halves token-shaped.
pub fn normalize_content_type(raw: &str) -> Result<String, MediaError> {
    let trimmed = raw.trim();
    let Some((kind, subkind)) = trimmed.split_once('/') else {
        return Err(MediaError::InvalidContentType(format!(
            "{trimmed:?} is not a content type"
        )));
    };

    let token_ok = |value: &str| {
        !value.is_empty()
            && value.len() <= 100
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(
                        character,
                        '!' | '#'
                            | '$'
                            | '&'
                            | '\''
                            | '*'
                            | '+'
                            | '-'
                            | '.'
                            | '^'
                            | '_'
                            | '`'
                            | '|'
                            | '~'
                    )
            })
    };

    if !token_ok(kind) || !token_ok(subkind) {
        return Err(MediaError::InvalidContentType(format!(
            "{trimmed:?} is not a content type"
        )));
    }

    Ok(format!("{}/{subkind}", kind.to_lowercase()).to_lowercase())
}

/// Object key of one media object: `sites/{site}/{media}{extension}`.
///
/// Built from ids and the sanitised extension, so the key never carries client-controlled path
/// material (see [`omnion_storage::validate_key`], which the storage layer applies again).
#[must_use]
pub fn object_key(site_id: Uuid, media_id: Uuid, filename: &str) -> String {
    match extension_of(filename) {
        Some(extension) => format!("sites/{site_id}/{media_id}.{extension}"),
        None => format!("sites/{site_id}/{media_id}"),
    }
}

/// The extension of an already-sanitised file name, when it carries a usable one.
fn extension_of(filename: &str) -> Option<String> {
    let (_, extension) = split_extension(filename);
    if extension.is_empty() || extension.len() > 8 {
        return None;
    }
    if !extension
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension.to_lowercase())
}

/// Split a file name into stem and extension; `("archive", "tar")` for `archive.tar`.
fn split_extension(filename: &str) -> (&str, &str) {
    match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => (stem, extension),
        _ => (filename, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_reduced_to_their_safe_form() {
        assert_eq!(
            sanitize_filename("../../etc/passwd").expect("a path is reduced to its last segment"),
            "passwd"
        );
        assert_eq!(
            sanitize_filename("Holiday Photo (1).PNG").expect("symbols become dashes"),
            "Holiday-Photo-1-.PNG"
        );
        assert_eq!(
            sanitize_filename("C:\\Users\\ada\\logo.svg").expect("windows paths are reduced too"),
            "logo.svg"
        );
        assert_eq!(
            sanitize_filename("  ..hidden  ").expect("leading dots go"),
            "hidden"
        );
    }

    #[test]
    fn a_name_without_usable_characters_is_refused() {
        assert!(matches!(
            sanitize_filename("   "),
            Err(MediaError::InvalidFilename(_))
        ));
        assert!(matches!(
            sanitize_filename("///"),
            Err(MediaError::InvalidFilename(_))
        ));
    }

    #[test]
    fn long_names_keep_their_extension() {
        let name = format!("{}.png", "a".repeat(400));
        let reduced = sanitize_filename(&name).expect("a long name is capped");
        assert!(reduced.chars().count() <= MAX_FILENAME_LENGTH);
        assert!(reduced.ends_with(".png"), "reduced: {reduced}");
    }

    #[test]
    fn content_types_are_normalised() {
        assert_eq!(
            normalize_content_type(" Image/PNG ").expect("case is normalised"),
            "image/png"
        );
        assert_eq!(
            normalize_content_type("application/vnd.ms-excel").expect("vendor types pass"),
            "application/vnd.ms-excel"
        );
    }

    #[test]
    fn content_types_must_be_shaped_like_one() {
        for raw in [
            "",
            "png",
            "image/",
            "/png",
            "image/png; charset=utf-8",
            "image/png\r\nx",
        ] {
            assert!(
                normalize_content_type(raw).is_err(),
                "{raw:?} must be refused"
            );
        }
    }

    #[test]
    fn object_keys_are_built_from_ids() {
        let site = Uuid::nil();
        let media = Uuid::nil();
        assert_eq!(
            object_key(site, media, "Logo.PNG"),
            "sites/00000000-0000-0000-0000-000000000000/00000000-0000-0000-0000-000000000000.png"
        );
        // An extension that is not alphanumeric (or too long) is dropped, never smuggled in.
        assert_eq!(
            object_key(site, media, "weird.name-extension-here"),
            "sites/00000000-0000-0000-0000-000000000000/00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn only_renderable_types_are_served_inline() {
        assert_eq!(
            serve_plan("image/png"),
            ServePlan {
                content_type: "image/png",
                disposition: "inline"
            }
        );
        for stored in ["text/html", "image/svg+xml", "application/x-httpd-php"] {
            let plan = serve_plan(stored);
            assert_eq!(plan.content_type, "application/octet-stream");
            assert_eq!(plan.disposition, "attachment");
        }
    }
}
