//! What a provider asserts, and what that assertion turns into (docs/07-IAM.md §11).
//!
//! Every protocol reduces to one shape — [`Identity`] — before the platform decides anything:
//! a subject id, an email, a display name, a set of group values and free attributes. The JIT and
//! role-mapping rules then read that one shape, so a new protocol never gets its own provisioning
//! policy and a claim-set change cannot fix one protocol and miss another.
//!
//! Two rules shape the mapping. **The email is the identity**: it is what an account is keyed by,
//! it is normalized the same way every local account is, and a provider that cannot supply one
//! cannot provision anyone. **Roles are named, never invented**: a claim value is mapped to a role
//! that must already exist, so a directory cannot mint a role nobody administers.

use serde_json::Value;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// The normalized assertion of one provider sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The provider's own subject id — stable for this person inside this provider.
    pub subject: String,
    /// Lowercased email; the key an account is matched or created on.
    pub email: String,
    /// Human name, when the provider sends one.
    pub display_name: Option<String>,
    /// Group / role values extracted from the configured claim.
    pub groups: Vec<String>,
    /// Everything else the provider sent, kept as attributes for ABAC conditions.
    pub attributes: serde_json::Map<String, Value>,
}

impl Identity {
    /// The name shown when a JIT account is created and the provider sent none.
    #[must_use]
    pub fn display_name_or_email(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.email)
    }
}

/// Build an identity from a claims object, pulling the email out of the first claim that holds
/// one and the groups out of the configured claim.
///
/// The email claim is looked up in a fixed order (`email`, then the provider-specific
/// `config.email_claim`, then a name that looks like a mail address) rather than trusted blindly,
/// so a token that only carries a name is refused with a reason instead of provisioning an account
/// nobody can receive mail for.
pub fn identity_from_claims(
    claims: &serde_json::Map<String, Value>,
    email_claim: Option<&str>,
    group_claim: Option<&str>,
) -> Result<Identity> {
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            IdentityError::InvalidProvider("the assertion carries no subject id (`sub`)".into())
        })?
        .to_owned();

    let email = resolve_email(claims, email_claim)?;

    let groups = group_claim
        .and_then(|claim| claims.get(claim))
        .map(collect_groups)
        .unwrap_or_default();

    let mut attributes = claims.clone();
    for structural in ["sub", "iss", "aud", "exp", "iat", "nonce"] {
        attributes.remove(structural);
    }
    if let Some(claim) = email_claim {
        attributes.remove(claim);
    }
    if let Some(claim) = group_claim {
        attributes.remove(claim);
    }

    Ok(Identity {
        subject,
        email,
        display_name: resolve_display_name(claims),
        groups,
        attributes,
    })
}

/// Find the email in a claims object, normalized the way a local account's is.
fn resolve_email(
    claims: &serde_json::Map<String, Value>,
    email_claim: Option<&str>,
) -> Result<String> {
    let mut candidate: Option<String> = None;

    for name in email_claim.into_iter().chain(["email"]) {
        if let Some(value) = claims.get(name).and_then(Value::as_str) {
            candidate = Some(value.to_owned());
            break;
        }
    }

    // A provider that does not use the standard claim still usually puts a mail-shaped value in
    // a claim whose name says so; looking there is a fallback, not a rule.
    if candidate.is_none() {
        for (name, value) in claims {
            let lowered = name.to_ascii_lowercase();
            if (lowered.contains("email") || lowered.contains("mail"))
                && let Some(text) = value.as_str()
            {
                candidate = Some(text.to_owned());
                break;
            }
        }
    }

    let email = candidate
        .map(|value| value.trim().to_owned())
        .ok_or_else(|| {
            IdentityError::InvalidProvider("the assertion carries no email address".into())
        })?;

    if !email.contains('@') || email.starts_with('@') || email.ends_with('@') || email.contains(' ')
    {
        return Err(IdentityError::InvalidProvider(
            "the assertion carries an email address that is not an address".into(),
        ));
    }
    Ok(email.to_ascii_lowercase())
}

/// A display name from the claims that conventionally carry one.
fn resolve_display_name(claims: &serde_json::Map<String, Value>) -> Option<String> {
    for name in ["name", "preferred_username", "display_name"] {
        if let Some(value) = claims.get(name).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_owned());
        }
    }
    // Some providers only send the two halves; joining them is better than showing an address.
    let given = claims.get("given_name").and_then(Value::as_str);
    let family = claims.get("family_name").and_then(Value::as_str);
    match (given, family) {
        (Some(given), Some(family)) => Some(format!("{given} {family}").trim().to_owned()),
        (Some(given), None) => Some(given.to_owned()),
        _ => None,
    }
}

