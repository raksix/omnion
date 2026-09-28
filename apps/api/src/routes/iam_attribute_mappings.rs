//! `/api/v1/iam/providers/{id}/attribute-mappings` — the wizard's third step (REQ-065, slice 2).
//!
//! Read the map, replace it atomically, and preview it against a sample payload. Three endpoints,
//! three verbs' worth of meaning:
//!
//! * `GET` returns the rows plus the **catalogue** — the eight panel fields and the six transforms
//!   with their argument rules. A picker with hard-coded options is a picker that goes stale the
//!   day a field is added, and this file does not have to be edited to keep it honest.
//! * `PUT` replaces the whole map in one transaction. It is a PUT and not a POST-a-row because a
//!   partially applied map is the failure mode that matters: a provider with an email row missing
//!   refuses real sign-ins for a reason nobody can see.
//! * `POST …/preview` runs the *sign-in function itself* against a pasted payload. That is the
//!   only preview worth having — a second implementation of the mapping rules is a preview of
//!   nothing, and it is guaranteed to agree with the callback right up until the day it does not.
//!
//! The preview never writes, and never echoes a stored secret: a sample payload is operator-supplied
//! text and the answer is field values.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_identity::sso::attributes::{self, AttributeMap, TargetField, Transform};
use omnion_identity::sso::mappings;
use omnion_identity::sso::providers;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::state::AppState;

/// Largest sample payload a preview accepts. Generous for a claims document, small enough that the
/// request body limit is not the thing that refuses a paste.
const MAX_SAMPLE_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The map, and what the editor needs to render itself.
#[derive(Debug, Serialize)]
pub struct AttributeMapBody {
    /// The provider the map belongs to.
    pub provider_id: Uuid,
    /// The rows, in editor order.
    pub mappings: Vec<MappingBody>,
    /// The eight panel fields a row may target.
    pub target_fields: Vec<&'static str>,
    /// The six transforms a row may use.
    pub transforms: Vec<TransformBody>,
    /// Problems with the current map, so a screen that renders before it is saved can still show
    /// what is wrong rather than waiting for a 422.
    pub problems: Vec<attributes::MapProblem>,
}

/// One row, as the editor reads it.
#[derive(Debug, Serialize)]
pub struct MappingBody {
    /// The provider's own attribute name.
    pub source_attr: String,
    /// The panel field this fills.
    pub target_field: &'static str,
    /// The transform's name.
    pub transform: &'static str,
    /// Whether it needs an argument — so the editor does not guess.
    pub needs_argument: bool,
    /// The argument, when there is one.
    pub transform_arg: Option<String>,
    /// Whether sign-in is refused when this comes back empty.
    pub required: bool,
    /// Editor order.
    pub position: i32,
}

/// One transform, as the editor's picker reads it.
#[derive(Debug, Serialize)]
pub struct TransformBody {
    /// Wire name.
    pub name: &'static str,
    /// Whether it needs an argument to do anything.
    pub needs_argument: bool,
    /// One sentence for the tooltip.
    pub hint: &'static str,
}

/// The body of a preview request.
#[derive(Debug, serde::Deserialize)]
pub struct PreviewBody {
    /// The claims document, an LDAP entry or a SAML assertion summary. A JSON object is what every
    /// kind reduces to, so one shape serves all five.
    pub sample: Value,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The provider's map plus the catalogue the editor needs.
pub async fn get_attribute_mappings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<AttributeMapBody>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let map = mappings::load_map(state.db().pool(), id)
        .await
        .map_err(internal)?;
    Ok(Json(map_body(&provider, &map)))
}

/// Replace the whole map.
pub async fn replace_attribute_mappings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<Value>,
) -> Result<Json<AttributeMapBody>, ApiError> {
    let provider = load(&state, &current, id).await?;

    // Parse first, validate second, write third — and refuse the whole map on any problem. The
    // 400 carries every problem, because a wizard that reports one error per submit is a wizard
    // somebody abandons halfway.
    let map = AttributeMap::from_value(&body).map_err(bad_request)?;
    let problems = map.validate();
    if !problems.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "attribute_map_invalid",
            problems
                .iter()
                .map(|problem| problem.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let before = mappings::load_map(state.db().pool(), id)
        .await
        .map_err(internal)?;
    let stored = mappings::replace_map(state.db().pool(), id, map)
        .await
        .map_err(bad_request)?;
    let after = mappings::load_map(state.db().pool(), id)
        .await
        .map_err(internal)?;

    // The audit entry carries the *shape* of the change, not the rows: which fields are mapped now
    // that were not, and the other way round. A diff of full rows is a diff of a person's email
    // address, department and employee number in the audit log, which is exactly the kind of
    // personal data an audit trail should not be collecting as a side effect of a mapping change.
    let before_fields = field_list(&before);
    let after_fields = field_list(&after);
    let added: Vec<&str> = after_fields
        .iter()
        .filter(|field| !before_fields.contains(field))
        .copied()
        .collect();
    let removed: Vec<&str> = before_fields
        .iter()
        .filter(|field| !after_fields.contains(field))
        .copied()
        .collect();

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_attribute_mappings_replaced")
            .target("auth_provider", id.to_string())
            .metadata(json!({
                "slug": provider.slug,
                "kind": provider.kind.as_str(),
                "row_count": stored.len(),
                "fields_added": added,
                "fields_removed": removed,
            }))
            .ip_address(address.as_text())
            .organization(Some(provider.organization_id)),
    )
    .await?;

    Ok(Json(map_body(&provider, &after)))
}

