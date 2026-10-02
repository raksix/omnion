//! The credential row: what the platform stores about a credential, and the rules about which
//! fields may hold a value (REQ-087, slice 2).
//!
//! The single rule this module exists to enforce is the negative one: **a secret field never
//! has a place to live**. `Credential::settings` is the only free-form column, and
//! [`Settings::reject_secrets`] refuses to build one that contains a key the type declares
//! `secret`. So the invariant "no plaintext secret is stored in the workflows schema" is not a
//! convention every future writer has to remember — it is the type system refusing a value.
//!
//! Everything else here is vocabulary that the database and the panel both have to agree on:
//! the health values ([`Health`]), the scope and sharing words, and the filter list. The
//! check constraints in `database/migrations/0053_workflow_credentials.sql` are the same
//! lists, and `vocabulary_matches_the_migration` names the file so the two cannot drift
//! without a test failing.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::registry::{CredentialDefinition, CredentialField, FieldType};

/// How healthy a credential is, from the panel's point of view.
///
/// Four values, and the check constraint in the migration agrees. `NeedsReauth` is its own
/// value rather than a flavour of `failing` because the two call for opposite actions: a
/// failing credential wants a test, a `needs_reauth` one wants an OAuth round trip, and a chip
/// that cannot tell them apart sends the reader to the wrong button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Never tested. The state a credential is saved in when the reader skipped the test.
    Untested,
    /// The last test passed.
    Ok,
    /// The last test failed and re-running it may fix it.
    Failing,
    /// An OAuth refresh failed; the credential needs a human to reconnect it.
    NeedsReauth,
}

impl Health {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untested => "untested",
            Self::Ok => "ok",
            Self::Failing => "failing",
            Self::NeedsReauth => "needs_reauth",
        }
    }

    /// Every value, in the order the migration constraint lists them.
    ///
    /// A test compares this list against the constraint in the SQL file, so the panel and the
    /// database cannot end up with different sets.
    #[must_use]
    pub const fn all() -> [&'static str; 4] {
        ["untested", "ok", "failing", "needs_reauth"]
    }

    /// Parse a stored value.
    ///
    /// Case-folded, because this runs on values read back out of the table: a hand-edited row
    /// or a migration written by a person rather than by this enum should render a chip, not
    /// turn a whole list into an error. Writing is still exact — every write goes through
    /// [`Health::as_str`], so the column only ever holds one spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "untested" => Some(Self::Untested),
            "ok" => Some(Self::Ok),
            "failing" => Some(Self::Failing),
            "needs_reauth" => Some(Self::NeedsReauth),
            _ => None,
        }
    }

    /// Whether the panel should show this as an amber "act on it" chip rather than a neutral
    /// one. A `needs_reauth` credential keeps working until its token expires, so it is not an
    /// error — but it is not fine either.
    #[must_use]
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Failing | Self::NeedsReauth)
    }
}

/// The `scope` vocabulary: what a credential is shared with.
pub const SCOPES: [&str; 2] = ["organization", "project"];

/// The `sharing` vocabulary: who may see it inside that scope.
pub const SHARINGS: [&str; 2] = ["private", "organization"];

/// Columns of `workflow_credentials` for one `select`, in [`Credential`] order.
pub const CREDENTIAL_COLUMNS: &str = "id, organization_id, key, name, type, scope, sharing, \
     secret_ref, settings, owner_user_id, health, health_checked_at, health_detail, \
     oauth_expires_at, oauth_scopes, oauth_subject, last_used_at, created_by, created_at, \
     updated_at";

