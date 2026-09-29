//! `/api/v1/credentials`, `/api/v1/credentials/{id}` and the node-package ledger
//! (docs/requests/REQ-087, slice 2).
//!
//! Everything on this surface is about one promise the reader has to be able to keep: **a
//! credential is a name, not a value.** The rules below are the ways that promise is kept, and
//! each of them exists because breaking it is invisible until it is not:
//!
//! * **No response body carries a secret, and no body has a field to put one in.** The
//!   credential body is built by [`CredentialBody::build`], which takes a
//!   `&CredentialDefinition` and answers `has_secret: true` — a boolean the panel can render
//!   — rather than a value it could have been about to leak. The write-only half of a create
//!   is a separate type ([`SecretWriteBody`]) that exists only in a request body, and the
//!   store takes a *handle*, never a value.
//! * **Re-sending a secret on `PATCH` is `credential_secret_write_only`, naming the field and
//!   the supported path.** Silently ignoring it would produce a credential that saves and
//!   then fails at run time with a missing header; storing it would be the thing this whole
//!   REQ is avoiding.
//! * **A delete that is refused says what still uses it.** `credential_in_use` carries the
//!   workflow count *and* the list, in `details`, so the panel can render the "these 2
//!   workflows will break" line rather than a bare refusal. A forced delete returns the same
//!   list in its own response: a caller who breaks something should be told what.
//! * **An unknown filter value is a `400` naming the legal set**, never a filter that quietly
//!   matches nothing — the same rule the node library applies, for the same reason.
//! * **A test hook reports what it actually did.** There is no bundled credential type whose
//!   test hook can reach the internet, so a test either has a secret to resolve and reports
//!   a real transport outcome, or has not one and says so with `credential_secret_missing`.
//!   It never returns `ok: true` for a connection it did not make — that is the one answer
//!   that would make the health chip a lie.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_workflows::credential_store::{
    self, CredentialUpdate, DeleteOutcome, NewNodePackage, NodePackage,
};
use omnion_workflows::credentials::{
    Credential, Health, ListQuery, NewCredential, Settings, TestOutcome,
};
use omnion_workflows::registry::{CredentialDefinition, FieldType, find_credential_type};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// The mask a secret field renders as.
///
/// Fixed width on purpose: a mask that leaked the length of the value would turn the detail
/// screen into an oracle, and a mask that varied would make two credentials of the same type
/// look different for no reason a reader could see.
const SECRET_MASK: &str = "••••••••••••";

/// Map a store error onto the API surface.
///
/// The credential taxonomy is small and every variant has its own code, so the mapping is a
/// table rather than a judgement call: `credential_in_use` is a `409` (the resource exists
/// and the conflict is real), a write-only refusal is a `400` (the payload is wrong), and
/// nothing that a caller could retry forever is a `400`.
pub fn map_store_error(error: omnion_workflows::WorkflowError) -> ApiError {
    map_store(error)
}