/// Read a group claim as a list, whatever shape it arrived in.
///
/// A directory sends groups as an array of strings, as a single space-delimited string, or as a
/// nested array after a mapping — all three are common, and refusing two of them would make the
/// feature fail for reasons the operator cannot see. Values are trimmed, de-duplicated and
/// compared case-insensitively downstream.
#[must_use]
pub fn collect_groups(value: &Value) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    let mut push = |candidate: &str| {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() && !groups.iter().any(|existing| existing == trimmed) {
            groups.push(trimmed.to_owned());
        }
    };

    match value {
        Value::String(text) => {
            for part in text.split([' ', ',', ';']) {
                push(part);
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(text) => push(text),
                    Value::Array(nested) => {
                        for inner in nested {
                            if let Some(text) = inner.as_str() {
                                push(text);
                            }
                        }
                    }
                    other => {
                        if let Some(text) = other.as_str() {
                            push(text);
                        }
                    }
                }
            }
        }
        other => {
            if let Some(text) = other.as_str() {
                push(text);
            }
        }
    }
    groups
}

/// One claim → role rule as the panel stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleMapping {
    /// The claim value that triggers the rule (a group name, matched case-insensitively).
    pub claim_value: String,
    /// The role's slug inside the organization.
    pub role_slug: String,
    /// Optional dotted path into the claim object; the value at the path is matched instead.
    pub claim_path: Option<String>,
}

impl RoleMapping {
    /// Parse one mapping from the provider's `config.role_mappings` array.
    pub fn from_value(value: &Value) -> Result<Self> {
        let claim_value = value
            .get("claim_value")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                IdentityError::InvalidProvider("a role mapping needs a `claim_value`".into())
            })?;
        let role_slug = value
            .get("role_slug")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                IdentityError::InvalidProvider("a role mapping needs a `role_slug`".into())
            })?;
        let claim_path = value
            .get("claim_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        Ok(Self {
            claim_value: claim_value.to_owned(),
            role_slug: role_slug.to_owned(),
            claim_path,
        })
    }

    /// Does this rule fire for an identity? The value is compared case-insensitively, because
    /// directories disagree about casing far more often than they disagree about names.
    #[must_use]
    pub fn matches(&self, identity: &Identity) -> bool {
        let values = match &self.claim_path {
            Some(path) => value_at_path(&Value::Object(identity.attributes.clone()), path)
                .map(collect_groups)
                .unwrap_or_default(),
            None => identity.groups.clone(),
        };
        let wanted = self.claim_value.to_ascii_lowercase();
        values
            .iter()
            .any(|value| value.to_ascii_lowercase() == wanted)
    }
}

/// Read a dotted path out of a JSON value (`a.b.0.c`).
#[must_use]
pub fn value_at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.').filter(|part| !part.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// The roles an identity's claims resolve to, in rule order and de-duplicated.
#[must_use]
pub fn resolve_roles(identity: &Identity, mappings: &[RoleMapping]) -> Vec<String> {
    let mut roles: Vec<String> = Vec::new();
    for mapping in mappings {
        if mapping.matches(identity)
            && !roles
                .iter()
                .any(|slug| slug.eq_ignore_ascii_case(&mapping.role_slug))
        {
            roles.push(mapping.role_slug.clone());
        }
    }
    roles
}

/// The roles a claim set maps to, read straight from the provider's `config` document.
pub fn mappings_from_config(config: &Value) -> Result<Vec<RoleMapping>> {
    let list = match config.get("role_mappings") {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => return Ok(Vec::new()),
        Some(_) => {
            return Err(IdentityError::InvalidProvider(
                "`role_mappings` must be an array of {claim_value, role_slug} objects".into(),
            ));
        }
    };
    list.iter().map(RoleMapping::from_value).collect()
}

