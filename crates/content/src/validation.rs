//! Validation shared by the content store: slugs, titles, page type keys, lifecycle states
//! and the language tags translation rows carry.
//!
//! Every rule mirrors a constraint of `database/migrations/0004_content.sql`, so a value the
//! store accepts is a value the database accepts — and a value it refuses comes back as a
//! typed error instead of a constraint violation.

use crate::error::{ContentError, Result};

/// Longest accepted slug (matches the schema check).
pub const MAX_SLUG_LENGTH: usize = 96;

/// Longest accepted title.
pub const MAX_TITLE_LENGTH: usize = 200;

/// Longest accepted summary.
pub const MAX_SUMMARY_LENGTH: usize = 500;

/// Largest accepted body (1 MiB).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Largest accepted translation value (64 KiB).
pub const MAX_TRANSLATION_BYTES: usize = 64 * 1024;

/// Lifecycle states a page and its revisions carry.
pub const PAGE_STATUSES: [&str; 3] = ["draft", "published", "archived"];

/// The fields the content surface translates, and the only ones the API accepts.
pub const TRANSLATION_FIELDS: [&str; 3] = ["title", "body", "summary"];

/// Validate a slug and normalize it to lowercase.
///
/// The slug is the page's address inside its site: lowercase letters, digits and dashes,
/// starting and ending alphanumeric (`home`, `about-us`).
pub fn validate_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_lowercase();
    let shaped = !slug.is_empty()
        && slug.len() <= MAX_SLUG_LENGTH
        && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(slug);
    }
    Err(ContentError::InvalidSlug(format!(
        "{slug:?} must be lowercase letters, digits and dashes (1-{MAX_SLUG_LENGTH} characters)"
    )))
}

/// Validate a title: non-empty after trimming and reasonably short.
pub fn validate_title(title: &str) -> Result<String> {
    let title = title.trim().to_owned();
    if title.is_empty() || title.chars().count() > MAX_TITLE_LENGTH {
        return Err(ContentError::InvalidTitle(format!(
            "the title must be 1 to {MAX_TITLE_LENGTH} characters"
        )));
    }
    Ok(title)
}

/// Validate a body: any text, bounded so one row cannot grow without limit.
pub fn validate_body(body: &str) -> Result<String> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ContentError::InvalidBody(format!(
            "the body must stay under {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(body.to_owned())
}

/// Validate a summary; an empty (or whitespace-only) summary clears the field.
pub fn validate_summary(summary: &str) -> Result<Option<String>> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Ok(None);
    }
    if summary.chars().count() > MAX_SUMMARY_LENGTH {
        return Err(ContentError::InvalidSummary(format!(
            "the summary must stay under {MAX_SUMMARY_LENGTH} characters"
        )));
    }
    Ok(Some(summary.to_owned()))
}

/// Validate a page type key (`page`, `blog_post`, …).
pub fn validate_page_type(page_type: &str) -> Result<String> {
    let page_type = page_type.trim().to_lowercase();
    let shaped = page_type.len() <= 63
        && page_type.starts_with(|c: char| c.is_ascii_lowercase())
        && page_type
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if shaped {
        return Ok(page_type);
    }
    Err(ContentError::InvalidPageType(format!(
        "{page_type:?} must be a lowercase key like \"page\" or \"blog_post\""
    )))
}

/// Validate a lifecycle state against the documented values.
pub fn validate_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if PAGE_STATUSES.contains(&status.as_str()) {
        return Ok(status);
    }
    Err(ContentError::InvalidStatus(format!(
        "status {status:?} must be one of {}",
        PAGE_STATUSES.join(", ")
    )))
}

/// Validate a language tag and normalize it to lowercase (`pt-BR` → `pt-br`).
pub fn validate_language(language: &str) -> Result<String> {
    let language = language.trim().to_lowercase();
    let mut parts = language.split('-');
    let primary_ok = parts
        .next()
        .is_some_and(|part| (2..=8).contains(&part.len()) && is_lower_alnum(part));
    let subtags_ok = parts.all(|part| {
        (2..=8).contains(&part.len()) && is_lower_alnum(part) && !part.starts_with('-')
    });
    if !language.is_empty() && primary_ok && subtags_ok {
        return Ok(language);
    }
    Err(ContentError::InvalidLanguage(format!(
        "{language:?} must be a lowercase language tag like \"tr\", \"en\" or \"pt-br\""
    )))
}

