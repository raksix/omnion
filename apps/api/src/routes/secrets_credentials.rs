//! `/api/v1/secrets/credentials` and `/api/v1/credential-slots`
//! (docs/requests/REQ-125, slice 2).
//!
//! Two surfaces with one idea behind them: **a consumer resolves a slot, never a secret id.**
//!
//! * `GET /secrets/credentials` — the typed profiles: kind, the non-secret fields, the
//!   validation chip, when it was last checked and when it is due again. **No value, ever** — the
//!   serializer has no field that could hold one, and the fields a wizard can write are checked
//!   for value-shaped content on the way in (see `omnion_secrets::sanitize_fields`).
//! * `POST /secrets/{id}/validate` — run the kind validator now. A failure is *recorded*, not
//!   raised: the request answers `200` with the chip and the provider's own sentence, because a
//!   provider being unreachable for an hour must not read as "your credential was not saved".
//! * `GET /credential-slots` / `PUT /credential-slots/{scope}/{slot}` — the assignment matrix and
//!   the editor. A self-referencing fallback is a `409` with the crate's sentence rather than a
//!   constraint violation; a removal names the consumer that was resolving the slot, because
//!   `resolve_slot` recorded it.
//!
//! Every write lands an audit row and, where the request names one, an event
//! (`credential.validation.failed` / `.recovered`, `credential_slot.assigned`). Both carry names,
//! ids and counts only.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_secrets::credentials::{self, CredentialRow, SlotRow};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

use super::secrets::map_error;

/// One typed credential, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct CredentialView {
    /// The secret id — an address for a lease, never a value.
    pub id: Uuid,
    /// The secret's name.
    pub name: String,
    /// One of the five kinds.
    pub kind: String,
    /// The kind's one-line purpose, so the panel never has to keep a second copy of the wording.
    pub kind_description: String,
    /// `true` when the kind can be proven without a network call.
    pub offline_checkable: bool,
    /// The non-secret fields, exactly as recorded.
    pub fields: Value,
    /// The fields as the detail list renders them (blank fields dropped, names humanised).
    pub field_pairs: Vec<(String, String)>,
    /// `unknown`, `valid`, `invalid` or `stale`.
    pub validation_state: String,
    /// The validator's sentence, already redacted.
    pub validation_message: String,
    /// When it was last checked.
    #[serde(with = "time::serde::rfc3339::option")]
    pub validation_checked_at: Option<time::OffsetDateTime>,
    /// How often it re-runs; 0 means on demand only.
    pub validation_interval_days: i32,
    /// When it is next due.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_validation_at: Option<time::OffsetDateTime>,
    /// `local`, `file` or `env`.
    pub provider: String,
    /// For a read-only bridge, the path or variable it points at.
    pub provider_locator: Option<String>,
    /// `true` for a `file`/`env` credential, which can never be written through the API.
    pub read_only: bool,
    /// The current version of the secret. Metadata, not content.
    pub version: i32,
    /// When the secret was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// The slots this credential is assigned to, filled by the list route.
    pub slots: Vec<String>,
}

impl CredentialView {
    /// Build from a row. `slots` is the list the list route joins in; a detail read passes an
    /// empty vector and the panel shows none.
    fn from_row(row: CredentialRow, slots: Vec<String>) -> Self {
        let kind = row.kind();
        // Computed up front: a struct literal moves `row.kind` and `row.validation_state` out of
        // the row, so everything that still borrows it has to be evaluated before that.
        let field_pairs = row.field_pairs();
        let validation_message = row.message();
        Self {
            id: row.secret_id,
            name: row.name,
            // A read-only bridge is not "a kind the platform could not parse" — it is a pointer
            // at a credential someone else owns. The panel has to say *that*, because the row's
            // whole reason for existing is the sentence under it.
            kind_description: if row.read_only {
                format!(
                    "Managed outside the platform: the {} provider resolves this on the machine \
                     that runs the workload, and Omnion stores only the pointer.",
                    row.provider
                )
            } else if let Some(kind) = kind {
                kind.description().to_owned()
            } else {
                "This credential was created by a newer version of the platform.".to_owned()
            },
            // A bridge has nothing to validate offline and no validator to run: the value never
            // reaches this process, so the button must not offer a check that cannot exist.
            offline_checkable: !row.read_only && kind.is_some_and(|kind| kind.is_offline()),
            kind: row.kind,
            field_pairs,
            fields: row.fields,
            validation_state: row.validation_state,
            validation_message,
            validation_checked_at: row.validation_checked_at,
            validation_interval_days: row.validation_interval_days,
            next_validation_at: None,
            provider: row.provider,
            provider_locator: row.provider_locator,
            read_only: row.read_only,
            version: row.version,
            created_at: row.created_at,
            slots,
        }
    }
}