/// Run the map against a sample payload.
///
/// The answer is the sign-in path's own projection: the field values it would produce, or the
/// fields it would refuse and name. Nothing is written and no account is created — this is a
/// rehearsal, and a rehearsal that creates an account is not one.
pub async fn preview_attribute_mappings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<Value>, ApiError> {
    load(&state, &current, id).await?;

    // A pasted sample is a document, not a query. A megabyte of pasted text is a mistake, and the
    // one thing this endpoint must never become is a way to push arbitrary JSON through the
    // projection loop.
    let size = serde_json::to_string(&body.sample)
        .map_err(|error| ApiError::bad_request("invalid_sample", error.to_string()))?
        .len();
    if size > MAX_SAMPLE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "sample_too_large",
            format!("the sample payload may be at most {MAX_SAMPLE_BYTES} bytes"),
        ));
    }
    if !body.sample.is_object() {
        return Err(ApiError::bad_request(
            "invalid_sample",
            "the sample must be a JSON object of attributes",
        ));
    }

    let map = mappings::load_map(state.db().pool(), id)
        .await
        .map_err(internal)?;
    let projection = map.project(&body.sample);

    // Every row is reported, not just the ones that produced a value: a mapping whose source the
    // sample does not carry is either a typo or a claim this provider only sometimes sends, and
    // the operator cannot tell which from "the field is empty".
    let rows: Vec<Value> = map
        .rows
        .iter()
        .map(|row| {
            json!({
                "source_attr": row.source_attr,
                "target_field": row.target_field.as_str(),
                "transform": row.transform.as_str(),
                "transform_arg": row.transform_arg,
                "raw": row.read(&body.sample),
                "value": row.apply(&body.sample),
                "required": row.required,
            })
        })
        .collect();

    Ok(Json(json!({
        "provider_id": id,
        "ok": projection.ok(),
        "values": projection
            .values
            .iter()
            .map(|(field, value)| json!({ "field": field.as_str(), "value": value }))
            .collect::<Vec<Value>>(),
        "missing": projection.missing_names(),
        "unused": projection.unused,
        "rows": rows,
        "problems": map.validate(),
    })))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Load a provider and prove the caller may see it.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<providers::AuthProvider, ApiError> {
    let provider = providers::find_provider(state.db().pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found())?;
    if provider.organization_id != current.organization_id {
        // A provider in another organization is reported as *absent* rather than forbidden: the
        // difference between "you may not see this" and "this does not exist" is an enumeration
        // oracle, and an id is not a secret.
        return Err(not_found());
    }
    Ok(provider)
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "provider_not_found",
        "no such provider",
    )
}

fn bad_request(error: omnion_identity::IdentityError) -> ApiError {
    ApiError::bad_request("attribute_map_invalid", error.to_string())
}

fn internal(error: omnion_identity::IdentityError) -> ApiError {
    // The message is deliberately generic: an `IdentityError` carrying a database message names
    // the table and sometimes the row. The detail belongs in the log, not in a response body.
    tracing::warn!(error = %error, "the attribute map could not be read");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "attribute_map_unreadable",
        "the attribute map could not be read",
    )
}

fn field_list(map: &AttributeMap) -> Vec<&'static str> {
    map.rows
        .iter()
        .map(|row| row.target_field.as_str())
        .collect()
}

/// Shape the answer: the rows, the catalogue the editor needs to render itself, and the problems
/// with what is stored right now.
fn map_body(provider: &providers::AuthProvider, map: &AttributeMap) -> AttributeMapBody {
    AttributeMapBody {
        provider_id: provider.id,
        mappings: map
            .rows
            .iter()
            .map(|row| MappingBody {
                source_attr: row.source_attr.clone(),
                target_field: row.target_field.as_str(),
                transform: row.transform.as_str(),
                needs_argument: row.transform.argument_required(),
                transform_arg: row.transform_arg.clone(),
                required: row.required,
                position: row.position,
            })
            .collect(),
        target_fields: TargetField::names(),
        transforms: Transform::ALL
            .iter()
            .map(|item| TransformBody {
                name: item.as_str(),
                needs_argument: item.argument_required(),
                hint: transform_hint(*item),
            })
            .collect(),
        problems: map.validate(),
    }
}

const fn transform_hint(transform: Transform) -> &'static str {
    match transform {
        Transform::None => "store the value exactly as it arrived",
        Transform::Trim => "remove surrounding whitespace",
        Transform::Lowercase => "lowercase the value",
        Transform::Prefix => "put the argument in front of the value",
        Transform::Static => "store the argument, whatever the source was",
        Transform::Split => "split on the argument (a comma when empty) and keep the first part",
    }
}