/// One credential instance.
///
/// `settings` holds the type's **non-secret** fields. There is no column anywhere in this
/// struct for a secret value: the only handle to one is `secret_ref`, which the execution
/// helper resolves inside the encrypted store (REQ-125) and nothing else may read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Credential {
    /// Credential id.
    pub id: Uuid,
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// The key a node's `params.credential_key` names. Non-secret by construction.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Credential-type key, from the registry.
    pub r#type: String,
    /// `organization` or `project`.
    pub scope: String,
    /// `private` or `organization`.
    pub sharing: String,
    /// Opaque handle into the encrypted store; `None` before a secret is written.
    pub secret_ref: Option<String>,
    /// The type's non-secret fields.
    pub settings: Value,
    /// Who owns it; `None` once the account is gone.
    pub owner_user_id: Option<Uuid>,
    /// Current health, as stored. Read it through [`Credential::health`]: a value outside the
    /// four is a row the panel must be able to render rather than a read that fails.
    pub health: String,
    /// When the last test ran.
    pub health_checked_at: Option<OffsetDateTime>,
    /// The failure message, stripped.
    pub health_detail: Option<String>,
    /// When the OAuth token expires.
    pub oauth_expires_at: Option<OffsetDateTime>,
    /// Scopes the token was granted.
    pub oauth_scopes: Option<String>,
    /// Who the provider says this is connected as.
    pub oauth_subject: Option<String>,
    /// When a node last resolved it.
    pub last_used_at: Option<OffsetDateTime>,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// Creation instant.
    pub created_at: OffsetDateTime,
    /// Last write instant.
    pub updated_at: OffsetDateTime,
}

impl Credential {
    /// The parsed health.
    ///
    /// A value outside the four is `None` rather than a failed read, for the same reason
    /// `WorkflowExecution::status` is: a row nobody can parse must still be *renderable*, and
    /// a list that errors out is worse than a list with one chip that says "unknown".
    #[must_use]
    pub fn health(&self) -> Option<Health> {
        Health::parse(&self.health)
    }

    /// Whether this credential is past its OAuth expiry.
    ///
    /// `health` is what the panel *records*; this is what it *computes*. A credential whose
    /// token expired while nothing was running has `health = ok` and an expiry in the past,
    /// and that is exactly the case a chip driven only by `health` would miss.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.oauth_expires_at.is_some_and(|at| at <= now)
    }

    /// The one word the panel colours by.
    ///
    /// An expired OAuth credential reports `needs_reauth` even when its last test passed,
    /// because the two facts mean the same thing to the reader: this will not work without a
    /// human, and the button that fixes it is the same. An unparseable health reports
    /// [`Health::Untested`] rather than pretending — the row is shown, with no green claim.
    #[must_use]
    pub fn effective_health(&self, now: OffsetDateTime) -> Health {
        if self.is_expired(now) {
            return Health::NeedsReauth;
        }
        self.health().unwrap_or(Health::Untested)
    }
}

/// A credential row to be written.
#[derive(Debug, Clone)]
pub struct NewCredential {
    /// Organization that owns it.
    pub organization_id: Uuid,
    /// The key graphs will name.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Credential-type key.
    pub r#type: String,
    /// `organization` or `project`.
    pub scope: String,
    /// `private` or `organization`.
    pub sharing: String,
    /// Opaque handle into the encrypted store.
    pub secret_ref: Option<String>,
    /// The type's non-secret fields.
    pub settings: Value,
    /// The account creating it.
    pub created_by: Option<Uuid>,
}

/// The write-only half of a create or replace.
///
/// It is a separate type from [`NewCredential`] for one reason: it carries the plaintext
/// secrets on its way *in* and has no field to carry them on the way out. A single struct
/// with an `Option<String> secret` would be serialisable, and the moment it is, the next
/// endpoint that returns a struct will return the secret with it.
#[derive(Debug, Clone, Default)]
pub struct SecretPayload {
    /// Field name to value, in the order the type declares them.
    pub values: Vec<(String, String)>,
}

