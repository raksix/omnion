//! Environment keys and staging hosts.
//!
//! The key is a URL-ish identifier: it appears in the staging host name, in the API path and in
//! every event payload, so it has to survive being typed, pasted and slugified from a name. The
//! format is deliberately the same one the rest of the platform uses for slugs, and the reserved
//! words are the ones that would collide with a real route.

use crate::error::EnvironmentError;

/// The words an environment key may not take.
///
/// `production` is reserved because the single production environment is created with the
/// organization under exactly that key, and a second environment with the same key would be
/// indistinguishable from it in a URL. The rest are platform routes: an environment whose key is
/// `api` would answer on `/environments/api` in a way that reads like the API itself.
pub const RESERVED_KEYS: &[&str] = &[
    "production",
    "prod",
    "staging",
    "api",
    "admin",
    "assets",
    "static",
    "media",
    "cdn",
    "www",
    "app",
    "auth",
    "login",
    "logout",
    "settings",
    "users",
    "organizations",
    "health",
    "status",
    "public",
    "graphql",
    "docs",
    "help",
    "support",
    "about",
    "new",
    "edit",
];

/// The longest key, matching the `key` column's own check.
pub const MAX_KEY_LEN: usize = 55;

/// The result of validating a key: what is wrong, or `Ok` with the normalised key.
pub type KeyCheck = Result<String, EnvironmentError>;

/// Is this key free to be used, and what is its normalised form?
///
/// The normalisation is lowercasing and nothing else. Trimming a key somebody can see in a host
/// name is a courtesy, but lowercasing *changes* the string, and a key the API stored as `qa` and
/// the panel displayed as `QA` is a support ticket — so an uppercase key is refused outright
/// rather than quietly folded.
pub fn check_key(raw: &str) -> KeyCheck {
    let key = raw.trim();
    if key.is_empty() {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "it is empty".to_string(),
        });
    }
    if key.len() > MAX_KEY_LEN {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: format!("it is longer than {MAX_KEY_LEN} characters"),
        });
    }
    if key != key.to_ascii_lowercase() {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "it must be lowercase".to_string(),
        });
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || key.starts_with('-')
        || key.ends_with('-')
        || key.contains("--")
    {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason:
                "it may use lowercase letters, digits and single dashes only, and cannot start \
                     or end with a dash"
                    .to_string(),
        });
    }
    if RESERVED_KEYS.contains(&key) {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "it is a reserved word".to_string(),
        });
    }
    Ok(key.to_string())
}

/// Derive a key from a name, for the wizard's auto-filled field.
///
/// The derivation is a proposal, never a decision: the field is editable and the API validates
/// what arrives. Everything it produces is a legal key shape by construction, but a reserved word
/// or a collision is still possible and is caught at save time — a wizard that silently invented a
/// different key than the one the operator sees is the failure this avoids by returning the value
/// rather than hiding it.
pub fn derive_key(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // a leading dash would be refused by check_key
    for ch in name.chars() {
        for c in ch.to_lowercase() {
            if c.is_ascii_alphanumeric() {
                out.push(c);
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > MAX_KEY_LEN {
        out.truncate(MAX_KEY_LEN);
        while out.ends_with('-') {
            out.pop();
        }
    }
    // A reserved word is caught at save time, but a *derived* key that is reserved is a wizard
    // dead end rather than a message: the operator types "Staging", the field fills itself with
    // "staging", and submitting is refused for a word they never chose. So the derivation
    // suffixes its way out of the reservation instead, and only after the reservation is
    // actually hit — "staging" becomes "staging-2", while "qa" stays "qa".
    if out.is_empty() {
        out.push_str("staging");
    }
    if RESERVED_KEYS.contains(&out.as_str()) {
        out.push_str("-2");
    }
    out
}

/// A staging host name, checked.
///
/// The host is what a search engine and a certificate are pointed at, so the rules are
/// host-shaped rather than slug-shaped: at least one dot, no scheme, no path, no port, and no
/// wildcard. An empty host is allowed — a staging environment without a host is a legitimate
/// thing to create before DNS is decided, and refusing it would push operators to invent one.
pub fn check_staging_host(raw: &str) -> KeyCheck {
    let host = raw.trim().trim_end_matches('.');
    if host.is_empty() {
        return Ok(String::new());
    }
    if host.len() > 253 {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "a host name is at most 253 characters".to_string(),
        });
    }
    if host.contains("://") {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "write the host on its own, without “https://”".to_string(),
        });
    }
    if host.contains('/') {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "a host name has no path".to_string(),
        });
    }
    if host.contains(':') {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "a host name has no port".to_string(),
        });
    }
    if host.contains('*') {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "wildcards are not accepted here".to_string(),
        });
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(EnvironmentError::InvalidKey {
            key: raw.to_string(),
            reason: "it needs at least one dot, as in “staging.example.com”".to_string(),
        });
    }
    for label in labels {
        if label.is_empty() || label.len() > 63 {
            return Err(EnvironmentError::InvalidKey {
                key: raw.to_string(),
                reason: "one of its parts is empty or too long".to_string(),
            });
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(EnvironmentError::InvalidKey {
                key: raw.to_string(),
                reason: "no part may start or end with a dash".to_string(),
            });
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(EnvironmentError::InvalidKey {
                key: raw.to_string(),
                reason: "it may use letters, digits, dashes and underscores only".to_string(),
            });
        }
    }
    Ok(host.to_ascii_lowercase())
}

