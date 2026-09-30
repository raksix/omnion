//! `/api/v1/content-api/tokens` — the panel's side of the headless content API (REQ-019, slice 1).
//!
//! This module manages credentials; it does not read content. The read surface
//! (`/api/v1/content/*`) arrives in slice 2 and authenticates with the tokens minted here, which
//! is why this file is the only place the plaintext ever appears.
//!
//! Three decisions that are easy to get subtly wrong:
//!
//! 1. **The create response is the only copy, and the panel is told so in the payload.** Not in a
//!    toast that can be missed — in the JSON, as `plaintext_shown_once: true` — because the dialog
//!    that shows it is a component, and a component that is not told the value is unrecoverable
//!    will happily render an input that lets someone close and come back expecting a second look.
//!
//! 2. **Rotation and revoke are separate verbs, and rotation is not revocation.** A rotate
//!    answers a new plaintext and leaves the same row; a revoke answers nothing secret and
//!    leaves the row visible in the list as `revoked`. Collapsing them would mean a leaked token
//!    is either permanently dead (annoying) or permanently alive (a breach) with no middle.
//!
//! 3. **A duplicate name is a 409 with the field named, not a 500.** The unique index is on
//!    `lower(name)`, so "Prod" and "prod" collide, and the person who typed it needs to be told
//!    *which* field to change — the same message shape the members screen uses for a duplicate
//!    email.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::api_tokens::{
    self, AuthFailure, TokenChanges, EXPIRY_PRESETS, RATE_TIER_ELEVATED, RATE_TIER_STANDARD, SCOPES,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::menus::{emit, record, site_in_scope};
use crate::state::AppState;

/// A token as the panel's list renders it. Structurally incapable of carrying the secret.
#[derive(Debug, Serialize)]
pub struct TokenBody {
    /// Row identity.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// The copyable `omn_xxxxxxxx` marker.
    pub prefix: String,
    /// Site scope, or `null` for every site of the organization.
    pub site_id: Option<Uuid>,
    /// Site key when scoped, for the "one site" column.
    pub site_key: Option<String>,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact origins allowed to call.
    pub allowed_origins: Vec<String>,
    /// Requests per minute before `429`.
    pub rate_limit_per_minute: i32,
    /// Expiry, ISO-8601.
    pub expires_at: Option<String>,
    /// Revocation time, ISO-8601.
    pub revoked_at: Option<String>,
    /// Last successful use, ISO-8601.
    pub last_used_at: Option<String>,
    /// Creation time, ISO-8601.
    pub created_at: String,
    /// `active`, `expired` or `revoked`.
    ///
    /// Derived here rather than stored: three states that mean different things to the person
    /// reading the list, and a column that can disagree with `expires_at` is a column that lies.
    pub status: &'static str,
}

/// The create response: the row plus the one copy of the plaintext.
#[derive(Debug, Serialize)]
pub struct CreatedTokenBody {
    /// The row, without any secret.
    pub token: TokenBody,
    /// `omn_<prefix>_<secret>`. Shown once, never recoverable.
    pub plaintext: String,
    /// Always `true`, so the client can render the warning from the payload rather than from a
    /// second request that could disagree.
    pub plaintext_shown_once: bool,
}

/// What the create dialog needs before it can be drawn: the vocabulary.
#[derive(Debug, Serialize)]
pub struct TokenVocabularyBody {
    /// Selectable scopes.
    pub scopes: Vec<String>,
    /// Scopes that exist but are not implemented in v1, with the reason.
    pub reserved_scopes: Vec<ReservedScopeBody>,
    /// Expiry presets, `(label, days)`; days `0` is "never".
    pub expiry_presets: Vec<ExpiryPresetBody>,
    /// The two rate tiers.
    pub rate_tiers: Vec<RateTierBody>,
    /// Longest accepted name.
    pub max_name_length: usize,
}

/// A scope that is reserved by name but not yet implemented.
#[derive(Debug, Serialize)]
pub struct ReservedScopeBody {
    /// The scope string.
    pub scope: String,
    /// Why it is not live, in the panel's own voice.
    pub note: String,
}

/// One expiry choice.
#[derive(Debug, Serialize)]
pub struct ExpiryPresetBody {
    /// Dialog label.
    pub label: String,
    /// Days, or `0` for "never".
    pub days: i32,
}

/// One rate-limit tier.
#[derive(Debug, Serialize)]
pub struct RateTierBody {
    /// Dialog label.
    pub label: String,
    /// Requests per minute.
    pub per_minute: i32,
    /// Whether the Elevated tier needs a permission the Standard one does not.
    pub elevated: bool,
}

/// Create a token.
#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    /// Display name, unique per organization (case-insensitively).
    pub name: String,
    /// Site scope, or `None` for every site of the organization.
    pub site_id: Option<Uuid>,
    /// At least one of [`SCOPES`].
    pub scopes: Vec<String>,
    /// Exact origins, one per entry; empty means any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// One of the two tiers.
    #[serde(default)]
    pub rate_limit_per_minute: Option<i32>,
    /// Expiry preset in days; `0` or absent means the default 90.
    #[serde(default)]
    pub expires_in_days: Option<i32>,
}

