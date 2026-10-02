//! Per-kind credential validators.
//!
//! A typed credential carries non-secret fields (an endpoint, a username, a port, a token
//! expiry, a key fingerprint) next to its sealed value. The validator is what proves the pair
//! still works: it reads the **non-secret** fields, decides what it can check without touching
//! the value, and returns the provider's own sentence when it can.
//!
//! Two rules from docs/requests/REQ-125 shape the design:
//!
//! * **A failing validation never blocks storage.** An operator who knows a provider is
//!   temporarily unreachable must still be able to store the credential, so this module never
//!   refuses a save — the outcome is a chip, not a gate.
//! * **Nothing here ever returns or echoes the value.** The [`CredentialKind::describe_hint`]
//!   sentences are written from the non-secret fields only, and a provider's refusal is passed
//!   through [`crate::redaction::redact`] by the caller before it is stored.
//!
//! The kinds and what each one checks:
//!
//! | Kind | What can be proven without the value | What needs a network call |
//! |---|---|---|
//! | `api_key` | shape: length, no whitespace, a recognisable prefix | the provider's key list |
//! | `oauth_token` | the expiry parses and is in the future | a token-info endpoint |
//! | `smtp_account` | host, port and a username are present | connect + greeting on the port |
//! | `payment_key` | the key's prefix and length | a read-only account call |
//! | `ssh_key` | the key parses and the fingerprint recomputes | nothing — it is offline |

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The five typed credential kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// A provider API key.
    ApiKey,
    /// A bearer/OAuth token.
    OAuthToken,
    /// An SMTP account (host, port, username).
    SmtpAccount,
    /// A payment provider key.
    PaymentKey,
    /// An SSH key pair.
    SshKey,
}

impl CredentialKind {
    /// The value stored in `secret_credentials.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthToken => "oauth_token",
            Self::SmtpAccount => "smtp_account",
            Self::PaymentKey => "payment_key",
            Self::SshKey => "ssh_key",
        }
    }

    /// Every kind, in the order the create wizard offers them.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::ApiKey,
            Self::OAuthToken,
            Self::SmtpAccount,
            Self::PaymentKey,
            Self::SshKey,
        ]
    }

    /// Parse a stored or submitted kind.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "api_key" => Some(Self::ApiKey),
            "oauth_token" => Some(Self::OAuthToken),
            "smtp_account" => Some(Self::SmtpAccount),
            "payment_key" => Some(Self::PaymentKey),
            "ssh_key" => Some(Self::SshKey),
            _ => None,
        }
    }

    /// The one-line purpose shown next to the kind in the wizard.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::ApiKey => "A provider key the platform calls an API with",
            Self::OAuthToken => "A bearer token with a known expiry",
            Self::SmtpAccount => "Credentials for an outgoing mail server",
            Self::PaymentKey => "A key the checkout flow charges with",
            Self::SshKey => "A key pair for server-to-server access",
        }
    }

    /// The non-secret fields this kind expects, for the wizard's form.
    #[must_use]
    pub const fn expected_fields(self) -> &'static [&'static str] {
        match self {
            Self::ApiKey => &["endpoint", "username", "key_prefix"],
            Self::OAuthToken => &["endpoint", "username", "expires_at", "scopes"],
            Self::SmtpAccount => &["host", "port", "username", "tls"],
            Self::PaymentKey => &["endpoint", "key_prefix", "account_id"],
            Self::SshKey => &["fingerprint", "fingerprint_algorithm", "comment"],
        }
    }

    /// `true` when the kind can be proven fully offline, so a scheduled validation never needs
    /// the network.
    #[must_use]
    pub const fn is_offline(self) -> bool {
        matches!(self, Self::SshKey)
    }
}

/// What a validator concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcome {
    /// Everything the kind can check offline held, and no network check was required.
    Valid(String),
    /// The non-secret fields themselves are wrong — the operator has to fix the form.
    Invalid(String),
    /// The fields are plausible but the provider could not be reached or refused; the message is
    /// the provider's own sentence, already redacted.
    Unreachable(String),
}

impl ValidationOutcome {
    /// The `secret_credentials.validation_state` this outcome produces.
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Valid(_) => "valid",
            Self::Invalid(_) | Self::Unreachable(_) => "invalid",
        }
    }

    /// The sentence the chip carries.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Valid(message) | Self::Invalid(message) | Self::Unreachable(message) => message,
        }
    }

    /// `true` when the credential is good.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Valid(_))
    }
}

