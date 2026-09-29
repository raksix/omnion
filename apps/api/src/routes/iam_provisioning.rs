//! `/api/v1/iam/provisioning` — SCIM provisioning tokens and the sync log (docs/07-IAM.md §19;
//! REQ-006, slice 4b).
//!
//! The token secret is shown exactly once, at minting: the store keeps only its SHA-256 hash, so
//! this module is the only place a client can ever read one. Revoking is idempotent — an already
//! revoked token answers the same "not live any more" statement instead of a 404, because a
//! client retrying a rotation must not learn anything it did not already know.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::provisioning;
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::{ensure_same_organization, resolve_organization};
use crate::state::AppState;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Query of the token list and the log.
#[derive(Debug, Deserialize)]
pub struct ProvisioningQuery {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Page size of the sync log.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// The body of a token minting.
#[derive(Debug, Deserialize)]
pub struct NewTokenRequest {
    /// Label the reader will see in the list.
    #[serde(default)]
    pub name: String,
    /// Organization to mint in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Lifetime in days; absent means the store's default rather than "no expiry".
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// One token as the panel reads it.
fn token_json(token: &provisioning::ProvisioningToken) -> Value {
    json!({
        "id": token.id,
        "organization_id": token.organization_id,
        "name": token.name,
        "prefix": token.prefix,
        "created_by": token.created_by,
        "last_used_at": token.last_used_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        "revoked_at": token.revoked_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        "created_at": token.created_at.format(&Rfc3339).unwrap_or_default(),
        "expires_at": token.expires_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        "rotated_at": token.rotated_at.map(|value| value.format(&Rfc3339).unwrap_or_default()),
        // The panel cannot compare timestamps itself, so the store's two columns are turned into
        // the sentence a reader acts on here. An expired token that still reads as "active" is the
        // one thing a security panel must never show.
        "expired": token
            .expires_at
            .is_some_and(|value| value <= time::OffsetDateTime::now_utc()),
        "rotated": token.rotated_at.is_some(),
    })
}

/// One sync-log line.
fn log_json(entry: &provisioning::SyncLogEntry) -> Value {
    json!({
        "id": entry.id,
        "organization_id": entry.organization_id,
        "direction": entry.direction,
        "resource": entry.resource,
        "external_id": entry.external_id,
        "entity_id": entry.entity_id,
        "action": entry.action,
        "outcome": entry.outcome,
        "detail": entry.detail,
        "created_at": entry.created_at.format(&Rfc3339).unwrap_or_default(),
    })
}

/// The organization's provisioning tokens (newest first, revoked ones included).
pub async fn list_tokens(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ProvisioningQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let tokens = provisioning::list_tokens(state.db().pool(), organization_id).await?;

    Ok(Json(json!({
        "organization_id": organization_id,
        "tokens": tokens.iter().map(token_json).collect::<Vec<_>>(),
    })))
}

/// Mint a token. The response carries the secret — the only time it exists in readable form.
pub async fn create_token(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewTokenRequest>,
) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let issued = provisioning::create_token_with_ttl(
        state.db().pool(),
        organization_id,
        &body.name,
        Some(current.user.id),
        body.expires_in_days,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provisioning.token_issued")
            .target("provisioning_token", issued.token.id.to_string())
            .metadata(json!({
                "prefix": issued.token.prefix,
                "name": issued.token.name,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "token": token_json(&issued.token),
            "secret": issued.secret,
        })),
    ))
}

/// Revoke a token. Revoking twice answers the same result (`revoked: false`).
pub async fn revoke_token(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(token_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    // The row is read first, so the tenancy check runs before anything is written and a token of
    // another organization is invisible rather than revocable.
    let token = sqlx::query_as::<_, provisioning::ProvisioningToken>(
        "select id, organization_id, name, prefix, created_by, last_used_at, revoked_at, \
         created_at from provisioning_tokens where id = $1",
    )
    .bind(token_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(omnion_identity::IdentityError::from)?
    .ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "token_not_found",
            "no such provisioning token",
        )
    })?;

    ensure_same_organization(&current, Some(token.organization_id))?;

    let revoked = provisioning::revoke_token(state.db().pool(), token.id).await?;

    if revoked.is_some() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "iam.provisioning.token_revoked")
                .target("provisioning_token", token.id.to_string())
                .metadata(json!({ "prefix": token.prefix }))
                .ip_address(address.as_text())
                .organization(Some(token.organization_id)),
        )
        .await?;
    }

    Ok(Json(json!({
        "revoked": revoked.is_some(),
        "token": revoked.as_ref().map(token_json),
    })))
}