/// Edit a token.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateTokenRequest {
    /// New name.
    pub name: Option<String>,
    /// Replacement scope set.
    pub scopes: Option<Vec<String>>,
    /// Replacement origin allow-list.
    pub allowed_origins: Option<Vec<String>>,
    /// Replacement rate tier.
    pub rate_limit_per_minute: Option<i32>,
    /// Expiry in days counted from now; `0` means "never".
    pub expires_in_days: Option<i32>,
}

/// `GET /api/v1/content-api/tokens`.
pub async fn list_tokens(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<Vec<TokenBody>>, ApiError> {
    let tokens = api_tokens::list_tokens(state.db().pool(), current.organization_id).await?;
    let mut bodies = Vec::with_capacity(tokens.len());
    for token in &tokens {
        bodies.push(token_body(&state, token).await?);
    }
    Ok(Json(bodies))
}

/// `GET /api/v1/content-api/tokens/vocabulary` — what the create dialog may offer.
///
/// A separate read rather than three constants duplicated into the client, because a dialog that
/// invents its own scope list will eventually offer a scope the store refuses, and the person
/// finds out at submit time.
pub async fn token_vocabulary(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<TokenVocabularyBody>, ApiError> {
    Ok(Json(TokenVocabularyBody {
        scopes: vec![SCOPES[0].to_string(), SCOPES[1].to_string()],
        reserved_scopes: vec![ReservedScopeBody {
            scope: SCOPES[2].to_string(),
            note: "Reserved for a future write surface. v1 tokens are read-only.".to_string(),
        }],
        expiry_presets: EXPIRY_PRESETS
            .iter()
            .map(|(label, days)| ExpiryPresetBody {
                label: (*label).to_string(),
                days: *days,
            })
            .collect(),
        rate_tiers: vec![
            RateTierBody {
                label: "Standard".to_string(),
                per_minute: RATE_TIER_STANDARD,
                elevated: false,
            },
            RateTierBody {
                label: "Elevated".to_string(),
                per_minute: RATE_TIER_ELEVATED,
                elevated: true,
            },
        ],
        max_name_length: api_tokens::MAX_NAME_LENGTH,
    }))
}

/// `POST /api/v1/content-api/tokens` — mint, and return the plaintext exactly once.
pub async fn create_token(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<CreatedTokenBody>), ApiError> {
    // The site, when one is named, must belong to the caller's organization. Validated through
    // the same helper every other surface uses, so a token can never be scoped to a site the
    // caller does not own — the row would be unreadable and the leak is silent.
    if let Some(site_id) = body.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }
    if body.scopes.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_parameter", "pick at least one scope")
            .with_details(json!({ "field": "scopes" })));
    }
    let days = body.expires_in_days.unwrap_or(90);
    if !EXPIRY_PRESETS.iter().any(|(_, preset)| *preset == days) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            "expiry must be one of the offered presets",
        )
        .with_details(json!({ "field": "expires_in_days" })));
    }
    let now = OffsetDateTime::now_utc();
    let created = api_tokens::create_token(
        state.db().pool(),
        current.organization_id,
        body.site_id,
        &body.name,
        &body.scopes,
        &body.allowed_origins,
        body.rate_limit_per_minute.unwrap_or(RATE_TIER_STANDARD),
        api_tokens::expiry_from_preset(days, now),
        Some(current.user.id),
    )
    .await
    .map_err(map_token_error)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "content.api.token.create")
            .organization(current.organization_id)
            .target("api_token", created.token.id)
            .metadata(json!({
                "name": created.token.name,
                "prefix": created.token.prefix,
                "scopes": created.token.scopes,
                "site_id": created.token.site_id,
            })),
    )
    .await?;
    // The event carries the prefix and never the secret: an event feed is a place a secret ends
    // up in a third party's dashboard.
    emit(
        &state,
        "content.api.token.created",
        json!({
            "token_id": created.token.id,
            "prefix": created.token.prefix,
            "scopes": created.token.scopes,
        }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CreatedTokenBody {
            token: token_body(&state, &created.token).await?,
            plaintext: created.plaintext,
            plaintext_shown_once: true,
        }),
    ))
}

/// `PATCH /api/v1/content-api/tokens/{id}`.
pub async fn update_token(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateTokenRequest>,
) -> Result<Json<TokenBody>, ApiError> {
    let existing = api_tokens::get_token(state.db().pool(), current.organization_id, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such token"))?;
    if let Some(site_id) = existing.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }
    let changes = TokenChanges {
        name: body.name,
        scopes: body.scopes,
        allowed_origins: body.allowed_origins,
        rate_limit_per_minute: body.rate_limit_per_minute,
        expires_at: body
            .expires_in_days
            .map(|days| api_tokens::expiry_from_preset(days, OffsetDateTime::now_utc())),
    };
    let updated = api_tokens::update_token(state.db().pool(), current.organization_id, id, &changes)
        .await
        .map_err(map_token_error)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such token"))?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "content.api.token.update")
            .organization(current.organization_id)
            .target("api_token", id)
            .metadata(json!({ "scopes": updated.scopes })),
    )
    .await?;
    Ok(Json(token_body(&state, &updated).await?))
}