/// The role a provider grants by default, if it names one.
#[must_use]
pub fn default_role_id(config: &Value, column: Option<Uuid>) -> Option<Uuid> {
    column.or_else(|| {
        config
            .get("default_role_slug")
            .and_then(Value::as_str)
            .and_then(|slug| Uuid::parse_str(slug).ok())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn claims(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    #[test]
    fn an_oidc_id_token_becomes_one_identity() {
        let identity = identity_from_claims(
            &claims(json!({
                "sub": "00u1abc",
                "email": "Robin.Fielding@Example.COM",
                "name": "Robin Fielding",
                "groups": ["editors", "reviewers"],
                "iss": "https://idp.example",
                "aud": "omnion",
                "nonce": "abc",
            })),
            None,
            Some("groups"),
        )
        .expect("a complete token");

        assert_eq!(identity.subject, "00u1abc");
        assert_eq!(identity.email, "robin.fielding@example.com");
        assert_eq!(identity.display_name.as_deref(), Some("Robin Fielding"));
        assert_eq!(identity.groups, vec!["editors", "reviewers"]);
        // The protocol's own fields are not attributes: an ABAC policy has no business reading
        // a nonce, and leaving them in would make a condition pass on issuer instead of person.
        assert!(!identity.attributes.contains_key("iss"));
        assert!(!identity.attributes.contains_key("nonce"));
        assert!(!identity.attributes.contains_key("groups"));
    }

    #[test]
    fn a_token_without_a_subject_or_an_email_is_refused() {
        let no_subject = identity_from_claims(&claims(json!({ "email": "a@b.co" })), None, None);
        assert!(no_subject.is_err(), "no subject means no identity to match");

        let no_email = identity_from_claims(&claims(json!({ "sub": "1" })), None, None);
        assert!(no_email.is_err(), "no email means no account to provision");

        let broken = identity_from_claims(
            &claims(json!({ "sub": "1", "email": "not-an-address" })),
            None,
            None,
        );
        assert!(
            broken.is_err(),
            "a malformed address must not provision an account"
        );
    }

    #[test]
    fn a_provider_specific_email_claim_wins() {
        let identity = identity_from_claims(
            &claims(json!({
                "sub": "1",
                "email": "primary@example.com",
                "work_email": "r.fielding@example.com",
            })),
            Some("work_email"),
            None,
        )
        .expect("the configured claim carries the address");

        assert_eq!(identity.email, "r.fielding@example.com");
    }

    #[test]
    fn groups_arrive_in_every_shape_a_directory_sends_them() {
        assert_eq!(collect_groups(&json!(["a", "b"])), vec!["a", "b"]);
        assert_eq!(collect_groups(&json!("a b  c")), vec!["a", "b", "c"]);
        assert_eq!(
            collect_groups(&json!([["a"], ["b", "c"]])),
            vec!["a", "b", "c"]
        );
        assert_eq!(collect_groups(&json!([])), Vec::<String>::new());
        assert_eq!(collect_groups(&json!("a, b; c")), vec!["a", "b", "c"]);
    }

    #[test]
    fn groups_are_read_from_the_configured_claim() {
        let identity = identity_from_claims(
            &claims(json!({
                "sub": "1",
                "email": "a@b.co",
                "roles": ["content.admin"],
                "groups": ["ignored"],
            })),
            None,
            Some("roles"),
        )
        .expect("a token with a roles claim");

        assert_eq!(identity.groups, vec!["content.admin"]);
    }

    #[test]
    fn a_claim_value_maps_to_a_role_case_insensitively() {
        let identity = identity_from_claims(
            &claims(json!({ "sub": "1", "email": "a@b.co", "groups": ["Editors"] })),
            None,
            Some("groups"),
        )
        .expect("a token with groups");
        let mappings = vec![RoleMapping {
            claim_value: "editors".into(),
            role_slug: "content-editor".into(),
            claim_path: None,
        }];

        assert_eq!(resolve_roles(&identity, &mappings), vec!["content-editor"]);
    }

    #[test]
    fn a_mapping_can_read_a_dotted_path_instead_of_the_group_claim() {
        let identity = identity_from_claims(
            &claims(json!({
                "sub": "1",
                "email": "a@b.co",
                "meta": { "teams": ["platform"] },
            })),
            None,
            Some("groups"),
        )
        .expect("a token with a nested claim");
        let mappings = vec![RoleMapping {
            claim_value: "platform".into(),
            role_slug: "sre".into(),
            claim_path: Some("meta.teams".into()),
        }];

        assert_eq!(resolve_roles(&identity, &mappings), vec!["sre"]);
    }

    #[test]
    fn one_claim_value_does_not_produce_the_same_role_twice() {
        let identity = identity_from_claims(
            &claims(json!({ "sub": "1", "email": "a@b.co", "groups": ["editors", "editors"] })),
            None,
            Some("groups"),
        )
        .expect("a repeated group");
        let mappings = vec![
            RoleMapping {
                claim_value: "editors".into(),
                role_slug: "content-editor".into(),
                claim_path: None,
            },
            RoleMapping {
                claim_value: "EDITORS".into(),
                role_slug: "content-editor".into(),
                claim_path: None,
            },
        ];

        assert_eq!(resolve_roles(&identity, &mappings), vec!["content-editor"]);
    }

    #[test]
    fn a_mapping_without_both_halves_is_refused() {
        assert!(RoleMapping::from_value(&json!({ "role_slug": "x" })).is_err());
        assert!(RoleMapping::from_value(&json!({ "claim_value": "x" })).is_err());
        assert!(mappings_from_config(&json!({ "role_mappings": {} })).is_err());
        assert!(
            mappings_from_config(&json!({}))
                .expect("no mappings is not an error")
                .is_empty()
        );
    }

    #[test]
    fn a_display_name_is_assembled_when_only_the_halves_arrive() {
        let identity = identity_from_claims(
            &claims(json!({
                "sub": "1",
                "email": "a@b.co",
                "given_name": "Robin",
                "family_name": "Fielding",
            })),
            None,
            None,
        )
        .expect("a token with name halves");

        assert_eq!(identity.display_name.as_deref(), Some("Robin Fielding"));
        assert_eq!(identity.display_name_or_email(), "Robin Fielding");
    }
}