/// Replace a live token with a new one and hand back the new secret (shown exactly once).
///
/// This is the operation the acceptance criterion means by "a rotated token refuses the old
/// value": the old secret is dead the moment this returns, and the response carries the
/// replacement's secret so the operator can paste it into the connector without minting a second
/// token and leaving an orphan.
pub async fn rotate_token(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(token_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    // Read the row first so the tenancy check runs before anything is written, exactly as in
    // `revoke_token`: a token of another organization must be invisible, not rotatable.
    //
    // The refusal is a `404` and not `cross_organization`, and that is the whole point of doing
    // the check here rather than in `ensure_same_organization`. A "this account may only work
    // inside its own organization" answer is a *confirmation that the id exists* — a stranger
    // holding a guessed uuid learns whether it names a real token, and a token is exactly the
    // kind of row whose existence is worth not confirming. `ensure_same_organization` is right for
    // a body field the caller supplied; it is wrong for a path id that was never theirs.
    let before = match sqlx::query_as::<_, provisioning::ProvisioningToken>(
        "select id, organization_id, name, prefix, created_by, last_used_at, revoked_at, \
         created_at, expires_at, rotated_at from provisioning_tokens where id = $1",
    )
    .bind(token_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(omnion_identity::IdentityError::from)?
    {
        Some(row) => row,
        None => {
            return Err(ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "token_not_found",
                "no such provisioning token",
            ));
        }
    };

    if let Some(own) = current.user.organization_id
        && own != before.organization_id
    {
        // Same answer as "no such token", and for the same reason: the caller may not learn that
        // this id names anything.
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "token_not_found",
            "no such provisioning token",
        ));
    }

    let issued = provisioning::rotate_token(state.db().pool(), token_id, Some(current.user.id))
        .await
        .map_err(|error| match error {
            omnion_identity::IdentityError::InvalidProvisioning(message) => {
                // A rotation of an already-revoked token is a conflict, not a bad request: the
                // caller's intent is well-formed, the state they are pointing at has moved on.
                if message.contains("already revoked") {
                    ApiError::new(
                        axum::http::StatusCode::CONFLICT,
                        "token_not_live",
                        message,
                    )
                } else {
                    ApiError::bad_request("invalid_request", message)
                }
            }
            other => ApiError::from(other),
        })?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provisioning.token_rotated")
            .target("provisioning_token", before.id.to_string())
            .metadata(json!({
                "old_prefix": before.prefix,
                "new_prefix": issued.token.prefix,
                "name": issued.token.name,
            }))
            .ip_address(address.as_text())
            .organization(Some(before.organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.provisioning.token_rotated")
            .organization(Some(before.organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "old_token_id": before.id,
                "new_token_id": issued.token.id,
                "old_prefix": before.prefix,
                "new_prefix": issued.token.prefix,
            })),
    )
    .await;

    // The replaced token is re-read rather than rendered from the row captured before the
    // rotation. That row is a *pre-image*: it says `rotated: false` about a token that is now
    // rotated, and the panel renders exactly this response immediately after the button — so the
    // one surface that says "this was replaced" would contradict the list it refreshes into. A
    // response about a write should describe the write, not the state that preceded it.
    let replaced = sqlx::query_as::<_, provisioning::ProvisioningToken>(
        "select id, organization_id, name, prefix, created_by, last_used_at, revoked_at, \
         created_at, expires_at, rotated_at from provisioning_tokens where id = $1",
    )
    .bind(before.id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(omnion_identity::IdentityError::from)?
    .ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "token_not_found",
            "no such provisioning token",
        )
    })?;

    Ok(Json(json!({
        "rotated": true,
        "replaced": token_json(&replaced),
        "token": token_json(&issued.token),
        "secret": issued.secret,
    })))
}

/// The sync log, newest first.
pub async fn list_log(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ProvisioningQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let log = provisioning::list_log(state.db().pool(), organization_id, query.limit).await?;

    Ok(Json(json!({
        "organization_id": organization_id,
        "log": log.iter().map(log_json).collect::<Vec<_>>(),
    })))
}
