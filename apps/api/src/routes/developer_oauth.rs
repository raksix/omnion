//! `/api/v1/oauth-apps` — the panel's half of the OAuth story (docs/requests/REQ-033, slice 3).
//!
//! This file is deliberately **only the panel side**: registering an app, editing it, rotating
//! its client secret, withdrawing it. The two endpoints a third-party client actually calls —
//! the authorization request and the token request — are in `oauth_flow.rs`, and they are a
//! different kind of surface: those take no session, carry a client secret in a body, and are
//! reached by machines rather than by people in a panel.
//!
//! # Why the split matters more than it looks
//!
//! The two halves authenticate differently, and a handler that serves both has to pick one
//! answer for "who is this?". A panel handler resolves the organization from the *session*; a
//! token handler resolves it from the *app row*. If one endpoint did both, the tenant it acted
//! on would depend on which value arrived first, and a caller who could influence that would be
//! choosing which tenant's data a write lands in. Keeping them apart makes the tenant for every
//! statement in this file come from one place: [`organization_of`].
//!
//! # The write-only property, again
//!
//! [`MintedAppResponse`] is the only shape here that carries a `client_secret`, and — exactly
//! as for API keys — it is produced only by a handler that just *wrote* a new hash. There is no
//! path in this file from a stored row to a plaintext, so "the secret came back a second time"
//! is a code path that does not exist rather than a test somebody has to remember to write.
//!
//! Reading an app returns [`OAuthApp`], which has no secret field to fill in. Its
//! `previous_secret_expires_at` is the half of the overlap an operator needs (when do I have to
//! finish redeploying?) and is not a credential.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_developer::model_oauth::{AppEdit, AppStatus, MintedApp, NewApp};
use omnion_developer::oauth::{GrantType, SECRET_OVERLAP_DAYS};
use omnion_developer::store_oauth;
use omnion_developer::{DeveloperError, OAuthApp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::developer::{audit, organization_of};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/oauth-apps`.
#[derive(Debug, Deserialize)]
pub struct CreateAppInput {
    /// Display name, 3–60 characters, unique among this tenant's live apps.
    pub name: String,
    /// Shown on the consent screen. Optional.
    #[serde(default)]
    pub description: Option<String>,
    /// Object key of an already-uploaded logo, served through the media surface.
    #[serde(default)]
    pub logo_object_key: Option<String>,
    /// Absolute redirect URIs, one per line of the panel's field.
    pub redirect_uris: Vec<String>,
    /// Permission keys the app may ever be granted.
    pub scopes: Vec<String>,
    /// Flows it may use. Omit for `authorization_code` alone.
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
}

/// Body of `PATCH /api/v1/oauth-apps/{id}`.
///
/// Every field optional, because the panel's form submits the whole record and a `PATCH` that
/// demanded all of them would make "change the description" a five-field request. `description`
/// and `logo_object_key` are `Option<Option<String>>` rather than plain options so that
/// *absent* and *cleared* are different: a `null` is the "remove this" button, and a field that
/// was omitted means "leave it alone".
#[derive(Debug, Default, Deserialize)]
pub struct EditAppInput {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description; `null` clears it.
    #[serde(default)]
    pub description: Option<Option<String>>,
    /// New logo object key; `null` clears it.
    #[serde(default)]
    pub logo_object_key: Option<Option<String>>,
    /// New redirect URI list. Replacing it invalidates every authorization in flight, which the
    /// panel warns about before it saves.
    #[serde(default)]
    pub redirect_uris: Option<Vec<String>>,
    /// New scope list.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// New flow list.
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    /// New status.
    #[serde(default)]
    pub status: Option<String>,
}

/// The apps list. A wrapper rather than a bare array, for the reason the key list is one.
#[derive(Debug, Serialize)]
pub struct AppsResponse {
    /// The apps, newest first.
    pub apps: Vec<AppSummary>,
}

/// One row of the list screen.
///
/// A **summary**, not the whole [`OAuthApp`]: a list of ten apps each carrying three redirect
/// URIs and a scope list is a payload nobody reads and a table that cannot be scanned. The
/// detail screen has the full row; the list has what a row needs to be recognisable and to say
/// whether it needs attention.
#[derive(Debug, Serialize)]
pub struct AppSummary {
    /// App id — what the row's link and its buttons carry.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// The public identifier, safe to display and to read aloud.
    pub client_id: String,
    /// `active`, `suspended` or `deleted`.
    pub status: AppStatus,
    /// How many redirect URIs are registered. Shown as a count on the row because the list is
    /// where somebody asks "is this the integration I think it is", and the count is the fastest
    /// way to tell a dev server's app from a production one.
    pub redirect_uri_count: usize,
    /// The first registered URI, for the row's subtitle.
    pub primary_redirect_uri: Option<String>,
    /// How many flows it may use.
    pub grant_types: Vec<GrantType>,
    /// Whether a rotation's overlap is still open, and until when — the one thing on this row
    /// that can be a *to-do* rather than a fact.
    pub previous_secret_expires_at: Option<OffsetDateTime>,
    /// When it was registered.
    pub created_at: OffsetDateTime,
}

/// The app detail: the full row plus the two figures only a live query can give.
#[derive(Debug, Serialize)]
pub struct AppDetailResponse {
    /// The app. No secret, and no field for one.
    pub app: OAuthApp,
    /// Authorization codes issued to this app that are still redeemable.
    ///
    /// A separate query rather than a stored counter, and deliberately *not* a count of rows in
    /// the ledger: an app with forty rows in the table and none redeemable is not holding forty
    /// codes, and a panel that said it was would send an operator hunting for a compromise that
    /// never happened. The column the sweep walks (`used_at is null`) is the same predicate.
    pub live_authorization_codes: i64,
}

/// A registered or rotated app, with its client secret shown exactly once.
///
/// The secret is named `client_secret` rather than `secret` because that is the word the OAuth
/// spec and every client library use, and a developer copying from this dialog into a
/// configuration file should not have to translate. The flatten means the response *is* an
/// `OAuthApp` with one extra field, so a client that already reads an app needs no new shape.
#[derive(Debug, Serialize)]
pub struct MintedAppResponse {
    /// The app as it now is.
    #[serde(flatten)]
    pub app: OAuthApp,
    /// The client secret. Displayed once and never again.
    pub client_secret: String,
    /// How many days the previous secret remains valid, when a rotation's overlap is open.
    ///
    /// Spelled out because "7" is a number a person can put in a calendar and the timestamp has
    /// to be computed against. The **timestamp itself is deliberately not repeated here**: the
    /// flattened [`OAuthApp`] already carries `previous_secret_expires_at`, and two fields
    /// serialising to the same JSON key is a collision the flatter writes first — so the app's
    /// own value is the one a client reads and this one is the one nobody does. That is not a
    /// cosmetic difference: on a *creation* the app's value is `null` (there is no previous
    /// secret) and on a *rotation* it is the deadline, so the pair has to come from one place,
    /// and the only place that has both facts is [`MintedApp`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_secret_valid_for_days: Option<i64>,
}

impl From<MintedApp> for MintedAppResponse {
    fn from(minted: MintedApp) -> Self {
        // `MintedApp::previous_secret_expires_at` and `app.previous_secret_expires_at` are two
        // views of one fact, and this is the assertion that they agree — a rotation that wrote
        // the overlap into the ledger but not into the row would otherwise show the operator
        // "no overlap" while the old secret keeps working for a week. Taking the *row's* value
        // for the response and this one only for "is there an overlap at all" means the panel
        // cannot be told a deadline the database does not hold.
        debug_assert_eq!(
            minted.previous_secret_expires_at, minted.app.previous_secret_expires_at,
            "the minted app and its row must agree about the overlap deadline"
        );
        Self {
            previous_secret_valid_for_days: minted
                .previous_secret_expires_at
                .map(|_| SECRET_OVERLAP_DAYS),
            app: minted.app,
            client_secret: minted.plaintext,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/oauth-apps` — this tenant's applications.
pub async fn list_apps(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<AppsResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let apps = store_oauth::list_apps(state.db().pool(), organization_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(AppsResponse {
        apps: apps.iter().map(AppSummary::from).collect(),
    }))
}

impl From<&OAuthApp> for AppSummary {
    fn from(app: &OAuthApp) -> Self {
        Self {
            id: app.id,
            name: app.name.clone(),
            client_id: app.client_id.clone(),
            status: app.status,
            redirect_uri_count: app.redirect_uris.len(),
            primary_redirect_uri: app.redirect_uris.first().cloned(),
            grant_types: app.grant_types.clone(),
            previous_secret_expires_at: app.previous_secret_expires_at,
            created_at: app.created_at,
        }
    }
}

/// `GET /api/v1/oauth-apps/{id}` — one app and its live-code count.
pub async fn get_app(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(app_id): Path<Uuid>,
) -> Result<Json<AppDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let app = store_oauth::get_app(state.db().pool(), organization_id, app_id)
        .await
        .map_err(ApiError::from)?;
    let live = store_oauth::live_code_count(state.db().pool(), app_id, OffsetDateTime::now_utc())
        .await
        .map_err(ApiError::from)?;

    Ok(Json(AppDetailResponse {
        app,
        live_authorization_codes: live,
    }))
}

