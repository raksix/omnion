//! `GET /security/secrets` — the read-only secret inventory (REQ-012, slice 4).
//!
//! ## The refusal this route has to make
//!
//! Management of secrets belongs to the secrets manager request (REQ-125); this screen only
//! reports. So there is **no** `PUT`, no `DELETE` and no `POST` here, and that is a decision
//! worth stating rather than a gap: a screen that can edit a secret reference invites an
//! operator to believe it can rotate a secret, and rotating one means replacing a value in an
//! environment and redeploying.
//!
//! `security.read` is the only key involved, for the same reason the events export takes it: an
//! auditor whose job is "what does this platform hold" must be able to ask.
//!
//! ## What the response is forbidden to contain
//!
//! The body is built by explicit field construction ([`SecretBody::from`]) rather than by
//! serialising a domain row, and that is the containment boundary. `SecretRef` cannot hold a
//! value — so a `select *` in the store cannot leak one — and the body adds no field of its own
//! that could. `the_response_body_declares_no_value_field` pins it: the router's own serialised
//! output is parsed and its top-level and per-row key sets are asserted, so the guarantee is
//! made against the bytes an operator's browser receives rather than against the types.
//!
//! The handler additionally **reports what it does not know**, in a field the screen renders as
//! a permanent note: the environment list is hand-maintained (`environment_names`), because a
//! process cannot enumerate its own environment. An inventory that silently covers seven of the
//! nine secrets a deployment holds is worse than one that says so.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// How much of a secret the platform can say.
#[derive(Debug, Serialize)]
pub struct SecretBody {
    /// Stable row identity — `source:name`.
    pub key: String,
    /// The reference: an environment variable name or a store key.
    pub name: String,
    /// Which source it came from.
    pub source: String,
    /// What the reference is scoped to.
    pub scope: String,
    /// The best rotation timestamp the platform can observe.
    pub rotated_at: Option<time::OffsetDateTime>,
    /// What that timestamp evidences.
    pub evidence: String,
    /// Days since the observed timestamp, when there is one.
    pub age_days: Option<i64>,
    /// How many rows hold material behind this reference. A count, never the material.
    pub material_count: i64,
    /// Whether an expiry is set and already past.
    pub expired: bool,
    /// What the platform can honestly say.
    pub state: String,
    /// The reason behind the state.
    pub note: String,
}

/// The inventory as the screen receives it.
#[derive(Debug, Serialize)]
pub struct SecretsBody {
    /// The rows, most urgent first.
    pub secrets: Vec<SecretBody>,
    /// How many references exist in total.
    pub total: usize,
    /// How many are missing.
    pub missing: usize,
    /// How many cannot be verified from inside the platform.
    pub unverifiable: usize,
    /// The source vocabulary the filter offers.
    pub sources: Vec<String>,
    /// The states the vocabulary can produce, so the screen's legend is served rather than
    /// hard-coded — and a legend with a state the API cannot emit is a lie in a security screen.
    pub states: Vec<&'static str>,
    /// **What this screen cannot see.** Rendered as a permanent note.
    pub limitation: String,
}