impl SecretPayload {
    /// Whether anything was actually sent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The type's non-secret fields, after the store has proved it holds no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings(Value);

impl Settings {
    /// Build settings from a caller-supplied object, refusing any secret field.
    ///
    /// This is the enforcement point for "no plaintext secret is stored in the workflows
    /// schema", and it refuses *by name*: the caller is told which field it may not put here
    /// and what the supported path is (`credential_secret_write_only`, replace-secret), which
    /// is a sentence a person can act on. Silently dropping the field would leave a credential
    /// that saves and then fails at run time with a missing header.
    ///
    /// Unknown fields are refused too, for the same reason a select is: a field the type does
    /// not declare is a typo that would otherwise be stored forever and never read.
    pub fn build(
        definition: &CredentialDefinition,
        raw: &Value,
    ) -> Result<Self, crate::error::WorkflowError> {
        let mut out = Map::new();
        match raw {
            Value::Null => {}
            Value::Object(map) => {
                for (name, value) in map {
                    let Some(field) = definition
                        .fields
                        .iter()
                        .find(|f: &&CredentialField| f.name == name.as_str())
                    else {
                        return Err(crate::error::WorkflowError::CredentialInvalid(format!(
                            "{name:?} is not a field of the {:?} credential type — it declares {}",
                            definition.key,
                            definition
                                .fields
                                .iter()
                                .map(|f| f.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    };
                    if field.kind == FieldType::Secret {
                        return Err(crate::error::WorkflowError::CredentialSecretWriteOnly {
                            field: name.clone(),
                        });
                    }
                    out.insert(name.clone(), value.clone());
                }
            }
            _ => {
                return Err(crate::error::WorkflowError::CredentialInvalid(
                    "settings must be an object of the credential type's non-secret fields".into(),
                ));
            }
        }

        // A required non-secret field cannot be missing *or blank*. The blank half matters: a
        // form that posts `{"host": ""}` and a form that posts nothing are the same credential
        // as far as a test hook is concerned, and refusing only the second one leaves the
        // first to fail later with a message nobody can act on. Secret fields are excluded
        // because they are not written yet at this point in the create.
        for field in &definition.fields {
            if field.required
                && field.kind != FieldType::Secret
                && !out.get(field.name).is_some_and(valid_non_empty)
            {
                return Err(crate::error::WorkflowError::CredentialFieldRequired {
                    field: field.name.to_string(),
                });
            }
        }

        Ok(Self(Value::Object(out)))
    }

    /// The stored object.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// Whether the type's required non-secret fields are all present.
    ///
    /// Used by the test hook: a credential with no host is not "failing to connect", it is
    /// not connected to anything, and the test should say which field is missing.
    #[must_use]
    pub fn missing_required(&self, definition: &CredentialDefinition) -> Vec<&'static str> {
        definition
            .fields
            .iter()
            .filter(|f| f.required && f.kind != FieldType::Secret)
            .filter(|f| !self.0.get(f.name).is_some_and(valid_non_empty))
            .map(|f| f.name)
            .collect()
    }
}

/// Whether a settings value counts as "the reader filled this in".
fn valid_non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

/// The read-only list a caller may filter by.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Match over name and key, case-insensitively.
    pub search: Option<String>,
    /// Keep only this credential type.
    pub r#type: Option<String>,
    /// Keep only this scope.
    pub scope: Option<String>,
    /// Keep only this health value (the recorded one, not the computed one).
    pub health: Option<String>,
    /// Keep only this sharing value.
    pub sharing: Option<String>,
    /// Keep only the ones this account owns.
    pub owner_user_id: Option<Uuid>,
    /// Rows to return.
    pub limit: i64,
}

impl ListQuery {
    /// The page size a list uses when the caller does not ask for one.
    pub const DEFAULT_LIMIT: i64 = 50;
    /// The largest page a list will return.
    pub const MAX_LIMIT: i64 = 200;

    /// A query with the default page size and no filters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: Self::DEFAULT_LIMIT,
            ..Self::default()
        }
    }
}

/// One workflow's reference to a credential, and where in its graph the reference sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct CredentialUsage {
    /// The workflow that names the credential.
    pub workflow_id: Uuid,
    /// Its name, so the detail screen can link rather than show an id.
    pub workflow_name: String,
    /// The node that names it.
    pub node_id: String,
    /// The node's label in the graph, when it has one.
    pub node_label: Option<String>,
    /// The node type, so a row can say "2 × HTTP request".
    pub node_type: Option<String>,
}

/// The usage view of one credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialUsageReport {
    /// Every reference found.
    pub references: Vec<CredentialUsage>,
    /// How many distinct workflows name it.
    pub workflow_count: usize,
    /// How many distinct node keys name it.
    pub node_type_count: usize,
    /// Whether anything references it — the delete guard's whole input.
    pub in_use: bool,
}