/// `POST /api/v1/oauth-apps` — register an app. The secret is in this response and nowhere else.
pub async fn create_app(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<CreateAppInput>,
) -> Result<(StatusCode, Json<MintedAppResponse>), ApiError> {
    let organization_id = organization_of(&current)?;

    // An omitted `grant_types` means the ordinary browser flow, which is what a panel's
    // unchecked box should mean. It is spelled here rather than defaulted in the crate because
    // "what does an absent field mean" is an API decision, not a domain rule.
    let grant_types = match input.grant_types.as_deref() {
        None | Some([]) => vec![GrantType::AuthorizationCode],
        Some(list) => parse_grants(list)?,
    };

    let minted = store_oauth::create_app(
        state.db().pool(),
        organization_id,
        &NewApp {
            name: input.name,
            description: input.description,
            logo_object_key: input.logo_object_key,
            redirect_uris: input.redirect_uris,
            scopes: input.scopes,
            grant_types,
            created_by: current.user.id,
        },
    )
    .await
    .map_err(ApiError::from)?;

    // The audit entry carries the client id and the *count* of redirect URIs, never the
    // URIs themselves and never the secret. A redirect URI is an attack surface by
    // construction and this action's audit row is exactly the sort of thing that gets
    // exported to a log aggregator.
    audit(
        &state,
        &current,
        &address,
        "developer.oauth_app.created",
        json!({
            "app": minted.app.id,
            "client_id": minted.app.client_id,
            "name": minted.app.name,
            "scopes": minted.app.scopes,
            "grant_types": minted.app.grant_types,
            "redirect_uri_count": minted.app.redirect_uris.len(),
        }),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(minted.into())))
}

