//! Object keys.
//!
//! Every driver stores objects under a key, and both drivers have to agree on what a key may
//! look like: the file-system driver joins the key onto its root directory, so a key is only
//! safe to accept when it cannot escape that root. Keys are validated here, in one place, and
//! the media library builds them from ids that are already opaque.

use crate::error::{Result, StorageError};

/// Longest object key the store accepts.
pub const MAX_KEY_LENGTH: usize = 512;

/// Check that a key is usable by every driver.
///
/// A valid key is a `/`-separated list of non-empty segments over `[A-Za-z0-9._-]`, without
/// `.`/`..` segments, without a leading or trailing separator and no longer than
/// [`MAX_KEY_LENGTH`] characters — the shape that cannot leave a storage root behind.
pub fn validate_key(key: &str) -> Result<()> {
    let invalid = |reason: &str| {
        StorageError::Invalid(format!("{key:?} is not a valid object key: {reason}"))
    };

    if key.is_empty() {
        return Err(invalid("it is empty"));
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(invalid("it is too long"));
    }
    if key.starts_with('/') || key.ends_with('/') {
        return Err(invalid("it must not start or end with a separator"));
    }
    for segment in key.split('/') {
        if segment.is_empty() {
            return Err(invalid("it carries an empty segment"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid("it carries a relative segment"));
        }
        if !segment.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        }) {
            return Err(invalid(
                "segments may only carry letters, digits, dashes, underscores and dots",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_shaped_keys_are_accepted() {
        assert!(validate_key("sites/6f2c1f38-1e0a-4a3a-9d0a-2f6f9a5b1c2d/9b1c.png").is_ok());
        assert!(validate_key("omnion-media/probe.txt").is_ok());
    }

    #[test]
    fn escaping_keys_are_rejected() {
        for key in [
            "",
            "/absolute",
            "trailing/",
            "double//separator",
            "../escape",
            "sites/../escape",
            "sites/./current",
            "with space",
            "with%20escape",
        ] {
            assert!(validate_key(key).is_err(), "{key:?} must be rejected");
        }
    }

    #[test]
    fn overlong_keys_are_rejected() {
        let key = format!("sites/{}", "a".repeat(MAX_KEY_LENGTH));
        assert!(validate_key(&key).is_err());
    }
}