/// `POST /api/v1/content-api/tokens/{id}/rotate` — a new secret, the old one dead immediately.
pub async fn rotate_token(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<CreatedTokenBody>, ApiError> {
    // Scoped read first: rotation is a manage action, and a rotation of somebody else's token
    // would be a way to break a third party's integration while the row stays visible.
    let existing = api_tokens::get_token(state.db().pool(), current.organization_id, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such token"))?;
    if existing.revoked_at.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "token_revoked",
            "a revoked token cannot be rotated; create a new one",
        ));
    }
    let (rotated, plaintext) = api_tokens::rotate_token(state.db().pool(), id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such token"))?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "content.api.token.rotate")
            .organization(current.organization_id)
            .target("api_token", id)
            .metadata(json!({ "prefix": rotated.prefix })),
    )
    .await?;
    emit(
        &state,
        "content.api.token.rotated",
        json!({ "token_id": id, "prefix": rotated.prefix }),
    )
    .await;

    Ok(Json(CreatedTokenBody {
        token: token_body(&state, &rotated).await?,
        plaintext,
        plaintext_shown_once: true,
    }))
}

/// `DELETE /api/v1/content-api/tokens/{id}` — revoke. Idempotent by design.
pub async fn revoke_token(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let revoked = api_tokens::revoke_token(state.db().pool(), current.organization_id, id).await?;
    if !revoked {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such token"));
    }
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "content.api.token.revoke")
            .organization(current.organization_id)
            .target("api_token", id)
            .metadata(json!({})),
    )
    .await?;
    emit(
        &state,
        "content.api.token.revoked",
        json!({ "token_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The state a token is in, as a person reads it.
#[must_use]
pub fn token_status(revoked_at: Option<OffsetDateTime>, expires_at: Option<OffsetDateTime>) -> &'static str {
    if revoked_at.is_some() {
        return "revoked";
    }
    if let Some(expiry) = expires_at {
        if OffsetDateTime::now_utc() >= expiry {
            return "expired";
        }
    }
    "active"
}

/// Render a row for the panel.
async fn token_body(state: &AppState, token: &api_tokens::ApiToken) -> Result<TokenBody, ApiError> {
    // The site key is read only for scoped tokens, and one query for the whole list would need a
    // join; a per-row read is a second round trip the panel can afford at token-list sizes and a
    // join here would couple this module to the site table's shape.
    let site_key = match token.site_id {
        Some(site_id) => sqlx::query_scalar::<_, String>("select key from sites where id = $1")
            .bind(site_id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("reading the site key: {error}"),
                )
            })?,
        None => None,
    };
    Ok(TokenBody {
        id: token.id,
        name: token.name.clone(),
        prefix: token.prefix.clone(),
        site_id: token.site_id,
        site_key,
        scopes: token.scopes.clone(),
        allowed_origins: token.allowed_origins.clone(),
        rate_limit_per_minute: token.rate_limit_per_minute,
        expires_at: token.expires_at.map(|value| value.to_string()),
        revoked_at: token.revoked_at.map(|value| value.to_string()),
        last_used_at: token.last_used_at.map(|value| value.to_string()),
        created_at: token.created_at.to_string(),
        status: token_status(token.revoked_at, token.expires_at),
    })
}

/// Map a store error to the platform envelope, naming the field when it can.
fn map_token_error(error: omnion_content::ContentError) -> ApiError {
    use omnion_content::ContentError;
    match error {
        ContentError::TokenNameTaken(name) => ApiError::new(
            StatusCode::CONFLICT,
            "name_taken",
            format!("a token named \"{name}\" already exists in this organization"),
        )
        .with_details(json!({ "field": "name" })),
        ContentError::InvalidName(message)
        | ContentError::InvalidText(message)
        | ContentError::InvalidKey(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_parameter", message)
                .with_details(json!({ "field": "name" }))
        }
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            other.to_string(),
        ),
    }
}

/// The 401 an unauthenticated content request answers, built from the store's verdict.
///
/// Exposed so slice 2's read surface and this module cannot disagree about what a failure is
/// called: a token that expired and a token that was never right are two different integrations
/// problems, and both reaching the caller as `invalid_token` teaches them to rotate a token that
/// was fine.
#[must_use]
pub fn auth_failure_response(failure: &AuthFailure) -> ApiError {
    let message = match failure {
        AuthFailure::Invalid => "this token is not valid",
        AuthFailure::Expired => "this token has expired",
        AuthFailure::Revoked => "this token was revoked",
    };
    ApiError::new(StatusCode::UNAUTHORIZED, failure.code(), message)
}