/// The whole credentials screen in one read.
#[derive(Debug, Serialize)]
pub struct CredentialsResponse {
    /// The profiles.
    pub credentials: Vec<CredentialView>,
    /// The five kinds, with their descriptions and the fields the wizard asks for. The panel
    /// builds its picker from this so a new kind needs no second edit in the frontend.
    pub kinds: Vec<KindView>,
    /// Counters for the header strip.
    pub total: i32,
    pub valid: i32,
    pub invalid: i32,
    pub unknown: i32,
}

/// One kind, as the create wizard offers it.
#[derive(Debug, Serialize)]
pub struct KindView {
    /// The stored spelling.
    pub kind: &'static str,
    /// One line on what it is.
    pub description: &'static str,
    /// The non-secret fields the wizard asks for.
    pub fields: &'static [&'static str],
    /// `true` when the check needs no network call.
    pub offline: bool,
}

/// The validator's verdict on one credential, after a run.
#[derive(Debug, Serialize)]
pub struct ValidationResponse {
    /// The credential's id.
    pub id: Uuid,
    /// `valid` or `invalid` — never a third state, because a run always concludes.
    pub validation_state: String,
    /// The sentence the chip carries.
    pub validation_message: String,
    /// When the check ran.
    #[serde(with = "time::serde::rfc3339")]
    pub checked_at: time::OffsetDateTime,
    /// `true` when the credential is usable.
    pub valid: bool,
}

/// One slot assignment, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct SlotView {
    /// Row id.
    pub id: Uuid,
    /// `environment`, `site`, `module` or `organization`.
    pub scope_type: String,
    /// The concrete scope.
    pub scope_id: String,
    /// The slot name.
    pub slot: String,
    /// The slot's one-line purpose, from the seeded catalogue.
    pub description: String,
    /// The consumers that resolve this slot — the sentence a removal quotes back.
    pub consumers: String,
    /// The primary secret id.
    pub primary_secret_id: Option<Uuid>,
    /// The primary's name.
    pub primary_name: Option<String>,
    /// The primary's version.
    pub primary_version: Option<i32>,
    /// The fallback secret id.
    pub fallback_secret_id: Option<Uuid>,
    /// The fallback's name.
    pub fallback_name: Option<String>,
    /// The fallback's version.
    pub fallback_version: Option<i32>,
    /// The last consumer that resolved here — `None` means nobody has yet.
    pub last_resolved_by: Option<String>,
    /// When it was last resolved.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_resolved_at: Option<time::OffsetDateTime>,
    /// What an operator sees when the slot is empty.
    pub empty_reason: String,
}

/// The slot screen in one read: the assignments, the catalogue and the unassigned names.
#[derive(Debug, Serialize)]
pub struct SlotsResponse {
    /// The assignments.
    pub slots: Vec<SlotView>,
    /// The six documented slots with their descriptions and consumers.
    pub catalog: Vec<SlotDef>,
    /// Every typed credential the editor can assign, for its pickers.
    pub assignable: Vec<AssignableCredential>,
    /// How many assignments exist.
    pub assigned: i32,
}