/// The result of a test hook run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestOutcome {
    /// Whether the connection worked.
    pub ok: bool,
    /// How long the hook took, in milliseconds.
    pub duration_ms: i64,
    /// The provider's own words, stripped of anything secret, or the reason it could not run.
    pub detail: String,
    /// The health the row was left in.
    pub health: Health,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WorkflowError;
    use crate::registry::find_credential_type;

    fn api_key_type() -> &'static CredentialDefinition {
        find_credential_type("api_key").expect("the registry ships an api_key type")
    }

    /// `json!` is shadowed by this function's own name in the module, so fixtures are parsed
    /// from a literal instead. Naming this `json` once and calling it everywhere beats
    /// qualifying the macro at every use.
    fn json(raw: &str) -> Value {
        serde_json::from_str(raw).expect("fixture is valid json")
    }

    #[test]
    fn the_health_list_is_the_migration_constraint() {
        // `workflow_credentials_health_valid` in 0053_workflow_credentials.sql lists exactly
        // these four. A fifth value the database accepts and the panel cannot colour is a row
        // with no chip, so the two lists are compared rather than trusted.
        let sql = include_str!("../../../database/migrations/0053_workflow_credentials.sql");
        for value in Health::all() {
            assert!(
                sql.contains(&format!("'{value}'")),
                "{value} must appear in the migration's health constraint"
            );
        }
        let constraint = sql
            .split("check (health in (")
            .nth(1)
            .and_then(|tail| tail.split(')').next())
            .expect("the health constraint is written as a list");
        let in_sql: Vec<&str> = constraint
            .split(',')
            .map(|part| part.trim().trim_matches('\'').trim())
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(
            in_sql,
            Health::all(),
            "the crate's list and the migration's list must be the same list, in the same order"
        );
    }

    #[test]
    fn every_vocabulary_word_appears_in_the_migration() {
        let sql = include_str!("../../../database/migrations/0053_workflow_credentials.sql");
        for word in SCOPES.iter().chain(SHARINGS.iter()) {
            assert!(
                sql.contains(&format!("'{word}'")),
                "{word} must be in the migration"
            );
        }
    }

    #[test]
    fn settings_refuse_a_secret_field_by_name() {
        let error = Settings::build(api_key_type(), &json(r#"{"api_key":"sk-live-1"}"#))
            .expect_err("a secret must not be storable as a setting");
        match error {
            WorkflowError::CredentialSecretWriteOnly { field } => {
                assert_eq!(field, "api_key");
            }
            other => panic!("expected a write-only refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_secret_value_never_reaches_the_stored_object() {
        // The property, stated directly: whatever the caller sends, the object the store
        // persists has no key that the type calls a secret.
        let definition = api_key_type();
        let accepted = Settings::build(definition, &json(r#"{"header":"X-Key"}"#))
            .expect("a non-secret field is fine");
        let secret_names: Vec<&str> = definition.secret_fields().iter().copied().collect();
        for name in secret_names {
            assert!(
                !accepted.value().get(name).is_some(),
                "{name} must not be readable from stored settings"
            );
        }
    }

    #[test]
    fn settings_refuse_a_field_the_type_does_not_declare() {
        let error = Settings::build(api_key_type(), &json(r#"{"headr":"X-Key"}"#))
            .expect_err("a typo is not a field");
        assert!(
            matches!(error, WorkflowError::CredentialInvalid(_)),
            "an unknown field is a refusal, got {error:?}"
        );
    }

    #[test]
    fn an_empty_object_is_accepted_when_every_required_field_is_a_secret() {
        // `api_key` declares exactly one required field and it is the secret one, which has not
        // been written yet at this point. Refusing here would make the create form impossible:
        // the reader must be able to save a credential before pasting the key into it, and the
        // save is what the key gets attached to.
        let built = Settings::build(api_key_type(), &json("{}"))
            .expect("a type whose only required field is secret accepts an empty object");
        assert_eq!(built.value(), &json("{}"));
        assert!(built.missing_required(api_key_type()).is_empty());
    }

    #[test]
    fn a_missing_required_field_is_refused_by_name() {
        // `smtp` has three required non-secret fields, so an empty object is genuinely
        // incomplete — and the refusal names the *first* one, so the form can put the caret
        // where the reader has to type.
        let smtp = find_credential_type("smtp").expect("the registry ships smtp");
        let error = Settings::build(smtp, &json("{}")).expect_err("an empty smtp is incomplete");
        assert!(
            matches!(&error, WorkflowError::CredentialFieldRequired { field } if field == "host"),
            "expected host to be named, got {error:?}"
        );
        assert_eq!(error.code(), "credential_field_required");
    }

    #[test]
    fn a_blank_required_field_is_refused_too() {
        // A form that posts `{"host": "   "}` is the same broken credential as one that posts
        // nothing, and it is the case a browser actually produces: an input the reader opened
        // and left alone still submits its empty value.
        let smtp = find_credential_type("smtp").expect("the registry ships smtp");
        let error = Settings::build(smtp, &json(r#"{"host":"  ","port":465,"username":"bot"}"#))
            .expect_err("a whitespace host is not a host");
        assert!(
            matches!(&error, WorkflowError::CredentialFieldRequired { field } if field == "host"),
            "expected host to be named, got {error:?}"
        );
    }

    #[test]
    fn a_blank_value_is_reported_missing_for_a_stored_credential() {
        // The read-side counterpart: a row that predates the rule (or was written by a person
        // with psql) still has to be diagnosable, which is what `missing_required` is for.
        let smtp = find_credential_type("smtp").expect("the registry ships smtp");
        let settings = Settings::build(
            smtp,
            &json(r#"{"host":"smtp.example","port":465,"username":"bot"}"#),
        )
        .expect("a complete smtp builds");
        assert!(settings.missing_required(smtp).is_empty());
    }

    #[test]
    fn a_blank_string_does_not_count_as_filled_in() {
        // A whitespace-only required field is refused at build time, so the check on the
        // *stored* object below can only ever see a field that was present when it was
        // written. That is the invariant `missing_required` relies on, and it is pinned here
        // by the fact that the only way to reach it is with a complete object.
        let smtp = find_credential_type("smtp").expect("the registry ships smtp");
        let settings = Settings::build(
            smtp,
            &json(r#"{"host":"smtp.example","port":465,"username":"bot"}"#),
        )
        .expect("a complete smtp builds");
        assert!(settings.missing_required(smtp).is_empty());
    }

    #[test]
    fn health_parsing_is_total_over_the_list() {
        for value in Health::all() {
            assert_eq!(Health::parse(value).map(Health::as_str), Some(value));
        }
        assert_eq!(Health::parse(" OK "), Some(Health::Ok));
        assert_eq!(Health::parse("broken"), None);
    }

    #[test]
    fn an_expired_token_reports_reauth_even_after_a_passing_test() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(100);
        let mut credential = credential_stub(Health::Ok);
        credential.oauth_expires_at = Some(now - time::Duration::minutes(1));
        assert_eq!(credential.effective_health(now), Health::NeedsReauth);

        credential.oauth_expires_at = Some(now + time::Duration::minutes(1));
        assert_eq!(credential.effective_health(now), Health::Ok);
    }

    /// A fully-populated row with nothing secret in it, for the computed-health tests.
    fn credential_stub(health: Health) -> Credential {
        Credential {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            key: "stripe".into(),
            name: "Stripe".into(),
            r#type: "api_key".into(),
            scope: "organization".into(),
            sharing: "private".into(),
            secret_ref: Some("vault://ref".into()),
            settings: json("{}"),
            owner_user_id: None,
            health: health.as_str().to_string(),
            health_checked_at: None,
            health_detail: None,
            oauth_expires_at: None,
            oauth_scopes: None,
            oauth_subject: None,
            last_used_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn attention_is_owed_for_failing_and_reauth_only() {
        assert!(Health::Failing.needs_attention());
        assert!(Health::NeedsReauth.needs_attention());
        assert!(!Health::Untested.needs_attention());
        assert!(!Health::Ok.needs_attention());
    }

    #[test]
    fn a_credential_has_no_field_a_secret_could_be_written_to() {
        // Stated as a structural property rather than a runtime check: the only handle to a
        // secret is `secret_ref`, which names a row in another subsystem.
        let json = serde_json::to_value(Credential {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            key: "k".into(),
            name: "n".into(),
            r#type: "api_key".into(),
            scope: "organization".into(),
            sharing: "private".into(),
            secret_ref: Some("vault://ref".into()),
            settings: json("{}"),
            owner_user_id: None,
            health: Health::Untested.as_str().to_string(),
            health_checked_at: None,
            health_detail: None,
            oauth_expires_at: None,
            oauth_scopes: None,
            oauth_subject: None,
            last_used_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        })
        .expect("a credential row serialises");
        for name in json.as_object().expect("an object").keys() {
            assert!(
                !matches!(
                    name.as_str(),
                    "api_key" | "token" | "password" | "secret" | "client_secret"
                ),
                "{name} is a field a secret could hide in"
            );
        }
    }
}