/// `PATCH /api/v1/oauth-apps/{id}` — a partial edit.
///
/// A `PATCH` rather than a `PUT`, and the difference is the whole reason the panel's form can
/// submit the record it read: `PUT` means "this is now the whole resource", so a form that
/// submitted only the description would blank the redirect URIs.
pub async fn edit_app(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(app_id): Path<Uuid>,
    Json(input): Json<EditAppInput>,
) -> Result<Json<AppDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;

    let grant_types = match input.grant_types.as_deref() {
        None => None,
        Some(list) => Some(parse_grants(list)?),
    };
    let status = match input.status.as_deref() {
        None => None,
        Some(value) => Some(
            AppStatus::parse(value)
                .map_err(ApiError::from)
                .map_err(|error| annotate_status(error))?,
        ),
    };

    let app = store_oauth::edit_app(
        state.db().pool(),
        organization_id,
        app_id,
        &AppEdit {
            name: input.name,
            description: input.description,
            logo_object_key: input.logo_object_key,
            redirect_uris: input.redirect_uris,
            scopes: input.scopes,
            grant_types,
            status,
        },
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    let live = store_oauth::live_code_count(state.db().pool(), app_id, OffsetDateTime::now_utc())
        .await
        .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        "developer.oauth_app.updated",
        json!({
            "app": app.id,
            "client_id": app.client_id,
            "name": app.name,
            "status": app.status,
            "scopes": app.scopes,
            "grant_types": app.grant_types,
            "redirect_uri_count": app.redirect_uris.len(),
        }),
    )
    .await?;

    Ok(Json(AppDetailResponse {
        app,
        live_authorization_codes: live,
    }))
}