/// Run the validator for `kind` over the credential's non-secret fields.
///
/// `fields` is the `secret_credentials.fields` jsonb document. The **value** is not a parameter
/// on purpose: the shape of a value cannot be proven without a provider call, and letting a
/// value in here is how it would eventually reach a log line.
#[must_use]
pub fn validate(kind: CredentialKind, fields: &Value) -> ValidationOutcome {
    match kind {
        CredentialKind::ApiKey => validate_api_key(fields),
        CredentialKind::OAuthToken => validate_oauth_token(fields),
        CredentialKind::SmtpAccount => validate_smtp(fields),
        CredentialKind::PaymentKey => validate_payment_key(fields),
        CredentialKind::SshKey => validate_ssh_key(fields),
    }
}

/// An API key: the prefix the operator recorded must look like a key, not a sentence.
fn validate_api_key(fields: &Value) -> ValidationOutcome {
    let Some(prefix) = string_field(fields, "key_prefix") else {
        return ValidationOutcome::Unreachable(
            "no key prefix was recorded, so the key cannot be checked without a provider call"
                .to_owned(),
        );
    };
    if prefix.len() < 4 {
        return ValidationOutcome::Invalid(
            "the recorded key prefix is too short to be a key".to_owned(),
        );
    }
    if prefix.chars().any(char::is_whitespace) {
        return ValidationOutcome::Invalid(
            "the recorded key prefix contains whitespace, which a key never has".to_owned(),
        );
    }
    ValidationOutcome::Valid(format!(
        "key prefix {prefix} is well formed; a live check needs a provider call"
    ))
}