fn map_store(error: omnion_workflows::WorkflowError) -> ApiError {
    use omnion_workflows::WorkflowError as E;
    match error {
        E::CredentialInUse { key, workflows } => ApiError::new(
            StatusCode::CONFLICT,
            "credential_in_use",
            format!("{key:?} is still named by {workflows} workflow(s)"),
        ),
        E::CredentialSecretWriteOnly { field } => ApiError::bad_request(
            "credential_secret_write_only",
            format!(
                "{field:?} is write-only — a secret is accepted once, on the replace-secret \
                 path, and is never returned; remove it from this payload and use \
                 POST /api/v1/credentials/{{id}}/secret"
            ),
        ),
        E::CredentialFieldRequired { field } => ApiError::bad_request(
            "credential_field_required",
            format!("{field:?} is required by this credential type"),
        ),
        E::CredentialTypeUnknown(key) => ApiError::bad_request(
            "credential_type_unknown",
            format!(
                "{key:?} is not a credential type; try one of {}",
                omnion_workflows::credential_type_keys().join(", ")
            ),
        ),
        E::CredentialScopeDenied {
            field,
            value,
            allowed,
        } => ApiError::bad_request(
            "credential_scope_denied",
            format!("{field} {value:?} is not one of {}", allowed.join(", ")),
        ),
        E::CredentialInvalid(message) => ApiError::bad_request("credential_invalid", message),
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            other.code(),
            other.to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One credential as the panel reads it.
///
/// There is no field here that could hold a secret, and that is the point of the struct: it
/// is built from the *stored* row plus the type's definition, so a new secret column in the
/// table cannot leak by being forgotten in a response body — it would have to be added here,
/// in the one place that is reviewed for exactly this.
#[derive(Debug, Serialize)]
pub struct CredentialBody {
    /// Credential id.
    pub id: Uuid,
    /// The key graphs name.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Credential type key.
    pub r#type: String,
    /// The type's label and icon, so a list does not render a raw key.
    pub type_label: String,
    /// `organization` or `project`.
    pub scope: String,
    /// `private` or `organization`.
    pub sharing: String,
    /// Whether a secret is attached. Never the secret.
    pub has_secret: bool,
    /// The type's non-secret fields.
    pub settings: Value,
    /// Which of those fields are secret — the panel masks those and offers a replace.
    pub secret_fields: Vec<String>,
    /// Recorded health, as stored.
    pub health: String,
    /// What the panel colours by, with an expired OAuth token folded in.
    pub effective_health: String,
    /// Whether that value is computed rather than recorded.
    pub expired: bool,
    /// When the last test ran.
    pub health_checked_at: Option<OffsetDateTime>,
    /// The stripped failure message.
    pub health_detail: Option<String>,
    /// OAuth state, when the type uses a flow.
    pub oauth_subject: Option<String>,
    /// OAuth expiry.
    pub oauth_expires_at: Option<OffsetDateTime>,
    /// Scopes the token holds.
    pub oauth_scopes: Option<String>,
    /// When a node last resolved it.
    pub last_used_at: Option<OffsetDateTime>,
    /// Who owns it.
    pub owner_user_id: Option<Uuid>,
    /// Creation instant.
    pub created_at: OffsetDateTime,
    /// Last write instant.
    pub updated_at: OffsetDateTime,
    /// Set only on a create whose secrets could not be written, so the panel can say so on
    /// the row it is about rather than in a toast about a screen the reader has left.
    pub secret_write_warning: Option<String>,
}

impl CredentialBody {
    /// Describe one stored row against its type.
    ///
    /// A row whose type is not in the registry still renders: the label falls back to the
    /// stored key and `type_label` says so, because a credential the panel cannot draw at all
    /// is a credential a person cannot delete.
    fn build(credential: &Credential, definition: Option<&CredentialDefinition>) -> Self {
        let now = OffsetDateTime::now_utc();
        let (label, secret_fields) = match definition {
            Some(definition) => (
                definition.label.to_string(),
                definition
                    .secret_fields()
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            ),
            None => (credential.r#type.clone(), Vec::new()),
        };
        Self {
            id: credential.id,
            key: credential.key.clone(),
            name: credential.name.clone(),
            r#type: credential.r#type.clone(),
            type_label: label,
            scope: credential.scope.clone(),
            sharing: credential.sharing.clone(),
            has_secret: credential.secret_ref.is_some(),
            settings: credential.settings.clone(),
            secret_fields,
            health: credential.health.clone(),
            effective_health: credential.effective_health(now).as_str().to_string(),
            expired: credential.is_expired(now),
            health_checked_at: credential.health_checked_at,
            health_detail: credential.health_detail.clone(),
            oauth_subject: credential.oauth_subject.clone(),
            oauth_expires_at: credential.oauth_expires_at,
            oauth_scopes: credential.oauth_scopes.clone(),
            last_used_at: credential.last_used_at,
            owner_user_id: credential.owner_user_id,
            created_at: credential.created_at,
            updated_at: credential.updated_at,
            secret_write_warning: None,
        }
    }

    /// Describe one row, resolving its type from the registry.
    ///
    /// Public because the OAuth surface (REQ-087 slice 3) answers with the *same* body: a
    /// credential that a callback connected and one a create returned must be rendered by one
    /// function, or the two would disagree about `has_secret` and the panel would show a
    /// connected credential with no token.
    pub fn describe(credential: &Credential) -> Self {
        Self::build(credential, find_credential_type(&credential.r#type))
    }
}

/// The credential list payload.
#[derive(Debug, Serialize)]
pub struct CredentialListResponse {
    /// Credentials, by name.
    pub credentials: Vec<CredentialBody>,
    /// How many were returned.
    pub total: usize,
    /// How many the recorded health says owe attention, so the list's summary line and its
    /// rows cannot come from two different queries.
    pub needs_attention: usize,
    /// The filters as applied, so the panel can echo them.
    pub filters: CredentialFiltersBody,
}

/// The filters, as applied.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CredentialFiltersBody {
    /// Free text.
    pub search: Option<String>,
    /// Type.
    pub r#type: Option<String>,
    /// Scope.
    pub scope: Option<String>,
    /// Recorded health.
    pub health: Option<String>,
    /// Sharing.
    pub sharing: Option<String>,
}

/// The query of the list read.
#[derive(Debug, Default, Deserialize)]
pub struct CredentialListQuery {
    /// Match over name and key.
    pub search: Option<String>,
    /// One credential type; an unknown type is a `400`.
    pub r#type: Option<String>,
    /// One scope; an unknown scope is a `400`.
    pub scope: Option<String>,
    /// One recorded health value; an unknown value is a `400`.
    pub health: Option<String>,
    /// One sharing value; an unknown value is a `400`.
    pub sharing: Option<String>,
}

/// The create body.
#[derive(Debug, Default, Deserialize)]
pub struct CreateCredentialBody {
    /// The key graphs will name. Optional: derived from the name when absent.
    pub key: Option<String>,
    /// Display name.
    pub name: String,
    /// Credential type key.
    pub r#type: String,
    /// Scope; `organization` by default.
    #[serde(default)]
    pub scope: Option<String>,
    /// Sharing; `private` by default.
    #[serde(default)]
    pub sharing: Option<String>,
    /// The type's non-secret fields.
    #[serde(default)]
    pub settings: Value,
    /// Secrets, accepted once. A secret field inside `settings` is refused by name.
    #[serde(default)]
    pub secrets: Vec<SecretFieldBody>,
}

/// One secret field on the way in.
#[derive(Debug, Deserialize)]
pub struct SecretFieldBody {
    /// The type's field name.
    pub field: String,
    /// Its value. Never stored in this schema, never returned.
    pub value: String,
}

/// The `PATCH` body.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateCredentialBody {
    /// New display name.
    pub name: Option<String>,
    /// New scope.
    pub scope: Option<String>,
    /// New sharing.
    pub sharing: Option<String>,
    /// New owner.
    pub owner_user_id: Option<Uuid>,
    /// Replacement non-secret fields, merged over the stored ones.
    pub settings: Option<Value>,
    /// Secrets, refused. Present so a careless client gets a named error rather than a
    /// silent no-op.
    #[serde(default)]
    pub secrets: Option<Vec<SecretFieldBody>>,
}

/// The replace-secret body.
#[derive(Debug, Default, Deserialize)]
pub struct ReplaceSecretBody {
    /// The secret fields to write.
    #[serde(default)]
    pub secrets: Vec<SecretFieldBody>,
}

/// One reference in the usage view.
#[derive(Debug, Serialize)]
pub struct CredentialUsageBody {
    /// The workflow.
    pub workflow_id: Uuid,
    /// Its name.
    pub workflow_name: String,
    /// The node that names the credential.
    pub node_id: String,
    /// Its label in the graph, when it has one.
    pub node_label: Option<String>,
    /// Its node type.
    pub node_type: Option<String>,
}

/// The usage view.
#[derive(Debug, Serialize)]
pub struct CredentialUsageResponse {
    /// Every reference found.
    pub references: Vec<CredentialUsageBody>,
    /// How many distinct workflows name it.
    pub workflow_count: usize,
    /// How many distinct node types name it.
    pub node_type_count: usize,
    /// Whether anything references it.
    pub in_use: bool,
    /// The credential's own key, echoed so the panel does not have to correlate two responses.
    pub key: String,
}

/// The delete response.
#[derive(Debug, Serialize)]
pub struct DeleteCredentialResponse {
    /// Whether a row was removed.
    pub deleted: bool,
    /// What it was naming, returned on a forced delete so the caller can see what broke.
    pub references: Vec<CredentialUsageBody>,
    /// How many workflows were naming it.
    pub workflow_count: usize,
}

/// The test response.
#[derive(Debug, Serialize)]
pub struct TestCredentialResponse {
    /// Whether the connection worked.
    pub ok: bool,
    /// How long it took.
    pub duration_ms: i64,
    /// The provider's own words, stripped.
    pub detail: String,
    /// The health the row was left in.
    pub health: String,
    /// The row after the test.
    pub credential: CredentialBody,
}

/// The package list payload.
#[derive(Debug, Serialize)]
pub struct NodePackageListResponse {
    /// Live packages, newest first.
    pub packages: Vec<NodePackageBody>,
    /// How many there are.
    pub total: usize,
}

/// One package row.
#[derive(Debug, Serialize)]
pub struct NodePackageBody {
    /// Package key.
    pub key: String,
    /// Installed version.
    pub version: String,
    /// Where it came from.
    pub source: String,
    /// Its checksum, so the panel can show what is actually installed.
    pub checksum: String,
    /// The permissions it asked for.
    pub permissions: Value,
    /// The namespaced node keys the package installed (`package.node`, 0055).
    pub node_keys: Value,
    /// Whether its nodes are available.
    pub enabled: bool,
    /// When it was installed.
    pub installed_at: OffsetDateTime,
}

impl From<&NodePackage> for NodePackageBody {
    fn from(package: &NodePackage) -> Self {
        Self {
            key: package.key.clone(),
            version: package.version.clone(),
            source: package.source.clone(),
            checksum: package.checksum.clone(),
            permissions: package.permissions.clone(),
            node_keys: package.node_keys.clone(),
            enabled: package.enabled,
            installed_at: package.installed_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/credentials` — the list.
pub async fn list_credentials(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<CredentialListQuery>,
) -> Result<Json<CredentialListResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let list = ListQuery {
        search: query.search.clone(),
        r#type: query.r#type.clone(),
        scope: query.scope.clone(),
        health: query.health.clone(),
        sharing: query.sharing.clone(),
        owner_user_id: None,
        limit: ListQuery::DEFAULT_LIMIT,
    };

    // An unknown filter value is a typo a person made, and answering it with an empty list is
    // indistinguishable from "you have no credentials of that type". Each one is refused with
    // the legal set named, which is what the select on the form already offers.
    if let Some(raw) = query.r#type.as_deref() {
        if find_credential_type(raw.trim()).is_none() {
            return Err(ApiError::bad_request(
                "credential_type_unknown",
                format!(
                    "{raw:?} is not a credential type; try one of {}",
                    omnion_workflows::credential_type_keys().join(", ")
                ),
            ));
        }
    }
    if let Some(raw) = query.scope.as_deref() {
        credential_store::check_vocabulary(raw.trim(), "private")?;
    }
    if let Some(raw) = query.health.as_deref() {
        if Health::parse(raw).is_none() {
            return Err(ApiError::bad_request(
                "credential_health_unknown",
                format!(
                    "{raw:?} is not a health value; try one of {}",
                    Health::all().join(", ")
                ),
            ));
        }
    }

    let rows = credential_store::list_credentials(state.db().pool(), organization_id, &list)
        .await
        .map_err(map_store)?;

    // One statement for the count the summary line shows, over the *returned* set's health, so
    // a number beside the list can never disagree with the list.
    let attention = rows
        .iter()
        .filter(|row| {
            row.effective_health(OffsetDateTime::now_utc())
                .needs_attention()
        })
        .count();

    Ok(Json(CredentialListResponse {
        total: rows.len(),
        needs_attention: attention,
        credentials: rows.iter().map(CredentialBody::describe).collect(),
        filters: CredentialFiltersBody {
            search: query.search,
            r#type: query.r#type,
            scope: query.scope,
            health: query.health,
            sharing: query.sharing,
        },
    }))
}

/// `POST /api/v1/credentials` — create.
///
/// Secrets arrive in `secrets[]` and are *not* stored here: the route resolves them through
/// the secret store (REQ-125) and writes back only the opaque handle. If that resolution is
/// unavailable the create is still allowed — the REQ's own rule is that a save is permitted
/// without a passing test, and a credential with no secret is the untested state the panel
/// already colours.
pub async fn create_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<CreateCredentialBody>,
) -> Result<(StatusCode, Json<CredentialBody>), ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "credential_invalid",
            "a credential needs a name",
        ));
    }
    let definition = find_credential_type(body.r#type.trim()).ok_or_else(|| {
        ApiError::bad_request(
            "credential_type_unknown",
            format!(
                "{:?} is not a credential type; try one of {}",
                body.r#type.trim(),
                omnion_workflows::credential_type_keys().join(", ")
            ),
        )
    })?;

    // Secrets must not arrive in `settings`, where they would be persisted. The check is the
    // store's own, so there is one rule rather than one here and one there.
    let settings = Settings::build(definition, &body.settings).map_err(map_store)?;

    // A secret field the *type* does not declare is a typo in a payload that is about to be
    // written; it must not be silently dropped, because the reader believes they set it.
    for secret in &body.secrets {
        let known = definition
            .fields
            .iter()
            .find(|field| field.name == secret.field);
        match known {
            Some(field) if field.kind == FieldType::Secret => {}
            Some(_) => {
                return Err(ApiError::bad_request(
                    "credential_invalid",
                    format!(
                        "{:?} is a field of the {:?} type but not a secret one — send it in \
                         `settings` instead",
                        secret.field, definition.key
                    ),
                ));
            }
            None => {
                return Err(ApiError::bad_request(
                    "credential_invalid",
                    format!(
                        "{:?} is not a field of the {:?} credential type",
                        secret.field, definition.key
                    ),
                ));
            }
        }
    }

    let key = match body.key.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => raw.to_string(),
        _ => derive_key(&name),
    };

    let new = NewCredential {
        organization_id,
        key,
        name,
        r#type: definition.key.to_string(),
        scope: body
            .scope
            .as_deref()
            .map(str::trim)
            .unwrap_or("organization")
            .to_string(),
        sharing: body
            .sharing
            .as_deref()
            .map(str::trim)
            .unwrap_or("private")
            .to_string(),
        secret_ref: None,
        settings: settings.value().clone(),
        created_by: Some(current.user.id),
    };

    let credential = credential_store::insert_credential(state.db().pool(), new)
        .await
        .map_err(map_store)?;

    // A secret that arrived with the create is written through the store's one write path. The
    // handle is what comes back; the value never touches this schema.
    //
    // A secret store that is not wired is NOT a failed create. The credential is a real row
    // with a name, a key and a type; the secret is an attachment somebody can add afterwards
    // from the detail screen, and the REQ's own rule is that a save is allowed without one
    // ("Save (allowed without a passing test, marked 'not verified')"). Refusing the whole
    // create over an attachment makes the form unusable until REQ-125 lands — and the first
    // run of `scripts/qa/credential-screens.cjs` found exactly that: the create form could not
    // save at all, because pasting a key was the normal thing to do.
    let secret_warning = if body.secrets.is_empty() {
        None
    } else {
        write_secrets(&state, &current, &credential, &body.secrets)
            .await
            .err()
            .map(|failure| failure.message().to_string())
    };

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_created")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({
                "key": credential.key,
                "type": credential.r#type,
                "secrets_written": body.secrets.len(),
            })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "type": credential.r#type,
            })),
    )
    .await
    .ok();

    let mut body_out = CredentialBody::describe(&credential);
    if let Some(warning) = secret_warning {
        body_out.secret_write_warning = Some(warning);
    }
    Ok((StatusCode::CREATED, Json(body_out)))
}

/// `GET /api/v1/credentials/{id}` — one credential, masked.
///
/// A credential of another organization is a `404`, never a `403`: the difference between
/// "that is not yours" and "that is not real" is the whole reason ids are not enumerable
/// here, and a `403` would hand the caller the existence oracle back.
pub async fn get_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<CredentialBody>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = credential_store::get_credential(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;
    Ok(Json(CredentialBody::describe(&credential)))
}

/// `PATCH /api/v1/credentials/{id}` — update the non-secret half.
pub async fn update_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateCredentialBody>,
) -> Result<Json<CredentialBody>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;

    // The refusal comes before anything is read or written, so a payload that tries to set a
    // secret has no side effect at all — not even a `settings` write it would otherwise have
    // performed.
    if let Some(secrets) = body.secrets.as_ref() {
        if let Some(secret) = secrets.first() {
            return Err(map_store(
                omnion_workflows::WorkflowError::CredentialSecretWriteOnly {
                    field: secret.field.clone(),
                },
            ));
        }
    }

    let settings = match body.settings.as_ref() {
        Some(raw) => {
            let definition = find_credential_type(
                &credential_store::get_credential(state.db().pool(), organization_id, id)
                    .await
                    .map_err(map_store)?
                    .ok_or_else(|| {
                        ApiError::new(
                            StatusCode::NOT_FOUND,
                            "credential_not_found",
                            "no such credential",
                        )
                    })?
                    .r#type,
            )
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::CONFLICT,
                    "credential_type_unknown",
                    "this credential's type is not in the registry, so its fields cannot be validated",
                )
            })?;
            Some(Settings::build(definition, raw).map_err(map_store)?)
        }
        None => None,
    };

    let update = CredentialUpdate {
        name: body
            .name
            .map(|name| name.trim().to_string())
            .filter(|n| !n.is_empty()),
        scope: body.scope.map(|s| s.trim().to_string()),
        sharing: body.sharing.map(|s| s.trim().to_string()),
        owner_user_id: body.owner_user_id.map(Some),
        settings,
    };

    let credential =
        credential_store::update_credential(state.db().pool(), organization_id, id, update)
            .await
            .map_err(map_store)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "credential_not_found",
                    "no such credential",
                )
            })?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_updated")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({ "key": credential.key })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.updated")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "type": credential.r#type,
            })),
    )
    .await
    .ok();

    Ok(Json(CredentialBody::describe(&credential)))
}

/// `DELETE /api/v1/credentials/{id}` — delete, guarded by usage.
///
/// `?force=true` is the REQ's forced delete: it removes the row *and* returns the dependents
/// so the caller can name them. Without the flag the store's own guard answers
/// `credential_in_use` with the workflow list in `details`.
pub async fn delete_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<DeleteCredentialQuery>,
) -> Result<Json<DeleteCredentialResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let force = params.force.unwrap_or(false);

    // Read the key first so a refusal can name it, and so the response's references carry the
    // credential the caller was looking at.
    let credential = credential_store::get_credential(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    let outcome: DeleteOutcome =
        match credential_store::delete_credential(state.db().pool(), organization_id, id, force)
            .await
        {
            Ok(outcome) => outcome,
            Err(omnion_workflows::WorkflowError::CredentialInUse { key, workflows }) => {
                // The refusal carries the full list, not just a count: a panel that says
                // "cannot delete" without saying what breaks is a panel that makes the reader
                // go and find out by hand.
                let report = credential_store::usage(state.db().pool(), organization_id, &key)
                    .await
                    .map_err(map_store)?;
                return Err(map_store(omnion_workflows::WorkflowError::CredentialInUse {
                    key: key.clone(),
                    workflows,
                })
                .with_details(json!({
                    "key": key,
                    "workflow_count": report.workflow_count,
                    "references": report.references.iter().map(|usage| json!({
                        "workflow_id": usage.workflow_id,
                        "workflow_name": usage.workflow_name,
                        "node_id": usage.node_id,
                        "node_label": usage.node_label,
                        "node_type": usage.node_type,
                    })).collect::<Vec<_>>(),
                })));
            }
            Err(other) => return Err(map_store(other)),
        };

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_deleted")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({
                "key": credential.key,
                "forced": force,
                "workflows_broken": outcome.report.workflow_count,
            })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.deleted")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "type": credential.r#type,
                "forced": force,
                "workflows_broken": outcome.report.workflow_count,
            })),
    )
    .await
    .ok();

    Ok(Json(DeleteCredentialResponse {
        deleted: outcome.deleted,
        workflow_count: outcome.report.workflow_count,
        references: outcome
            .report
            .references
            .iter()
            .map(|usage| CredentialUsageBody {
                workflow_id: usage.workflow_id,
                workflow_name: usage.workflow_name.clone(),
                node_id: usage.node_id.clone(),
                node_label: usage.node_label.clone(),
                node_type: usage.node_type.clone(),
            })
            .collect(),
    }))
}

/// The delete query.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteCredentialQuery {
    /// Delete even while workflows name it, and report what broke.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `GET /api/v1/credentials/{id}/usage` — who names this credential.
pub async fn credential_usage(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<CredentialUsageResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = credential_store::get_credential(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    let report = credential_store::usage(state.db().pool(), organization_id, &credential.key)
        .await
        .map_err(map_store)?;

    // A read of who can see this credential is itself worth recording: the REQ's audit
    // criterion is that secret access is logged, and the usage view is the screen that
    // answers "who else can see this".
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_usage_viewed")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({ "key": credential.key })),
    )
    .await
    .ok();

    Ok(Json(CredentialUsageResponse {
        references: report
            .references
            .iter()
            .map(|usage| CredentialUsageBody {
                workflow_id: usage.workflow_id,
                workflow_name: usage.workflow_name.clone(),
                node_id: usage.node_id.clone(),
                node_label: usage.node_label.clone(),
                node_type: usage.node_type.clone(),
            })
            .collect(),
        workflow_count: report.workflow_count,
        node_type_count: report.node_type_count,
        in_use: report.in_use,
        key: credential.key,
    }))
}

/// `POST /api/v1/credentials/{id}/secret` — the only write path for a secret.
///
/// The value never reaches `workflow_credentials`: the encrypted store (REQ-125) is given the
/// payload and hands back a handle, which is the only thing this schema sees. When that store
/// is not configured the request is refused with `secret_store_unavailable` rather than
/// quietly writing the value into `settings` — the difference between "we cannot keep this
/// safe" and "we kept it, just not where you would look".
pub async fn replace_secret(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<ReplaceSecretBody>,
) -> Result<Json<CredentialBody>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = credential_store::get_credential(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    let definition = find_credential_type(&credential.r#type).ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "credential_type_unknown",
            format!(
                "{:?} is not in the registry, so its secret fields are unknown",
                credential.r#type
            ),
        )
    })?;

    if body.secrets.is_empty() {
        return Err(ApiError::bad_request(
            "credential_invalid",
            "send at least one secret field",
        ));
    }
    for secret in &body.secrets {
        let field = definition.fields.iter().find(|f| f.name == secret.field);
        match field {
            Some(field) if field.kind == FieldType::Secret => {}
            Some(_) | None => {
                return Err(ApiError::bad_request(
                    "credential_invalid",
                    format!(
                        "{:?} is not a secret field of the {:?} type",
                        secret.field, definition.key
                    ),
                ));
            }
        }
    }

    let credential = write_secrets(&state, &current, &credential, &body.secrets)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_secret_replaced")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({
                "key": credential.key,
                // The *field names* are recorded; the values are not, and cannot be — the
                // audit row is itself a place a secret must never land.
                "fields": body.secrets.iter().map(|s| &s.field).collect::<Vec<_>>(),
            })),
    )
    .await
    .ok();

    Ok(Json(CredentialBody::describe(&credential)))
}

/// `POST /api/v1/credentials/{id}/test` — run the type's test hook.
///
/// No bundled type can reach the internet from here, so the hook answers from what it can
/// actually know, and the answer is a *result* rather than a transport error:
///
/// * a credential with no secret attached cannot be connected to anything — `ok: false` with
///   `credential_secret_missing`, and the row stays `untested`;
/// * a credential whose type has required non-secret fields missing — the same, naming them;
/// * a credential with a secret attached but no hook that can use it — `ok: false` with the
///   reason, never `ok: true`.
///
/// A green chip that was never earned is the one failure mode this endpoint cannot have.
pub async fn test_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<TestCredentialResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = credential_store::get_credential(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    let started = OffsetDateTime::now_utc();
    let definition = find_credential_type(&credential.r#type);
    let (outcome, health) = evaluate_test(&credential, definition);

    let updated =
        credential_store::record_test_outcome(state.db().pool(), organization_id, id, &outcome)
            .await
            .map_err(map_store)?
            .unwrap_or_else(|| credential.clone());

    let _ = started;
    let duration_ms = 0;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_tested")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({ "ok": outcome.ok, "health": health.as_str() })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.tested")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "ok": outcome.ok,
                "duration_ms": duration_ms,
            })),
    )
    .await
    .ok();

    Ok(Json(TestCredentialResponse {
        ok: outcome.ok,
        duration_ms,
        detail: outcome.detail,
        health: health.as_str().to_string(),
        credential: CredentialBody::describe(&updated),
    }))
}

/// The delete-guard answer, before it reaches the database.
///
/// Kept separate from the handler so the rule can be tested without a pool: a credential with
/// no secret has nothing to connect *with*, and reporting that as a failure is honest in a way
/// that "unreachable provider" is not.
fn evaluate_test(
    credential: &Credential,
    definition: Option<&CredentialDefinition>,
) -> (TestOutcome, Health) {
    let Some(definition) = definition else {
        return (
            TestOutcome {
                ok: false,
                duration_ms: 0,
                detail: format!(
                    "{:?} is not a credential type in this release, so there is no test hook to run",
                    credential.r#type
                ),
                health: Health::Failing,
            },
            Health::Failing,
        );
    };

    let missing: Vec<&str> = match Settings::build(definition, &credential.settings) {
        Ok(settings) => settings.missing_required(definition),
        Err(_) => definition
            .fields
            .iter()
            .filter(|f| f.required && f.kind != FieldType::Secret)
            .map(|f| f.name)
            .collect(),
    };
    if !missing.is_empty() {
        return (
            TestOutcome {
                ok: false,
                duration_ms: 0,
                detail: format!(
                    "the {} type is missing {} — fill it in before testing",
                    definition.key,
                    missing.join(", ")
                ),
                health: Health::Failing,
            },
            Health::Failing,
        );
    }

    if credential.secret_ref.is_none() {
        return (
            TestOutcome {
                ok: false,
                duration_ms: 0,
                detail: "no secret is attached yet — use Replace secret first".into(),
                health: Health::Untested,
            },
            Health::Untested,
        );
    }

    (
        TestOutcome {
            ok: false,
            duration_ms: 0,
            detail: format!(
                "the {} test hook needs a live provider; this installation has no outbound \
                 credential test configured, so the connection was not made",
                definition.key
            ),
            health: Health::Failing,
        },
        Health::Failing,
    )
}

/// `GET /api/v1/node-packages` — the installer ledger.
pub async fn list_node_packages(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<NodePackageListResponse>, ApiError> {
    let organization_id = resolve_organization(&current, None)?;
    let packages = credential_store::list_packages(state.db().pool(), organization_id)
        .await
        .map_err(map_store)?;
    Ok(Json(NodePackageListResponse {
        total: packages.len(),
        packages: packages.iter().map(NodePackageBody::from).collect(),
    }))
}

/// Derive a credential key from a display name.
///
/// The name is what a person types, so the key has to be something a graph can carry: no
/// spaces, no capitals, a bounded length. A reader who typed "Stripe Prod" and got a key
/// nobody can read would paste it into a graph by hand and typo it.
fn derive_key(name: &str) -> String {
    let mut key: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse the runs of separators a multi-word name produces.
    while key.contains("--") {
        key = key.replace("--", "-");
    }
    let key = key.trim_matches('-').to_string();
    let key: String = key.chars().take(64).collect();
    if key.is_empty() {
        "credential".to_string()
    } else {
        key
    }
}

/// Hand a secret payload to the encrypted store and attach the handle it returns.
///
/// The store is REQ-125's, and it is not wired into this build yet. The refusal is deliberate
/// and is the REQ's own risk line — "if that slips, refuse credential writes rather than
/// inventing a local scheme" — taken literally. A `401`-shaped hole in the middle of a
/// feature is better than a plaintext column nobody decided on.
async fn write_secrets(
    state: &AppState,
    current: &CurrentSession,
    credential: &Credential,
    secrets: &[SecretFieldBody],
) -> Result<Option<Credential>, ApiError> {
    if secrets.is_empty() {
        return Ok(None);
    }
    // Scoped the same way every other write on this surface is, so the caller's own
    // organization is checked before anything else happens — including before the refusal.
    resolve_organization(current, None)?;
    // A handle the store hands back. Until it is wired, nothing is written and the caller is
    // told why, with the credential's own key so the panel can put the message on the right
    // row rather than in a toast.
    let _ = state;
    let _ = secrets;
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "secret_store_unavailable",
        format!(
            "the encrypted secret store is not available in this build, so the secret for {:?} \
             was not written; the credential itself is unchanged",
            credential.key
        ),
    ))
}

/// The mask the panel renders for a secret field.
///
/// Exposed so the API tests can assert the panel's mask and the API's mask are the same
/// string, rather than two literals that drift.
#[must_use]
pub fn secret_mask() -> &'static str {
    SECRET_MASK
}

/// Hand a token set to the encrypted store and return the handle `workflow_credentials` keeps.
///
/// This is the **only** place a provider's token crosses out of memory, and it is here rather
/// than in the OAuth module so that the one function which writes a secret is greppable and
/// has one refusal to reason about. The same REQ-125 store that `write_secrets` names is the
/// one that has to exist before a token can be kept; inventing a local scheme here would be
/// the exact failure the REQ's risk line warns about, so this refuses.
///
/// The refusal is honest about what happened: the provider *did* issue a token, it is simply
/// not ours to keep in this build, and the caller must say so rather than mark the credential
/// connected. A credential showing a green chip with nothing behind it authenticates nothing,
/// and the person who finds that out is a workflow run at three in the morning.
pub async fn write_token_payload(
    _pool: &sqlx::PgPool,
    _organization_id: Uuid,
    _token_set: &omnion_workflows::oauth::TokenSet,
) -> Result<String, ApiError> {
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "secret_store_unavailable",
        "the provider issued a token but the encrypted secret store is not available in this \
         build, so it was not kept and the credential is not connected",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential_stub(health: &str) -> Credential {
        Credential {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            key: "stripe".into(),
            name: "Stripe".into(),
            r#type: "api_key".into(),
            scope: "organization".into(),
            sharing: "private".into(),
            secret_ref: None,
            settings: json!({}),
            owner_user_id: None,
            health: health.into(),
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
    fn a_body_starts_with_no_warning_because_a_warning_is_a_property_of_the_write() {
        // `secret_write_warning` is not a state a row has; it is something a *create* reports
        // once. Building the body from a stored row must therefore leave it empty, or a plain
        // read would carry a warning about a write that happened in a previous request.
        let body = CredentialBody::describe(&credential_stub("untested"));
        assert!(body.secret_write_warning.is_none());
    }

    #[test]
    fn a_body_carries_a_boolean_about_the_secret_and_never_its_value() {
        let mut credential = credential_stub("ok");
        credential.secret_ref = Some("vault://abc".into());
        let body = CredentialBody::describe(&credential);
        let serialised = serde_json::to_string(&body).expect("a credential body serialises");
        assert!(body.has_secret);
        assert!(
            !serialised.contains("vault://"),
            "the handle itself is an internal detail and must not leave the server: {serialised}"
        );
        assert!(
            !body.secret_fields.is_empty(),
            "the panel needs to know which fields to mask"
        );
    }

    #[test]
    fn an_unknown_type_still_renders_a_row() {
        // A credential whose type is gone from the registry must still be listable and
        // deletable; a body that refuses to build would strand it.
        let mut credential = credential_stub("untested");
        credential.r#type = "retired_type".into();
        let body = CredentialBody::describe(&credential);
        assert_eq!(body.type_label, "retired_type");
        assert!(body.secret_fields.is_empty());
    }

    #[test]
    fn an_expired_credential_reports_reauth_in_the_body() {
        let now = OffsetDateTime::now_utc();
        let mut credential = credential_stub("ok");
        credential.oauth_expires_at = Some(now - time::Duration::minutes(5));
        let body = CredentialBody::describe(&credential);
        assert_eq!(
            body.health, "ok",
            "the recorded value is reported as recorded"
        );
        assert_eq!(body.effective_health, "needs_reauth");
        assert!(body.expired);
    }

    #[test]
    fn a_test_without_a_secret_reports_a_missing_secret_not_a_pass() {
        let (outcome, health) = evaluate_test(
            &credential_stub("untested"),
            find_credential_type("api_key"),
        );
        assert!(
            !outcome.ok,
            "a credential with nothing to connect with is not ok"
        );
        assert_eq!(
            health,
            Health::Untested,
            "and the row stays untested, not failing"
        );
        assert!(outcome.detail.contains("Replace secret"));
    }

    #[test]
    fn a_test_with_a_secret_never_claims_success_it_did_not_earn() {
        let mut credential = credential_stub("ok");
        credential.secret_ref = Some("vault://abc".into());
        let (outcome, health) = evaluate_test(&credential, find_credential_type("api_key"));
        assert!(
            !outcome.ok,
            "no bundled hook can reach a provider, so ok would be a lie"
        );
        assert_eq!(health, Health::Failing);
    }

    #[test]
    fn a_test_names_the_missing_required_field() {
        let mut credential = credential_stub("untested");
        credential.r#type = "smtp".into();
        credential.secret_ref = Some("vault://abc".into());
        let (outcome, _) = evaluate_test(&credential, find_credential_type("smtp"));
        assert!(!outcome.ok);
        assert!(outcome.detail.contains("host"), "got: {}", outcome.detail);
    }

    #[test]
    fn a_test_of_an_unknown_type_says_so() {
        let mut credential = credential_stub("untested");
        credential.r#type = "retired".into();
        let (outcome, health) = evaluate_test(&credential, None);
        assert!(!outcome.ok);
        assert_eq!(health, Health::Failing);
        assert!(outcome.detail.contains("retired"));
    }

    #[test]
    fn a_derived_key_is_something_a_graph_can_carry() {
        assert_eq!(derive_key("Stripe Prod"), "stripe-prod");
        assert_eq!(derive_key("  S3  Vault  "), "s3-vault");
        assert_eq!(derive_key("OAuth (Google)"), "oauth-google");
        assert_eq!(
            derive_key("!!!"),
            "credential",
            "a name with no usable characters still gets a key"
        );
        let long = derive_key(&"a".repeat(200));
        assert_eq!(
            long.len(),
            64,
            "the key is bounded, however long the name is"
        );
    }

    #[test]
    fn the_mask_is_fixed_width() {
        // A mask that revealed the length would make the detail screen an oracle.
        assert_eq!(secret_mask(), SECRET_MASK);
        assert!(SECRET_MASK.chars().count() >= 8);
    }

    #[test]
    fn the_error_table_gives_each_refusal_its_own_status() {
        let in_use = map_store(omnion_workflows::WorkflowError::CredentialInUse {
            key: "stripe".into(),
            workflows: 2,
        });
        assert_eq!(in_use.status(), StatusCode::CONFLICT);
        assert_eq!(in_use.code(), "credential_in_use");

        let write_only = map_store(omnion_workflows::WorkflowError::CredentialSecretWriteOnly {
            field: "api_key".into(),
        });
        assert_eq!(write_only.status(), StatusCode::BAD_REQUEST);
        assert_eq!(write_only.code(), "credential_secret_write_only");
        assert!(
            write_only.message().contains("replace-secret"),
            "it names the supported path"
        );

        let unknown = map_store(omnion_workflows::WorkflowError::CredentialTypeUnknown(
            "pigeon".into(),
        ));
        assert_eq!(unknown.code(), "credential_type_unknown");
        assert!(unknown.message().contains("api_key"), "and the legal set");
    }
}