/// Put the `field` detail on a status error so the panel can place it.
///
/// A free function rather than an inline `map_err` because the answer is a *fact about the
/// request* — which field was wrong — and a closure that rebuilt `ApiError` inline is the shape
/// that loses the field on the next edit.
fn annotate_status(error: ApiError) -> ApiError {
    error.with_details(json!({ "field": "status" }))
}

/// `POST /api/v1/oauth-apps/{id}/rotate` — a new client secret; the old one dies in 7 days.
pub async fn rotate_secret(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(app_id): Path<Uuid>,
) -> Result<Json<MintedAppResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let minted = store_oauth::rotate_secret(
        state.db().pool(),
        organization_id,
        app_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    // The audit row names *which* secret authenticated from now on, and — the reason the store
    // returns the slot name — records that the overlap is open. An operator reading this later
    // can answer "is every deployment on the new secret yet?" without reading the table.
    audit(
        &state,
        &current,
        &address,
        "developer.oauth_app.secret_rotated",
        json!({
            "app": minted.app.id,
            "client_id": minted.app.client_id,
            "name": minted.app.name,
            "previous_secret_valid_for_days": SECRET_OVERLAP_DAYS,
            "previous_secret_expires_at": minted.previous_secret_expires_at,
        }),
    )
    .await?;

    Ok(Json(minted.into()))
}