/// The host a staging environment is served on, given the organization's own domains.
///
/// Used as the wizard's *proposal* when the operator types none. A production domain of
/// `example.com` and a key of `qa` give `qa.example.com`; a multi-level production domain
/// (`www.example.com`) drops the leading `www.` first, because `qa.www.example.com` is a host
/// nobody asked for.
pub fn suggest_staging_host(production_host: &str, key: &str) -> String {
    let base = production_host.trim().trim_end_matches('.');
    let base = base.strip_prefix("www.").unwrap_or(base);
    if base.is_empty() {
        return String::new();
    }
    format!("{key}.{base}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_key_passes() {
        assert_eq!(check_key("qa").unwrap(), "qa");
        assert_eq!(check_key("staging-2").unwrap(), "staging-2");
    }

    #[test]
    fn an_empty_key_is_refused_by_name() {
        let err = check_key("   ").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn an_uppercase_key_is_refused_rather_than_folded() {
        // Folding would store `qa` and display `QA`, and the two would diverge in a host name.
        let err = check_key("QA").unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{err}");
    }

    #[test]
    fn a_key_may_not_start_or_end_with_a_dash_or_double_one() {
        for bad in ["-qa", "qa-", "qa--2"] {
            assert!(check_key(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn reserved_words_are_refused() {
        for reserved in ["production", "api", "admin", "settings", "www"] {
            let err = check_key(reserved).unwrap_err();
            assert!(err.to_string().contains("reserved"), "{reserved}: {err}");
        }
    }

    #[test]
    fn an_over_long_key_is_refused_at_the_column_limit() {
        let long = "a".repeat(MAX_KEY_LEN + 1);
        assert!(check_key(&long).unwrap_err().to_string().contains("55"));
    }

    #[test]
    fn a_derived_key_is_always_a_legal_key_shape() {
        // Whatever the operator typed as a name, the proposal must not need fixing to be saved.
        for name in [
            "QA Environment",
            "  Ünïcode Näme  ",
            "----",
            "Ünïcode Only",
            "release/2026-09",
        ] {
            let key = derive_key(name);
            assert!(!key.is_empty(), "{name:?} produced an empty key");
            // Legality apart from the reserved-word rule, which is a collision, not a shape.
            if !RESERVED_KEYS.contains(&key.as_str()) {
                assert!(check_key(&key).is_ok(), "{name:?} → {key:?} is not legal");
            }
        }
    }

    #[test]
    fn a_derived_key_never_exceeds_the_limit_or_ends_in_a_dash() {
        let key = derive_key(&"a very long environment name ".repeat(6));
        assert!(key.len() <= MAX_KEY_LEN, "{}", key.len());
        assert!(!key.ends_with('-'), "{key}");
    }

    #[test]
    fn an_empty_host_is_allowed_so_dns_can_be_decided_later() {
        assert_eq!(check_staging_host("  ").unwrap(), "");
    }

    #[test]
    fn a_host_needs_a_dot_and_no_scheme_path_or_port() {
        for (bad, why) in [
            ("localhost", "dot"),
            ("https://qa.example.com", "scheme"),
            ("qa.example.com/path", "path"),
            ("qa.example.com:8443", "port"),
            ("*.example.com", "wildcard"),
            ("qa..example.com", "empty part"),
        ] {
            let err = check_staging_host(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("dot")
                    || msg.contains("https")
                    || msg.contains("path")
                    || msg.contains("port")
                    || msg.contains("wildcard")
                    || msg.contains("empty or too long"),
                "{bad} ({why}) → {msg}"
            );
        }
    }

    #[test]
    fn a_trailing_dot_is_accepted_and_dropped() {
        assert_eq!(
            check_staging_host("QA.Example.com.").unwrap(),
            "qa.example.com"
        );
    }

    #[test]
    fn the_suggested_host_drops_a_leading_www() {
        assert_eq!(
            suggest_staging_host("www.example.com", "qa"),
            "qa.example.com"
        );
        assert_eq!(suggest_staging_host("example.com", "qa"), "qa.example.com");
        assert_eq!(suggest_staging_host("", "qa"), "");
    }
}