/// One catalogue row.
#[derive(Debug, Serialize)]
pub struct SlotDef {
    /// The slot name.
    pub slot: String,
    /// What it is for.
    pub description: String,
    /// Which consumers resolve it.
    pub consumers: String,
}

/// A credential a slot can be pointed at.
#[derive(Debug, Serialize)]
pub struct AssignableCredential {
    /// The secret id.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Its kind, so an operator can pick the right one without opening each.
    pub kind: String,
    /// `true` for a `file`/`env` bridge.
    pub read_only: bool,
    /// The provider it resolves through. A bridge's *source* is the only thing an operator can
    /// act on, so the picker has to name it — `external` alone would be a word, not an answer.
    pub provider: String,
}

/// The body of a slot assignment.
#[derive(Debug, serde::Deserialize)]
pub struct AssignSlotInput {
    /// The concrete scope: an environment name, a site id, a module name.
    pub scope_id: String,
    /// The primary secret, or `null` to clear the assignment.
    #[serde(default)]
    pub primary_secret_id: Option<Uuid>,
    /// The fallback secret.
    #[serde(default)]
    pub fallback_secret_id: Option<Uuid>,
}

/// `GET /api/v1/secrets/credentials` — the typed profiles.
pub async fn read_credentials(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<CredentialsResponse>, ApiError> {
    let organization_id = Some(resolve_organization(&session, None)?);
    let pool = state.db().pool();
    let rows = credentials::list_credentials(pool, organization_id)
        .await
        .map_err(map_credential_error)?;

    // The slots each credential is assigned to, in one read rather than one per row.
    let assignments: Vec<(Uuid, String, String)> =
        sqlx::query_as("select primary_secret_id, scope_type, slot from credential_slots where primary_secret_id is not null")
            .fetch_all(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?;
    let fallbacks: Vec<(Uuid, String, String)> =
        sqlx::query_as("select fallback_secret_id, scope_type, slot from credential_slots where fallback_secret_id is not null")
            .fetch_all(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?;

    let slot_of = |secret_id: Uuid| -> Vec<String> {
        let mut names: Vec<String> = assignments
            .iter()
            .chain(fallbacks.iter())
            .filter(|(id, _, _)| *id == secret_id)
            .map(|(_, scope_type, slot)| format!("{scope_type} · {slot}"))
            .collect();
        names.sort();
        names.dedup();
        names
    };

    let views: Vec<CredentialView> = rows
        .into_iter()
        .map(|row| {
            let slots = slot_of(row.secret_id);
            CredentialView::from_row(row, slots)
        })
        .collect();

    let count_of = |state_name: &str| {
        views
            .iter()
            .filter(|view| view.validation_state == state_name)
            .count() as i32
    };
    let kinds = omnion_secrets::validators::CredentialKind::all()
        .into_iter()
        .map(|kind| KindView {
            kind: kind.as_str(),
            description: kind.description(),
            fields: kind.expected_fields(),
            offline: kind.is_offline(),
        })
        .collect();

    Ok(Json(CredentialsResponse {
        total: views.len() as i32,
        valid: count_of("valid"),
        invalid: count_of("invalid"),
        unknown: views.len() as i32 - count_of("valid") - count_of("invalid"),
        credentials: views,
        kinds,
    }))
}

/// `GET /api/v1/secrets/credentials/{id}` — one profile with its detail list.
pub async fn read_credential(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<CredentialView>, ApiError> {
    let organization_id = resolve_organization(&session, None)?;
    let row = credentials::find_credential(state.db().pool(), id)
        .await
        .map_err(map_credential_error)?
        .ok_or_else(|| not_found("secret"))?;
    in_organization(&row.organization_id, Some(organization_id))?;
    Ok(Json(CredentialView::from_row(row, Vec::new())))
}

/// `POST /api/v1/secrets/{id}/credential` — pin a secret to a kind and its non-secret fields.
pub async fn attach_credential_profile(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
    Json(input): Json<AttachProfileInput>,
) -> Result<(StatusCode, Json<CredentialView>), ApiError> {
    let organization_id = resolve_organization(&session, None)?;
    // The *secret* is the tenant check, not a profile row: on create there is no profile yet, so
    // looking one up first would make every create a `404`.
    let owner = credentials::find_secret_owner(state.db().pool(), id)
        .await
        .map_err(map_credential_error)?;
    in_organization(&owner.organization_id, Some(organization_id))?;

    let kind =
        omnion_secrets::credentials::parse_kind(&input.kind).map_err(map_credential_error)?;
    let row = credentials::attach_profile(state.db().pool(), id, kind, &input.fields)
        .await
        .map_err(map_credential_error)?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.credential.typed")
            .target("secret", id.to_string())
            .metadata(json!({ "kind": kind.as_str(), "name": row.name }))
            .ip_address(address.as_text()),
    )
    .await;

    // A run that fails is an event, not an error: the operator's credential is saved, and this
    // is the record that says which provider refused it.
    if row.validation_state == "invalid" {
        emit(
            &state,
            NewEvent::new("secrets.credential_validation_failed")
                .actor(session.user.id)
                .payload(json!({
                    "secret_id": id,
                    "name": row.name,
                    "kind": kind.as_str(),
                    "message": row.message(),
                })),
        )
        .await;
    }

    Ok((
        StatusCode::CREATED,
        Json(CredentialView::from_row(row, Vec::new())),
    ))
}

/// The body of a typed profile.
#[derive(Debug, serde::Deserialize)]
pub struct AttachProfileInput {
    /// One of the five kinds.
    pub kind: String,
    /// The non-secret fields. Anything shaped like the value is refused with the field name.
    #[serde(default)]
    pub fields: Value,
}

/// `POST /api/v1/secrets/{id}/validate` — run the kind validator now.
pub async fn validate_credential(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<ValidationResponse>, ApiError> {
    let organization_id = resolve_organization(&session, None)?;
    let pool = state.db().pool();
    let before = credentials::find_credential(pool, id)
        .await
        .map_err(map_credential_error)?
        .ok_or_else(|| not_found("secret"))?;
    in_organization(&before.organization_id, Some(organization_id))?;

    // No value is passed to the validator: a shape cannot be proven offline, and letting the
    // value in here is how it would eventually reach a log line. The redaction argument exists
    // for the one case where a *provider* echoes it back inside a refusal sentence.
    let outcome = credentials::record_validation(pool, id, None)
        .await
        .map_err(map_credential_error)?;
    let after = credentials::find_credential(pool, id)
        .await
        .map_err(map_credential_error)?
        .ok_or_else(|| not_found("secret"))?;
    let checked_at = after
        .validation_checked_at
        .unwrap_or_else(time::OffsetDateTime::now_utc);

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.credential.validated")
            .target("secret", id.to_string())
            .metadata(json!({
                "kind": after.kind,
                "state": after.validation_state,
                "message": after.message(),
            }))
            .ip_address(address.as_text()),
    )
    .await;

    // The recovery event exists because "a credential that was failing works again" is the one
    // transition an operator wants on a phone, and a red chip with no way back is not enough.
    let was_invalid = before.validation_state == "invalid";
    if after.validation_state == "valid" && was_invalid {
        emit(
            &state,
            NewEvent::new("secrets.credential_validation_recovered")
                .actor(session.user.id)
                .payload(json!({ "secret_id": id, "name": after.name, "kind": after.kind })),
        )
        .await;
    }

    let validation_message = after.message();
    Ok(Json(ValidationResponse {
        id,
        valid: outcome.is_valid(),
        validation_state: after.validation_state,
        validation_message,
        checked_at,
    }))
}

/// `GET /api/v1/credential-slots` — the assignment matrix.
pub async fn read_slots(State(state): State<AppState>) -> Result<Json<SlotsResponse>, ApiError> {
    let pool = state.db().pool();
    let rows = credentials::list_slots(pool)
        .await
        .map_err(map_credential_error)?;
    let catalog = credentials::slot_catalog(pool)
        .await
        .map_err(map_credential_error)?;
    let profiles = credentials::list_credentials(pool, None)
        .await
        .map_err(map_credential_error)?;

    let described = |slot: &str| -> (String, String) {
        catalog
            .iter()
            .find(|(name, _, _)| name == slot)
            .map_or_else(
                || (String::new(), String::new()),
                |(_, description, consumers)| (description.clone(), consumers.clone()),
            )
    };

    let slots: Vec<SlotView> = rows
        .iter()
        .map(|row| {
            let (description, consumers) = described(&row.slot);
            slot_view(row, description, consumers)
        })
        .collect();

    Ok(Json(SlotsResponse {
        assigned: slots.len() as i32,
        slots,
        catalog: catalog
            .into_iter()
            .map(|(slot, description, consumers)| SlotDef {
                slot,
                description,
                consumers,
            })
            .collect(),
        assignable: profiles
            .into_iter()
            .map(|row| AssignableCredential {
                id: row.secret_id,
                name: row.name,
                kind: row.kind,
                read_only: row.read_only,
                provider: row.provider,
            })
            .collect(),
    }))
}

/// `PUT /api/v1/credential-slots/{scope}/{slot}` — assign primary and fallback.
pub async fn put_slot(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path((scope_type, slot)): Path<(String, String)>,
    Json(input): Json<AssignSlotInput>,
) -> Result<Json<SlotView>, ApiError> {
    let pool = state.db().pool();
    let before = credentials::find_slot(pool, &scope_type, &slot, &input.scope_id)
        .await
        .map_err(map_credential_error)?;

    let row = credentials::assign_slot(
        pool,
        &scope_type,
        &input.scope_id,
        &slot,
        input.primary_secret_id,
        input.fallback_secret_id,
    )
    .await
    .map_err(map_credential_error)?;

    // The name a removal has to quote: the consumer `resolve_slot` last recorded for this slot.
    let affected = row
        .last_resolved_by
        .clone()
        .or_else(|| before.as_ref().and_then(|row| row.last_resolved_by.clone()));

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "credential_slot.assigned")
            .target("credential_slot", row.id.to_string())
            .metadata(json!({
                "scope_type": row.scope_type,
                "scope_id": row.scope_id,
                "slot": row.slot,
                "primary": row.primary_name,
                "fallback": row.fallback_name,
                "last_consumer": affected,
            }))
            .ip_address(address.as_text()),
    )
    .await;
    emit(
        &state,
        NewEvent::new("secrets.credential_slot_assigned")
            .actor(session.user.id)
            .payload(json!({
                "scope_type": row.scope_type,
                "scope_id": row.scope_id,
                "slot": row.slot,
                "primary": row.primary_name,
                "fallback": row.fallback_name,
            })),
    )
    .await;

    let catalog = credentials::slot_catalog(pool)
        .await
        .map_err(map_credential_error)?;
    let (description, consumers) = catalog
        .iter()
        .find(|(name, _, _)| *name == row.slot)
        .map_or_else(
            || (String::new(), String::new()),
            |(_, description, consumers)| (description.clone(), consumers.clone()),
        );
    Ok(Json(slot_view(&row, description, consumers)))
}

/// `GET /api/v1/credential-slots/{scope}/{slot}/resolve` — what a consumer would get.
///
/// The route exists so the panel can *show* a resolution without pretending to be a consumer, and
/// so the fallback behaviour is observable before a deploy hits it. It answers metadata only.
pub async fn resolve_slot_route(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((scope_type, slot, scope_id)): Path<(String, String, String)>,
) -> Result<Json<ResolutionView>, ApiError> {
    let resolution = credentials::resolve_slot(
        state.db().pool(),
        &scope_type,
        &scope_id,
        &slot,
        &format!("preview:{}", session.user.id),
    )
    .await
    .map_err(map_credential_error)?;

    audit(
        &state,
        NewAuditEntry::by_user(session.user.id, "credential_slot.resolved")
            .target("credential_slot", slot.clone())
            .metadata(json!({
                "scope_type": scope_type,
                "scope_id": scope_id,
                "resolved": resolution.name,
                "fell_back": resolution.fell_back,
            })),
    )
    .await;

    Ok(Json(ResolutionView {
        scope_type,
        scope_id,
        slot,
        secret_id: resolution.secret_id,
        name: resolution.name,
        version: resolution.version,
        fell_back: resolution.fell_back,
        summary: if resolution.fell_back {
            "The primary could not be read, so the fallback answered.".to_owned()
        } else {
            "The primary answered.".to_owned()
        },
    }))
}

/// What a slot resolves to, in the panel's words.
#[derive(Debug, Serialize)]
pub struct ResolutionView {
    /// The scope the read was for.
    pub scope_type: String,
    /// The concrete scope.
    pub scope_id: String,
    /// The slot name.
    pub slot: String,
    /// The secret that would be read.
    pub secret_id: Uuid,
    /// Its name.
    pub name: String,
    /// Its version.
    pub version: i32,
    /// `true` when the fallback answered.
    pub fell_back: bool,
    /// One sentence saying which.
    pub summary: String,
}

/// Turn a row into the panel's view.
fn slot_view(row: &SlotRow, description: String, consumers: String) -> SlotView {
    SlotView {
        id: row.id,
        scope_type: row.scope_type.clone(),
        scope_id: row.scope_id.clone(),
        slot: row.slot.clone(),
        description,
        consumers,
        primary_secret_id: row.primary_secret_id,
        primary_name: row.primary_name.clone(),
        primary_version: row.primary_version,
        fallback_secret_id: row.fallback_secret_id,
        fallback_name: row.fallback_name.clone(),
        fallback_version: row.fallback_version,
        last_resolved_by: row.last_resolved_by.clone(),
        last_resolved_at: row.last_resolved_at,
        empty_reason: row.empty_reason().to_owned(),
    }
}

/// A not-found for one of the crate's nouns, with its own code.
fn not_found(what: &'static str) -> ApiError {
    let code = match what {
        "secret" => "secret_not_found",
        "slot" => "credential_slot_not_found",
        _ => "secrets_not_found",
    };
    ApiError::new(StatusCode::NOT_FOUND, code, format!("no such {what}"))
}

/// Map a crate failure onto the HTTP surface, keeping the two statuses the request names.
///
/// `409` is the one that matters here: "a slot cannot hold the same secret as primary and
/// fallback" is a conflict with the current state of the row, not a malformed request, and an
/// operator who is told `400` reads it as "my input was wrong" rather than "that assignment is
/// a no-op".
fn map_credential_error(error: omnion_secrets::SecretsError) -> ApiError {
    if let omnion_secrets::SecretsError::Invalid(message) = &error {
        if message.contains("fallback cannot be the primary") {
            return ApiError::new(
                StatusCode::CONFLICT,
                "credential_slot_self_reference",
                message.clone(),
            );
        }
    }
    map_error(error)
}

/// Refuse a secret that belongs to another organization.
fn in_organization(row: &Option<Uuid>, caller: Option<Uuid>) -> Result<(), ApiError> {
    match (row, caller) {
        // A `global` installation secret is readable by everyone; an organization secret is not.
        (None, _) => Ok(()),
        (Some(_), None) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_organization",
            "this secret belongs to an organization",
        )),
        (Some(owner), Some(caller)) if *owner == caller => Ok(()),
        (Some(_), Some(_)) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_organization",
            "this secret belongs to another organization",
        )),
    }
}

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Write an audit row, and never let an audit problem fail the caller's request.
async fn audit(state: &AppState, entry: NewAuditEntry) {
    if let Err(error) = omnion_audit::entries::record(state.db().pool(), entry).await {
        tracing::warn!(error = %error, "the audit row could not be written");
    }
}