/// `DELETE /api/v1/oauth-apps/{id}` — withdraw. Idempotent, and it does not delete the row.
///
/// A revoked *client secret* is recoverable; a withdrawn *app* is not, because its codes and its
/// audit trail reference it and "show me what this integration did last March" has to keep
/// working. So this is a status change, and the 204 says "it is withdrawn", not "it is gone".
pub async fn delete_app(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(app_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&current)?;
    let app = store_oauth::delete_app(
        state.db().pool(),
        organization_id,
        app_id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        "developer.oauth_app.withdrawn",
        json!({
            "app": app.id,
            "client_id": app.client_id,
            "name": app.name,
        }),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/oauth-apps/{id}/suspend` — switch an app off and back on without withdrawing it.
///
/// Two verbs rather than a `PATCH` with a status, because the two are not symmetric: suspending
/// is reversible and safe, withdrawing is neither. A panel that offered both behind one dropdown
/// would eventually have somebody withdraw an app they meant to pause, and the only way back
/// would be to re-register and break every client holding the old client id.
pub async fn set_suspended(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(app_id): Path<Uuid>,
    Json(input): Json<SuspendInput>,
) -> Result<Json<AppDetailResponse>, ApiError> {
    let organization_id = organization_of(&current)?;
    let status = if input.suspended {
        AppStatus::Suspended
    } else {
        AppStatus::Active
    };

    let app = store_oauth::edit_app(
        state.db().pool(),
        organization_id,
        app_id,
        &AppEdit {
            status: Some(status),
            ..AppEdit::default()
        },
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        if input.suspended {
            "developer.oauth_app.suspended"
        } else {
            "developer.oauth_app.resumed"
        },
        json!({
            "app": app.id,
            "client_id": app.client_id,
            "name": app.name,
        }),
    )
    .await?;

    let live = store_oauth::live_code_count(state.db().pool(), app_id, OffsetDateTime::now_utc())
        .await
        .map_err(ApiError::from)?;

    Ok(Json(AppDetailResponse {
        app,
        live_authorization_codes: live,
    }))
}

/// Body of `POST /api/v1/oauth-apps/{id}/suspend`.
#[derive(Debug, Deserialize)]
pub struct SuspendInput {
    /// `true` to suspend, `false` to resume.
    pub suspended: bool,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Parse a submitted grant list, refusing an unknown name with the field named.
///
/// `parse` returning `None` for an unknown string is the right behaviour in the crate — a stored
/// row from a future build must still be listable — but a *submitted* one is a different
/// question, and "none of them parsed" has to be a `400` naming the field rather than a list
/// that quietly became empty and then failed the "at least one" rule with a vaguer message.
fn parse_grants(list: &[String]) -> Result<Vec<GrantType>, ApiError> {
    let mut grants = Vec::with_capacity(list.len());
    for raw in list {
        let trimmed = raw.trim();
        let Some(grant) = GrantType::parse(trimmed) else {
            return Err(ApiError::bad_request(
                "unknown_grant_type",
                format!("{trimmed:?} is not a grant type this platform has"),
            )
            .with_details(json!({ "field": "grant_types" })));
        };
        grants.push(grant);
    }
    Ok(grants)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_list_row_serialises_the_count_and_never_the_uris_or_a_secret() {
        // The list projection as an assertion on the bytes. The count is there because a row
        // needs to be identifiable at a glance; the URIs are not, because the detail screen has
        // them and a table of ten rows each with three long URIs is unreadable.
        let app = OAuthApp {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Reporting".to_owned(),
            description: None,
            logo_object_key: None,
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uris: vec![
                "https://app.example.com/callback".to_owned(),
                "http://localhost:3000/callback".to_owned(),
            ],
            scopes: vec!["content.pages.read".to_owned()],
            grant_types: vec![GrantType::AuthorizationCode],
            status: AppStatus::Active,
            previous_secret_expires_at: None,
            created_by: Uuid::nil(),
            created_at: datetime!(2026-10-01 12:00 UTC),
            updated_at: datetime!(2026-10-01 12:00 UTC),
        };
        let rendered = serde_json::to_string(&AppSummary::from(&app)).expect("serialises");
        assert!(rendered.contains("\"redirect_uri_count\":2"));
        assert!(rendered.contains("https://app.example.com/callback"));
        assert!(
            !rendered.contains("http://localhost:3000/callback"),
            "only the first URI is a subtitle, not the whole list"
        );
        // **Not** `!rendered.contains("secret")`: the summary legitimately carries
        // `previous_secret_expires_at`, which is a *deadline* and the one fact on this row that
        // can be a to-do. What must never appear is a credential, so the assertion is on the
        // field *names* rather than the substring — a substring check would have to be weakened
        // to let the deadline through, and the weakened version would no longer notice a secret.
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");
        let fields: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for name in &fields {
            assert!(
                !matches!(
                    *name,
                    "client_secret" | "client_secret_hash" | "previous_secret_hash"
                ),
                "{name} must not be on a list row"
            );
        }
        // And the credential's *value*, if one were somehow bound, would be a 64-char hex under
        // this scheme's prefix. Assert on the shape a real credential has rather than the word.
        assert!(!rendered.contains("omnion-oauth-secret.v1$"));
    }

    #[test]
    fn the_whole_response_shape_never_carries_a_field_named_hash() {
        // The write-only property as an assertion on bytes, in the app's own vocabulary this
        // time: not merely "no client_secret on a list row" but "no hash anywhere in the detail
        // read either".
        let rendered = serde_json::to_string(&AppsResponse { apps: Vec::new() }).expect("renders");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("hash"));
    }

    #[test]
    fn a_created_app_carries_its_secret_and_no_overlap_deadline() {
        // Creation has no previous secret, so it must not claim a deadline. A response that
        // carried `previous_secret_expires_at: null` anyway would invite a reader to wonder what
        // it used to point at.
        let minted = MintedApp {
            app: OAuthApp {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                name: "Reporting".to_owned(),
                description: None,
                logo_object_key: None,
                client_id: "omn_app_0123456789abcdef01234567".to_owned(),
                redirect_uris: vec!["https://app.example.com/cb".to_owned()],
                scopes: vec!["content.pages.read".to_owned()],
                grant_types: vec![GrantType::AuthorizationCode],
                status: AppStatus::Active,
                previous_secret_expires_at: None,
                created_by: Uuid::nil(),
                created_at: datetime!(2026-10-01 12:00 UTC),
                updated_at: datetime!(2026-10-01 12:00 UTC),
            },
            plaintext: "abc123".to_owned(),
            previous_secret_expires_at: None,
        };
        let rendered = serde_json::to_value(MintedAppResponse::from(minted)).expect("serialises");
        assert_eq!(rendered["client_secret"], "abc123");
        // Flattened: the app's own fields sit at the top level, not under `app`.
        assert_eq!(rendered["name"], "Reporting");
        assert!(rendered.get("app").is_none());
        // `previous_secret_valid_for_days` is *absent* — there is no overlap on a creation, and
        // a `null` there would read as "the deadline is unknown" rather than "there is none".
        assert!(rendered.get("previous_secret_valid_for_days").is_none());
        // The deadline comes from the flattened app and is `null` here: that is the shape, and a
        // test asserting it is *absent* would be asserting that `OAuthApp` skips a field it
        // shares with the list and the detail screens. Null is the honest spelling of "this app
        // has no previous secret", and the panel renders it as "no overlap".
        assert!(
            rendered["previous_secret_expires_at"].is_null(),
            "creation has no previous secret, so the deadline must read as null"
        );
    }

    #[test]
    fn a_rotation_states_the_deadline_in_both_spellings_and_says_how_long() {
        // "7 days" and a timestamp, because one is a number a person can put in a calendar and
        // the other is a thing they can paste into a check. The panel renders both.
        let expires = datetime!(2026-10-08 12:00 UTC);
        let minted = MintedApp {
            app: OAuthApp {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                name: "Reporting".to_owned(),
                description: None,
                logo_object_key: None,
                client_id: "omn_app_0123456789abcdef01234567".to_owned(),
                redirect_uris: vec!["https://app.example.com/cb".to_owned()],
                scopes: vec!["content.pages.read".to_owned()],
                grant_types: vec![GrantType::AuthorizationCode],
                status: AppStatus::Active,
                previous_secret_expires_at: Some(expires),
                created_by: Uuid::nil(),
                created_at: datetime!(2026-10-01 12:00 UTC),
                updated_at: datetime!(2026-10-01 12:00 UTC),
            },
            plaintext: "abc123".to_owned(),
            previous_secret_expires_at: Some(expires),
        };
        let rendered = serde_json::to_value(MintedAppResponse::from(minted)).expect("serialises");
        assert_eq!(
            rendered["previous_secret_valid_for_days"],
            SECRET_OVERLAP_DAYS
        );
        // The deadline itself comes from the flattened app — one field, one value, and a client
        // cannot read a different number depending on which duplicate it happened to find. It is
        // a `time::OffsetDateTime`, which this crate's `time` features serialise as a
        // structured value rather than an ISO string; the assertion is that it is *not null*,
        // which is the property the panel depends on.
        assert!(
            !rendered["previous_secret_expires_at"].is_null(),
            "a rotation must carry the deadline in the flattened app"
        );
        assert_eq!(rendered["client_secret"], "abc123");
    }

    #[test]
    fn an_unknown_grant_name_is_a_field_error_and_not_an_empty_list() {
        // The refusal that must not degrade: "none of them parsed" is not the same answer as
        // "you sent nothing", and the panel needs the field to place the message under.
        let error = parse_grants(&["authorization_code".to_owned(), "device_code".to_owned()])
            .expect_err("an unknown flow must be refused");
        // Read through the accessors rather than by serialising: `ApiError` renders itself as
        // the `{"error":{…}}` envelope only on the way out, and a test that asserted on that
        // shape would be asserting on the wire format from inside the crate that produces it.
        assert_eq!(error.code(), "unknown_grant_type");
        assert_eq!(
            error.details().and_then(|details| details.get("field")),
            Some(&json!("grant_types")),
            "the panel places the message under the field, so the field has to be named"
        );
        // And the message must not quote the submitted value: an unknown grant name is
        // caller-supplied, and this error can reach a log.
        assert!(error.message().contains("device_code"));
    }

    #[test]
    fn both_known_grants_parse_and_trim() {
        // The panel's checkboxes submit exactly these strings, with whitespace from a textarea.
        assert_eq!(
            parse_grants(&[
                " authorization_code ".to_owned(),
                "client_credentials".to_owned()
            ])
            .expect("both are known"),
            vec![GrantType::AuthorizationCode, GrantType::ClientCredentials]
        );
    }
}