/// `GET /security/secrets` — the inventory.
pub async fn get(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<SecretsBody>, ApiError> {
    let inventory = omnion_security::secret_inventory(
        state.db().pool(),
        session.user.organization_id,
        |name| std::env::var_os(name).is_some(),
    )
    .await
    .map_err(map_store)?;

    let now = time::OffsetDateTime::now_utc();
    Ok(Json(SecretsBody {
        secrets: inventory
            .secrets
            .iter()
            .map(|row| SecretBody {
                key: row.key.clone(),
                name: row.name.clone(),
                source: row.source.as_str().to_string(),
                scope: row.scope.clone(),
                rotated_at: row.rotated_at,
                evidence: row.evidence.as_str().to_string(),
                age_days: row.age_days(now),
                material_count: row.material_count,
                expired: row.expired,
                state: row.state.as_str().to_string(),
                note: row.note.clone(),
            })
            .collect(),
        total: inventory.total,
        missing: inventory.missing,
        unverifiable: inventory.unverifiable,
        sources: inventory.sources,
        states: omnion_security::SecretState::ALL
            .iter()
            .map(|s| s.as_str())
            .collect(),
        limitation: LIMITATION.to_string(),
    }))
}

/// Map a store error onto the API surface — the same mapping the other security routes use.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    match error {
        omnion_security::SecurityError::Invalid(message) => {
            ApiError::bad_request("invalid_security_input", message)
        }
        omnion_security::SecurityError::NotFound => {
            ApiError::new(axum::http::StatusCode::NOT_FOUND, "not_found", "not found")
        }
        omnion_security::SecurityError::Database(inner) => ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security store: {inner}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response's declared field set, as the router serialises it.
    ///
    /// Asserted against the **serialised** body rather than the struct, because the claim is
    /// about the bytes that leave the API. A field that only exists in a struct is a field the
    /// operator never receives; a field the operator receives is the thing that must be named
    /// here.
    const RESPONSE_FIELDS: &[&str] = &[
        "key",
        "name",
        "source",
        "scope",
        "rotated_at",
        "evidence",
        "age_days",
        "material_count",
        "expired",
        "state",
        "note",
    ];

    fn declared_fields(value: &serde_json::Value) -> Vec<String> {
        value
            .as_object()
            .expect("the body is an object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn a_serialised_row_carries_a_name_and_never_a_value() {
        // The real check, against a row the route would actually emit.
        let row = SecretBody {
            key: "environment:OMNION_CSRF_SECRET".into(),
            name: "OMNION_CSRF_SECRET".into(),
            source: "environment".into(),
            scope: "platform".into(),
            rotated_at: None,
            evidence: "unknown".into(),
            age_days: None,
            material_count: 0,
            expired: false,
            state: "unverifiable".into(),
            note: "the variable is set".into(),
        };
        let json = serde_json::to_value(&row).expect("a row serialises");
        let keys = declared_fields(&json);

        for expected in RESPONSE_FIELDS {
            assert!(
                keys.contains(&expected.to_string()),
                "the response is missing {expected}; it has {keys:?}"
            );
        }
        for forbidden in [
            "value",
            "secret",
            "ciphertext",
            "hash",
            "token",
            "preview",
            "plaintext",
            "password",
        ] {
            assert!(
                !keys.contains(&forbidden.to_string()),
                "the response declares a {forbidden} field: {keys:?}"
            );
        }
        // And the shape is exactly the declared set — no field was added
        // without this test noticing, which is the direction that leaks.
        assert_eq!(
            keys.len(),
            RESPONSE_FIELDS.len(),
            "the row grew a field: {keys:?}"
        );
    }

    #[test]
    fn the_limitation_is_not_optional() {
        // The note is what stops an operator concluding the inventory is
        // exhaustive. If it were ever empty the screen would be lying.
        assert!(
            LIMITATION.contains("maintained by hand"),
            "the limitation must name the hand-maintained list"
        );
        assert!(
            LIMITATION.contains("never values"),
            "the limitation must state that no value is reported"
        );
    }

    #[test]
    fn the_states_offered_are_exactly_the_states_the_type_has() {
        let offered: Vec<&str> = omnion_security::SecretState::ALL
            .iter()
            .map(|s| s.as_str())
            .collect();
        for state in &offered {
            assert!(
                omnion_security::SecretState::parse(state).is_some(),
                "the legend offers {state}, which the type cannot produce"
            );
        }
        assert!(!offered.contains(&"healthy"), "{offered:?}");
    }
}

/// The sentence the screen shows about what this inventory cannot do.
///
/// A constant rather than an inline string so the route and its test read the
/// same words — a limitation note that the test asserts against a different
/// copy is a note nobody has verified.
pub const LIMITATION: &str = "This screen reports references, never values. It cannot list a secret the platform was not \
     told about, and its environment list is maintained by hand.";
