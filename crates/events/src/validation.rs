//! Validation of event names and endpoint definitions.
//!
//! Everything here is a pure function so the API surface, the bus and the tests all agree on
//! what the platform stores: an event name is dotted and lower-case (`page.published`), an
//! endpoint URL is http(s) without whitespace, and a signing secret is long enough to be worth
//! signing with.

use rand::RngCore;

use crate::error::{EventsError, Result};

/// Longest event name the platform records.
pub const MAX_EVENT_NAME: usize = 96;

/// Longest single segment of an event name.
pub const MAX_EVENT_SEGMENT: usize = 32;

/// How many event names one endpoint may subscribe to.
pub const MAX_SUBSCRIPTIONS: usize = 32;

/// Shortest accepted signing secret.
pub const MIN_SECRET: usize = 16;

/// Longest accepted signing secret.
pub const MAX_SECRET: usize = 128;

/// Longest accepted endpoint name.
pub const MAX_ENDPOINT_NAME: usize = 64;

/// Longest accepted endpoint URL.
pub const MAX_URL: usize = 2048;

/// Validate an event name: at least two dotted segments, each `[a-z][a-z0-9_]*`.
pub fn validate_event_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(EventsError::invalid_event("the event name is empty"));
    }
    if name.len() > MAX_EVENT_NAME {
        return Err(EventsError::invalid_event(format!(
            "the event name is longer than {MAX_EVENT_NAME} characters"
        )));
    }

    let segments: Vec<&str> = name.split('.').collect();
    if segments.len() < 2 {
        return Err(EventsError::invalid_event(
            "an event name needs a domain and an action, e.g. page.published",
        ));
    }

    for segment in &segments {
        let mut characters = segment.chars();
        let starts_right = characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase());
        let rest_right = characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        });
        if !starts_right || !rest_right || segment.len() > MAX_EVENT_SEGMENT || segment.is_empty() {
            return Err(EventsError::invalid_event(format!(
                "{segment:?} is not a lower-case event segment"
            )));
        }
    }

    Ok(name.to_owned())
}

/// Validate a subscription list: every name valid, deduplicated, sorted, bounded.
pub fn validate_subscriptions(raw: &[String]) -> Result<Vec<String>> {
    if raw.is_empty() {
        return Err(EventsError::invalid_endpoint(
            "subscribe the endpoint to at least one event",
        ));
    }
    if raw.len() > MAX_SUBSCRIPTIONS {
        return Err(EventsError::invalid_endpoint(format!(
            "an endpoint subscribes to at most {MAX_SUBSCRIPTIONS} events"
        )));
    }

    let mut names = Vec::with_capacity(raw.len());
    for entry in raw {
        let name = validate_event_name(entry)?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();

    Ok(names)
}

/// Validate an endpoint name.
pub fn validate_endpoint_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(EventsError::invalid_endpoint("the endpoint name is empty"));
    }
    if name.chars().count() > MAX_ENDPOINT_NAME {
        return Err(EventsError::invalid_endpoint(format!(
            "the endpoint name is longer than {MAX_ENDPOINT_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(EventsError::invalid_endpoint(
            "the endpoint name carries control characters",
        ));
    }

    Ok(name.to_owned())
}

/// Validate an endpoint URL: http(s), a host, no whitespace.
pub fn validate_url(raw: &str) -> Result<String> {
    let url = raw.trim();
    if url.len() > MAX_URL {
        return Err(EventsError::invalid_endpoint(format!(
            "the URL is longer than {MAX_URL} characters"
        )));
    }
    if url.chars().any(char::is_whitespace) {
        return Err(EventsError::invalid_endpoint("the URL carries whitespace"));
    }

    match reqwest::Url::parse(url) {
        Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
            if parsed.host_str().is_none_or(str::is_empty) {
                return Err(EventsError::invalid_endpoint("the URL has no host"));
            }
            Ok(parsed.to_string())
        }
        _ => Err(EventsError::invalid_endpoint(
            "the URL must be http(s) and absolute, e.g. https://example.com/hooks/omnion",
        )),
    }
}

/// Validate a signing secret the operator supplied themselves.
pub fn validate_secret(raw: &str) -> Result<String> {
    let secret = raw.trim();
    if secret.len() < MIN_SECRET {
        return Err(EventsError::invalid_endpoint(format!(
            "the signing secret needs at least {MIN_SECRET} characters"
        )));
    }
    if secret.len() > MAX_SECRET {
        return Err(EventsError::invalid_endpoint(format!(
            "the signing secret is longer than {MAX_SECRET} characters"
        )));
    }

    Ok(secret.to_owned())
}

/// Generate a signing secret: 32 random bytes, hex-encoded (64 characters).
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_dotted_and_lower_case() {
        assert_eq!(
            validate_event_name("page.published").expect("valid"),
            "page.published"
        );
        assert_eq!(
            validate_event_name(" order.created_v2 ").expect("valid"),
            "order.created_v2"
        );

        for invalid in [
            "page",
            "Page.published",
            "page..published",
            "page.published!",
            "page .published",
            "1page.published",
            "",
        ] {
            assert!(
                validate_event_name(invalid).is_err(),
                "{invalid:?} must be refused"
            );
        }
    }

    #[test]
    fn subscriptions_are_deduplicated_and_sorted() {
        let names = validate_subscriptions(&[
            "page.updated".to_owned(),
            "page.published".to_owned(),
            "page.updated".to_owned(),
        ])
        .expect("valid");

        assert_eq!(names, vec!["page.published", "page.updated"]);
        assert!(validate_subscriptions(&[]).is_err());
        assert!(validate_subscriptions(&["nope".to_owned()]).is_err());
    }

    #[test]
    fn urls_must_be_absolute_http_or_https() {
        assert_eq!(
            validate_url("https://example.test/hooks/omnion").expect("valid"),
            "https://example.test/hooks/omnion"
        );
        assert_eq!(
            validate_url("http://127.0.0.1:9000/hook").expect("valid"),
            "http://127.0.0.1:9000/hook"
        );

        for invalid in [
            "example.test/hook",
            "ftp://example.test/hook",
            "https://",
            "https://example.test/a b",
            "",
        ] {
            assert!(
                validate_url(invalid).is_err(),
                "{invalid:?} must be refused"
            );
        }
    }

    #[test]
    fn secrets_have_a_floor_and_a_ceiling() {
        assert!(validate_secret("short").is_err());
        assert!(validate_secret(&"x".repeat(MAX_SECRET + 1)).is_err());
        assert_eq!(
            validate_secret("0123456789abcdef").expect("valid"),
            "0123456789abcdef"
        );

        let generated = generate_secret();
        assert_eq!(generated.len(), 64);
        assert_ne!(generated, generate_secret(), "two secrets are not equal");
        assert!(validate_secret(&generated).is_ok());
    }

    #[test]
    fn endpoint_names_are_trimmed_and_bounded() {
        assert_eq!(
            validate_endpoint_name("  Receiver  ").expect("valid"),
            "Receiver"
        );
        assert!(validate_endpoint_name("   ").is_err());
        assert!(validate_endpoint_name(&"n".repeat(MAX_ENDPOINT_NAME + 1)).is_err());
    }
}