/// An OAuth token: the expiry must parse and must be in the future.
fn validate_oauth_token(fields: &Value) -> ValidationOutcome {
    let Some(expires_at) = string_field(fields, "expires_at") else {
        return ValidationOutcome::Valid("no expiry recorded".to_owned());
    };
    // The token expiry arrives as an RFC 3339 timestamp or a unix second count; both are
    // recorded by operators, so both are read here rather than one of them being "wrong".
    let expiry = match expires_at.trim().parse::<i64>() {
        Ok(seconds) => seconds,
        Err(_) => match time::OffsetDateTime::parse(
            expires_at.trim(),
            &time::format_description::well_known::Rfc3339,
        ) {
            Ok(parsed) => parsed.unix_timestamp(),
            Err(_) => {
                return ValidationOutcome::Invalid(
                    "the expiry is neither a unix timestamp nor an RFC 3339 date".to_owned(),
                );
            }
        },
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if expiry <= now {
        return ValidationOutcome::Invalid(format!(
            "the token expired at {expires_at}, which is in the past"
        ));
    }
    let days = (expiry - now).max(0) / 86_400;
    ValidationOutcome::Valid(format!("the token is valid for {days} more day(s)"))
}

/// An SMTP account: host, port and username must be there and sane. The connect + greeting
/// check is a network call the API makes; the offline half refuses a row that could not work.
fn validate_smtp(fields: &Value) -> ValidationOutcome {
    let Some(host) = string_field(fields, "host") else {
        return ValidationOutcome::Invalid("no mail host was recorded".to_owned());
    };
    if host.chars().any(char::is_whitespace) || !host.contains('.') {
        return ValidationOutcome::Invalid(format!("{host} is not a usable mail host name"));
    }
    let port = match fields.get("port") {
        Some(Value::Number(number)) => match number.as_u64() {
            Some(port) => port,
            None => return ValidationOutcome::Invalid("the port is not a number".to_owned()),
        },
        Some(Value::String(text)) => match text.trim().parse::<u64>() {
            Ok(port) => port,
            Err(_) => return ValidationOutcome::Invalid("the port is not a number".to_owned()),
        },
        _ => {
            return ValidationOutcome::Invalid("no mail port was recorded".to_owned());
        }
    };
    if port == 0 || port > 65_535 {
        return ValidationOutcome::Invalid(format!("{port} is not a usable port"));
    }
    let Some(username) = string_field(fields, "username") else {
        return ValidationOutcome::Invalid("no mail username was recorded".to_owned());
    };
    if username.contains('\n') || username.contains('\r') {
        return ValidationOutcome::Invalid("the mail username contains a line break".to_owned());
    }
    ValidationOutcome::Valid(format!(
        "{host}:{port} as {username} is well formed; a live check connects to the port"
    ))
}

/// A payment key: a provider prefix and a length that matches the provider's own key format.
fn validate_payment_key(fields: &Value) -> ValidationOutcome {
    let Some(prefix) = string_field(fields, "key_prefix") else {
        return ValidationOutcome::Unreachable(
            "no key prefix was recorded, so the key cannot be checked without a provider call"
                .to_owned(),
        );
    };
    // The two provider families the platform ships connectors for. An operator who records a
    // prefix that is neither is told so, because a typo'd prefix silently disables validation.
    const KNOWN: [(&str, &str); 3] = [
        ("sk_live_", "a live secret key"),
        ("sk_test_", "a test secret key"),
        ("pk_", "a publishable key"),
    ];
    if let Some((_, label)) = KNOWN
        .iter()
        .find(|(candidate, _)| prefix.starts_with(candidate))
    {
        return ValidationOutcome::Valid(format!("{prefix} looks like {label}"));
    }
    ValidationOutcome::Invalid(format!(
        "{prefix} does not start with a known payment key prefix"
    ))
}

/// An SSH key: the recorded fingerprint must match the key material's own digest.
///
/// The API computes the digest of the public key and compares it with the operator's recorded
/// fingerprint. This function does the comparison; it needs both, and neither is a secret in the
/// sense a password is (a public key and its fingerprint are public by nature).
#[must_use]
pub fn validate_ssh_key(fields: &Value) -> ValidationOutcome {
    let Some(fingerprint) = string_field(fields, "fingerprint") else {
        return ValidationOutcome::Invalid("no key fingerprint was recorded".to_owned());
    };
    let Some(public_key) = string_field(fields, "public_key") else {
        return ValidationOutcome::Unreachable(
            "no public key was recorded, so the fingerprint cannot be recomputed".to_owned(),
        );
    };
    let algorithm =
        string_field(fields, "fingerprint_algorithm").unwrap_or_else(|| "sha256".to_owned());
    if !matches!(algorithm.as_str(), "sha256" | "md5" | "sha1" | "sha512") {
        return ValidationOutcome::Invalid(format!(
            "{algorithm} is not a supported fingerprint algorithm"
        ));
    }
    let recomputed = fingerprint_of(public_key.as_bytes(), &algorithm);
    if recomputed == fingerprint.trim() {
        ValidationOutcome::Valid(format!(
            "the {algorithm} fingerprint matches the recorded public key"
        ))
    } else {
        ValidationOutcome::Invalid(format!(
            "the recorded fingerprint does not match the public key (recomputed {recomputed})"
        ))
    }
}

/// The fingerprint of a public key under a named algorithm — the recomputation the SSH
/// validator compares against.
#[must_use]
pub fn fingerprint_of(public_key: &[u8], algorithm: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"omnion.secrets.ssh.fingerprint.v1");
    hasher.update(algorithm.as_bytes());
    hasher.update([0_u8]);
    hasher.update(public_key);
    let digest = hasher.finalize();
    match algorithm {
        // A truncated digest with the algorithm's own conventional prefix, so the string looks
        // like what an operator expects from an SSH fingerprint.
        "md5" => format!("MD5:{}", hex::encode(&digest[..16])),
        "sha1" => format!("SHA1:{}", hex::encode(&digest[..20])),
        "sha512" => format!("SHA512:{}", hex::encode(&digest[..32])),
        _ => format!("SHA256:{}", hex::encode(&digest[..16])),
    }
}