/// Validate a translation or resource field name (`title`, `body`, `blog_post`).
pub fn validate_field(field: &str) -> Result<String> {
    let field = field.trim().to_lowercase();
    let shaped = field.len() <= 63
        && field.starts_with(|c: char| c.is_ascii_lowercase())
        && field
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if shaped {
        return Ok(field);
    }
    Err(ContentError::InvalidField(format!(
        "{field:?} must be a lowercase field name like \"title\""
    )))
}

/// Validate a translated value (any text, bounded).
pub fn validate_translation_value(value: &str) -> Result<String> {
    if value.len() > MAX_TRANSLATION_BYTES {
        return Err(ContentError::InvalidValue(format!(
            "the value must stay under {MAX_TRANSLATION_BYTES} bytes"
        )));
    }
    Ok(value.to_owned())
}

fn is_lower_alnum(part: &str) -> bool {
    part.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_normalize_and_reject_bad_shapes() {
        assert_eq!(validate_slug(" About-Us ").expect("valid"), "about-us");
        assert_eq!(validate_slug("home").expect("valid"), "home");
        for bad in [
            "",
            "-home",
            "home-",
            "about us",
            "about_us",
            "About!",
            "blog/post",
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_slug(&"a".repeat(MAX_SLUG_LENGTH + 1)).is_err());
    }

    #[test]
    fn titles_are_trimmed_and_bounded() {
        assert_eq!(validate_title("  Welcome  ").expect("valid"), "Welcome");
        assert!(validate_title("   ").is_err());
        assert!(validate_title(&"t".repeat(MAX_TITLE_LENGTH + 1)).is_err());
    }

    #[test]
    fn summaries_clear_on_empty_and_bodies_are_bounded() {
        assert_eq!(validate_summary("  ").expect("empty"), None);
        assert_eq!(
            validate_summary(" Short intro ").expect("valid"),
            Some("Short intro".to_owned())
        );
        assert!(validate_summary(&"s".repeat(MAX_SUMMARY_LENGTH + 1)).is_err());

        assert_eq!(validate_body("hello").expect("valid"), "hello");
        assert!(validate_body(&"b".repeat(MAX_BODY_BYTES + 1)).is_err());
    }

    #[test]
    fn page_types_and_statuses_follow_the_schema() {
        assert_eq!(
            validate_page_type(" Blog_Post ").expect("valid"),
            "blog_post"
        );
        assert_eq!(validate_page_type("page").expect("valid"), "page");
        for bad in ["", "1page", "blog post", "blog-post"] {
            assert!(validate_page_type(bad).is_err(), "{bad:?} must be rejected");
        }

        assert_eq!(validate_status(" Published ").expect("valid"), "published");
        assert!(validate_status("scheduled").is_err());
    }

    #[test]
    fn languages_normalize_to_lowercase_tags() {
        assert_eq!(validate_language(" TR ").expect("valid"), "tr");
        assert_eq!(validate_language("pt-BR").expect("valid"), "pt-br");
        assert_eq!(validate_language("zh-hant").expect("valid"), "zh-hant");
        for bad in ["", "t", "türkçe", "tr_", "-tr", "tr-", "english!"] {
            assert!(validate_language(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn the_translation_surface_fields_are_valid_names() {
        for field in TRANSLATION_FIELDS {
            assert_eq!(
                validate_field(field).expect("catalogued fields are valid"),
                field
            );
        }
        assert!(validate_field("Blog Post").is_err());
        assert!(validate_translation_value("Merhaba").is_ok());
        assert!(validate_translation_value(&"v".repeat(MAX_TRANSLATION_BYTES + 1)).is_err());
    }
}