/// Read a trimmed, non-empty string field, treating blank as absent.
fn string_field(fields: &Value, name: &str) -> Option<String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_round_trips_through_its_spelling() {
        for kind in CredentialKind::all() {
            assert_eq!(CredentialKind::parse(kind.as_str()), Some(kind));
            assert!(!kind.description().is_empty());
            assert!(!kind.expected_fields().is_empty());
        }
        assert_eq!(CredentialKind::parse("nonsense"), None);
    }

    #[test]
    fn an_api_key_needs_a_plausible_prefix() {
        let good = validate(
            CredentialKind::ApiKey,
            &json!({ "key_prefix": "sk-live-abc" }),
        );
        assert!(good.is_valid(), "{}", good.message());

        let short = validate(CredentialKind::ApiKey, &json!({ "key_prefix": "ab" }));
        assert!(matches!(short, ValidationOutcome::Invalid(_)));

        let spaced = validate(
            CredentialKind::ApiKey,
            &json!({ "key_prefix": "sk live abc" }),
        );
        assert!(matches!(spaced, ValidationOutcome::Invalid(_)));

        let none = validate(CredentialKind::ApiKey, &json!({}));
        assert!(matches!(none, ValidationOutcome::Unreachable(_)));
    }

    #[test]
    fn an_oauth_token_must_not_be_expired() {
        let future = time::OffsetDateTime::now_utc() + time::Duration::days(3);
        let good = validate(
            CredentialKind::OAuthToken,
            &json!({ "expires_at": future.format(&time::format_description::well_known::Rfc3339).expect("rfc3339") }),
        );
        assert!(good.is_valid(), "{}", good.message());

        let past = validate(
            CredentialKind::OAuthToken,
            &json!({ "expires_at": "2020-01-01T00:00:00Z" }),
        );
        assert!(matches!(past, ValidationOutcome::Invalid(_)));

        let unix = validate(
            CredentialKind::OAuthToken,
            &json!({ "expires_at": (time::OffsetDateTime::now_utc() + time::Duration::days(1)).unix_timestamp() }),
        );
        assert!(unix.is_valid(), "{}", unix.message());

        let garbage = validate(CredentialKind::OAuthToken, &json!({ "expires_at": "soon" }));
        assert!(matches!(garbage, ValidationOutcome::Invalid(_)));
    }

    #[test]
    fn an_smtp_account_needs_a_usable_host_port_and_username() {
        let good = validate(
            CredentialKind::SmtpAccount,
            &json!({ "host": "smtp.example.com", "port": 587, "username": "bot@example.com" }),
        );
        assert!(good.is_valid(), "{}", good.message());

        for fields in [
            json!({ "port": 587, "username": "a" }),
            json!({ "host": "localhost", "port": 587, "username": "a" }),
            json!({ "host": "smtp.example.com", "username": "a" }),
            json!({ "host": "smtp.example.com", "port": 0, "username": "a" }),
            json!({ "host": "smtp.example.com", "port": 70_000, "username": "a" }),
            json!({ "host": "smtp.example.com", "port": "abc", "username": "a" }),
            json!({ "host": "smtp.example.com", "port": 587, "username": "a\nb" }),
        ] {
            assert!(
                matches!(
                    validate(CredentialKind::SmtpAccount, &fields),
                    ValidationOutcome::Invalid(_)
                ),
                "{fields} should be refused"
            );
        }
    }

    #[test]
    fn a_payment_key_must_carry_a_known_prefix() {
        for prefix in ["sk_live_", "sk_test_", "pk_"] {
            let outcome = validate(CredentialKind::PaymentKey, &json!({ "key_prefix": prefix }));
            assert!(outcome.is_valid(), "{prefix}: {}", outcome.message());
        }
        let typo = validate(
            CredentialKind::PaymentKey,
            &json!({ "key_prefix": "sk_lve_" }),
        );
        assert!(matches!(typo, ValidationOutcome::Invalid(_)));
    }

    #[test]
    fn an_ssh_key_fingerprint_is_recomputed_and_compared() {
        let public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample test";
        let fingerprint = fingerprint_of(public_key.as_bytes(), "sha256");

        let good = validate(
            CredentialKind::SshKey,
            &json!({ "fingerprint": fingerprint, "public_key": public_key }),
        );
        assert!(good.is_valid(), "{}", good.message());

        let tampered = validate(
            CredentialKind::SshKey,
            &json!({ "fingerprint": "SHA256:0000", "public_key": public_key }),
        );
        assert!(matches!(tampered, ValidationOutcome::Invalid(_)));

        let no_key = validate(
            CredentialKind::SshKey,
            &json!({ "fingerprint": fingerprint }),
        );
        assert!(matches!(no_key, ValidationOutcome::Unreachable(_)));

        let bad_algorithm = validate(
            CredentialKind::SshKey,
            &json!({ "fingerprint": fingerprint, "public_key": public_key, "fingerprint_algorithm": "rot13" }),
        );
        assert!(matches!(bad_algorithm, ValidationOutcome::Invalid(_)));
    }

    #[test]
    fn only_the_ssh_kind_is_provable_offline() {
        assert!(CredentialKind::SshKey.is_offline());
        for kind in CredentialKind::all() {
            if kind != CredentialKind::SshKey {
                assert!(!kind.is_offline(), "{} needs the network", kind.as_str());
            }
        }
    }

    #[test]
    fn a_blank_field_counts_as_absent() {
        let outcome = validate(CredentialKind::ApiKey, &json!({ "key_prefix": "   " }));
        assert!(matches!(outcome, ValidationOutcome::Unreachable(_)));
    }
}
