//! `/api/v1/iam/providers` — the sign-in providers of an organization (REQ-006, slice 4b-2;
//! docs/07-IAM.md §11).
//!
//! The management half of enterprise sign-in. The other half — the browser round trip itself —
//! lives in [`crate::routes::sso`], because it is a *public* surface: a callback arrives with no
//! session, so it cannot sit behind a permission guard.
//!
//! Two rules shape everything here:
//!
//! * **No secret ever enters a row or a response.** A provider names where its client secret
//!   lives (`secret_ref`) and the panel can only ever edit that *name*. The list answers with the
//!   wiring plus a boolean saying whether the named variable is defined in this installation, so
//!   an operator can tell "not set up" from "broken" without the value ever being readable.
//! * **Local sign-in stays available in every state.** Nothing on this surface can turn password
//!   sign-in off; a provider is a *choice*, and a disabled one is unreachable rather than gone.
//!
//! The `test` action is the useful one: it asks the provider what it actually is (OIDC discovery
//! and the JWKS, a SAML metadata document) instead of trusting what an operator typed, so a
//! half-configured provider is caught before the first real person tries to sign in with it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::sso::oidc::{self, Discovery, HttpClient};
use omnion_identity::sso::providers::{
    self, AuthProvider, NewProvider, ProviderChanges, ProviderKind, default_scopes,
};
use omnion_identity::sso::provisioning;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// Largest provider event page the list answers.
const MAX_EVENT_PAGE: i64 = 100;

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Query of the provider list and the event log.
#[derive(Debug, Deserialize)]
pub struct ProviderQuery {
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// How many sign-in events to answer with the list.
    #[serde(default)]
    pub event_limit: Option<i64>,
}

/// The body of a provider creation.
#[derive(Debug, Deserialize)]
pub struct NewProviderBody {
    /// URL-safe name the sign-in URL carries (`/auth/sso/okta/start`).
    pub slug: String,
    /// Which protocol: `oidc`, `oauth2` or `saml`.
    pub kind: String,
    /// Label on the sign-in screen.
    pub name: String,
    /// Endpoints and provider-specific settings.
    #[serde(default)]
    pub config: Value,
    /// Name of the environment variable holding the client secret.
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// Scopes to request; the kind's defaults are used when this is empty.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Claim carrying group membership.
    #[serde(default)]
    pub group_claim: Option<String>,
    /// Role every successful sign-in gets.
    #[serde(default)]
    pub default_role_id: Option<Uuid>,
    /// Provision an account on first sign-in?
    #[serde(default)]
    pub jit_enabled: Option<bool>,
    /// Organization to create in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a provider update; every field is optional and `None` leaves the column alone.
#[derive(Debug, Deserialize)]
pub struct ProviderPatchBody {
    /// New label.
    #[serde(default)]
    pub name: Option<String>,
    /// New endpoints and settings.
    #[serde(default)]
    pub config: Option<Value>,
    /// New secret reference.
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// New scope list.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// New group claim.
    #[serde(default)]
    pub group_claim: Option<String>,
    /// New default role.
    #[serde(default)]
    pub default_role_id: Option<Uuid>,
    /// New JIT flag.
    #[serde(default)]
    pub jit_enabled: Option<bool>,
    /// New reachable flag.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// One provider as the panel reads it.
#[derive(Debug, Serialize)]
pub struct ProviderBody {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// URL-safe name.
    pub slug: String,
    /// Protocol.
    pub kind: &'static str,
    /// Label.
    pub name: String,
    /// Endpoints and settings.
    pub config: Value,
    /// The name of the environment variable holding the client secret (never the value).
    pub secret_ref: Option<String>,
    /// Whether that variable is defined in this installation — the field the panel turns into
    /// "secret missing" without ever reading it.
    pub secret_present: bool,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Claim carrying groups.
    pub group_claim: Option<String>,
    /// Default role.
    pub default_role_id: Option<Uuid>,
    /// Provision on first sign-in?
    pub jit_enabled: bool,
    /// Reachable?
    pub enabled: bool,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// How many sign-ins the event log holds for this provider.
    pub sign_in_count: i64,
    /// When it last signed somebody in.
    pub last_sign_in_at: Option<String>,
}

impl ProviderBody {
    /// Read a row for the panel, pairing it with its log summary.
    fn new(provider: &AuthProvider, sign_in_count: i64, last_sign_in_at: Option<String>) -> Self {
        let secret_present = provider
            .secret_ref
            .as_deref()
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .is_some_and(|reference| std::env::var(reference).is_ok());
        Self {
            id: provider.id,
            organization_id: provider.organization_id,
            slug: provider.slug.clone(),
            kind: provider.kind.as_str(),
            name: provider.name.clone(),
            config: provider.config.clone(),
            secret_ref: provider.secret_ref.clone(),
            secret_present,
            scopes: provider.scopes.clone(),
            group_claim: provider.group_claim.clone(),
            default_role_id: provider.default_role_id,
            jit_enabled: provider.jit_enabled,
            enabled: provider.enabled,
            created_at: provider.created_at.format(&Rfc3339).unwrap_or_default(),
            updated_at: provider.updated_at.format(&Rfc3339).unwrap_or_default(),
            sign_in_count,
            last_sign_in_at,
        }
    }
}

/// The answer of the discovery test.
#[derive(Debug, Serialize)]
pub struct ProviderTestBody {
    /// The provider that was asked.
    pub provider_id: Uuid,
    /// Its slug.
    pub slug: String,
    /// Its protocol.
    pub kind: &'static str,
    /// `ok` when the provider answered and every required endpoint was present.
    pub status: &'static str,
    /// What the test proved, in words the panel shows next to the button.
    pub detail: String,
    /// The endpoints the provider actually reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Value>,
    /// Whether the client secret is readable in this installation.
    pub secret_present: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The organization's providers, each with its sign-in summary.
pub async fn list_providers(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ProviderQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = resolve_organization(&current, query.organization_id)?;
    let list = providers::list_providers(state.db().pool(), organization_id).await?;

    // One grouped query rather than one per provider: the list is small but the event log is not,
    // and an N+1 here is a page that scales with the number of providers.
    let mut summaries: std::collections::HashMap<Uuid, (i64, Option<String>)> =
        std::collections::HashMap::new();
    for provider in &list {
        let (count, last) = sqlx::query_as::<_, (i64, Option<time::OffsetDateTime>)>(
            "select count(*), max(created_at) from auth_provider_events \
             where provider_id = $1 and outcome in ('success', 'provisioned')",
        )
        .bind(provider.id)
        .fetch_one(state.db().pool())
        .await
        .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;
        summaries.insert(
            provider.id,
            (
                count,
                last.map(|value| value.format(&Rfc3339).unwrap_or_default()),
            ),
        );
    }

    let rows = list
        .iter()
        .map(|provider| {
            let (sign_in_count, last_sign_in_at) = summaries
                .get(&provider.id)
                .cloned()
                .unwrap_or((0, None));
            ProviderBody::new(provider, sign_in_count, last_sign_in_at)
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({
        "organization_id": organization_id,
        "providers": rows,
        "kinds": [
            { "value": "oidc", "label": "OpenID Connect", "default_scopes": default_scopes(ProviderKind::Oidc) },
            { "value": "oauth2", "label": "OAuth 2.0", "default_scopes": default_scopes(ProviderKind::Oauth2) },
            { "value": "saml", "label": "SAML 2.0", "default_scopes": Vec::<String>::new() },
        ],
    })))
}

/// One provider by id.
pub async fn get_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ProviderBody>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let (sign_in_count, last) = sign_in_summary(&state, id).await?;
    Ok(Json(ProviderBody::new(
        &provider,
        sign_in_count,
        last.map(|value| value.format(&Rfc3339).unwrap_or_default()),
    )))
}

/// Connect a provider.
pub async fn create_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<NewProviderBody>,
) -> Result<(StatusCode, Json<ProviderBody>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;
    let kind = ProviderKind::parse(&body.kind)?;

    // The claim → role rules live inside `config`, and they are checked at save time: a rule
    // that can never be read is a sign-in that silently loses a role, which is exactly the kind
    // of thing an operator only finds out about after somebody could not get in.
    omnion_identity::sso::claims::mappings_from_config(&body.config).map_err(|error| {
        ApiError::bad_request("invalid_request", error.to_string())
            .with_details(json!({ "field": "config.role_mappings" }))
    })?;
    if let Some(reference) = body.secret_ref.as_deref().map(str::trim)
        && !reference.is_empty()
        && !reference.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(ApiError::bad_request(
            "invalid_request",
            "secret_ref is the name of an environment variable, so it may only hold A–Z, 0–9 and underscores",
        )
        .with_details(json!({ "field": "secret_ref" })));
    }

    let scopes = if body.scopes.is_empty() {
        default_scopes(kind).iter().map(|scope| (*scope).to_owned()).collect()
    } else {
        body.scopes
    };

    let created = providers::create_provider(
        state.db().pool(),
        NewProvider {
            organization_id,
            slug: body.slug,
            kind,
            name: body.name,
            config: body.config,
            secret_ref: body.secret_ref,
            scopes,
            group_claim: body.group_claim,
            default_role_id: body.default_role_id,
            // Provisioning is off until it is asked for: a provider that silently creates accounts
            // is a provider that can be used to fill an organization with strangers.
            jit_enabled: body.jit_enabled.unwrap_or(false),
            // A provider is created switched off. The `test` action is how it is proved, and
            // `enabled` is how it is published.
            enabled: false,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_connected")
            .target("auth_provider", created.id.to_string())
            .metadata(json!({
                "slug": created.slug,
                "kind": created.kind.as_str(),
                "organization_id": created.organization_id,
            }))
            .ip_address(address.as_text())
            .organization(Some(organization_id)),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.provider_connected")
            .organization(Some(organization_id))
            .actor(Some(current.user.id))
            .payload(json!({
                "provider_id": created.id,
                "slug": created.slug,
                "kind": created.kind.as_str(),
            })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(ProviderBody::new(&created, 0, None))))
}

/// Change a provider's mutable fields.
pub async fn update_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<ProviderPatchBody>,
) -> Result<Json<ProviderBody>, ApiError> {
    load(&state, &current, id).await?;

    if let Some(config) = body.config.as_ref() {
        omnion_identity::sso::claims::mappings_from_config(config)
            .map_err(|error| ApiError::bad_request("invalid_request", error.to_string()))?;
    }

    let updated = providers::update_provider(
        state.db().pool(),
        id,
        ProviderChanges {
            name: body.name,
            config: body.config,
            secret_ref: body.secret_ref,
            scopes: body.scopes,
            group_claim: body.group_claim,
            default_role_id: body.default_role_id,
            jit_enabled: body.jit_enabled,
            enabled: body.enabled,
        },
    )
    .await?
    .ok_or_else(|| ApiError::new(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "no such provider",
        ))?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_updated")
            .target("auth_provider", id.to_string())
            .metadata(json!({
                "slug": updated.slug,
                "kind": updated.kind.as_str(),
                "enabled": updated.enabled,
            }))
            .ip_address(address.as_text())
            .organization(Some(updated.organization_id)),
    )
    .await?;

    Ok(Json(ProviderBody::new(&updated, 0, None)))
}

/// Remove a provider. Its challenges and event log go with it (the migration cascades them).
pub async fn delete_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let provider = load(&state, &current, id).await?;
    providers::delete_provider(state.db().pool(), id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_removed")
            .target("auth_provider", id.to_string())
            .metadata(json!({ "slug": provider.slug, "kind": provider.kind.as_str() }))
            .ip_address(address.as_text())
            .organization(Some(provider.organization_id)),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Ask the provider what it is.
///
/// This is the action that makes the screen honest: a typed-in issuer that is not the issuer the
/// provider publishes, a JWKS that cannot be read, or a SAML certificate nobody can parse are
/// all caught here rather than on the first real sign-in. The answer is deliberately a `200` with
/// a `status` field even when the provider is broken — a failed *test* is a result, not an error
/// the browser has to guess at.
pub async fn test_provider(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ProviderTestBody>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let client = HttpClient::new();
    let secret_present = provisioning::resolve_client_secret(&provider)
        .map(|value| value.is_some())
        .unwrap_or(false);

    let (status, detail, endpoints) = match provider.kind {
        ProviderKind::Oidc | ProviderKind::Oauth2 => {
            test_oidc(&client, &provider, secret_present).await
        }
        ProviderKind::Saml => test_saml(&provider, secret_present),
    };

    Ok(Json(ProviderTestBody {
        provider_id: provider.id,
        slug: provider.slug.clone(),
        kind: provider.kind.as_str(),
        status,
        detail,
        endpoints,
        secret_present,
    }))
}

/// The sign-in event log of one provider, newest first.
pub async fn list_provider_events(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(query): Query<ProviderQuery>,
) -> Result<Json<Value>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let limit = query.event_limit.unwrap_or(25).clamp(1, MAX_EVENT_PAGE);

    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<Uuid>, String, Option<String>, Option<String>)>(
        "select outcome, reason, external_subject, user_id, \
                array_to_string(roles_applied, ', '), ip_address::text, created_at::text \
         from auth_provider_events where provider_id = $1 \
         order by created_at desc limit $2",
    )
    .bind(provider.id)
    .bind(limit)
    .fetch_all(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;

    let events = rows
        .into_iter()
        .map(|(outcome, reason, subject, user_id, roles, ip, created_at)| {
            json!({
                "outcome": outcome,
                "reason": reason,
                "external_subject": subject,
                "user_id": user_id,
                "roles_applied": roles,
                "ip_address": ip,
                "created_at": created_at,
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({ "provider_id": provider.id, "events": events })))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Read a provider and refuse one outside the caller's organization.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<AuthProvider, ApiError> {
    let provider = providers::find_provider(state.db().pool(), id)
        .await?
        .ok_or_else(|| ApiError::new(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "no such provider",
        ))?;
    resolve_organization(current, Some(provider.organization_id))?;
    Ok(provider)
}

/// How many times a provider signed somebody in, and when it last did.
async fn sign_in_summary(
    state: &AppState,
    id: Uuid,
) -> Result<(i64, Option<time::OffsetDateTime>), ApiError> {
    sqlx::query_as::<_, (i64, Option<time::OffsetDateTime>)>(
        "select count(*), max(created_at) from auth_provider_events \
         where provider_id = $1 and outcome in ('success', 'provisioned')",
    )
    .bind(id)
    .fetch_one(state.db().pool())
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))
}

/// The `code` flows: discovery first, then the key set the discovery document points at.
async fn test_oidc(
    client: &HttpClient,
    provider: &AuthProvider,
    secret_present: bool,
) -> (&'static str, String, Option<Value>) {
    // A SAML provider stores its endpoints explicitly; an OIDC one may either name an issuer to
    // discover, or name the endpoints outright for a provider that publishes no discovery
    // document. Both are supported, and which one is in use is visible in the row.
    let issuer = provider
        .config
        .get("issuer")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let discovery = match issuer.as_deref() {
        Some(issuer) => {
            let url = format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            );
            match client.get_json(&url).await {
                Ok(document) => match Discovery::from_value(&document) {
                    Ok(discovery) => discovery,
                    Err(error) => {
                        return (
                            "failed",
                            format!("the discovery document is incomplete: {error}"),
                            None,
                        );
                    }
                },
                Err(error) => {
                    return (
                        "failed",
                        format!("the discovery document could not be read: {error}"),
                        None,
                    );
                }
            }
        }
        None => {
            let text = |field: &str| {
                provider
                    .config
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            let (Some(issuer), Some(authorization_endpoint), Some(token_endpoint), Some(jwks_uri)) = (
                text("issuer"),
                text("authorization_endpoint"),
                text("token_endpoint"),
                text("jwks_uri"),
            ) else {
                return (
                    "failed",
                    "the provider names no issuer, so nothing can be discovered — set `issuer` \
                     in the configuration to discover the rest"
                        .to_owned(),
                    None,
                );
            };
            Discovery {
                issuer,
                authorization_endpoint,
                token_endpoint,
                jwks_uri,
                userinfo_endpoint: text("userinfo_endpoint"),
            }
        }
    };

    let jwks = match client.get_json(&discovery.jwks_uri).await {
        Ok(document) => document,
        Err(error) => {
            return (
                "failed",
                format!("the discovery document is fine, but its signing keys are not readable: {error}"),
                Some(endpoints_json(&discovery)),
            );
        }
    };
    let keys = oidc::parse_jwks(&jwks);
    if keys.is_empty() {
        return (
            "failed",
            "the provider publishes no RSA signing key this platform can verify".to_owned(),
            Some(endpoints_json(&discovery)),
        );
    }

    let note = if secret_present {
        String::new()
    } else {
        " (the client secret is not defined in this installation, so a sign-in will be refused \
         until it is)"
            .to_owned()
    };
    (
        "ok",
        format!(
            "discovery answered and {} usable signing key(s) were published{note}",
            keys.len()
        ),
        Some(endpoints_json(&discovery)),
    )
}

/// SAML has no discovery: every endpoint is entered, so the test is a *parse* of what is
/// configured — which is exactly the failure an operator hits first (a certificate copied with
/// its BEGIN line missing, or a base64 blob that lost its wrapping).
fn test_saml(provider: &AuthProvider, secret_present: bool) -> (&'static str, String, Option<Value>) {
    let text = |field: &str| {
        provider
            .config
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(issuer), Some(audience), Some(certificate)) =
        (text("issuer"), text("audience"), text("certificate_pem"))
    else {
        return (
            "failed",
            "a SAML provider needs `issuer`, `audience` and `certificate_pem` in its \
             configuration"
                .to_owned(),
            None,
        );
    };

    // A configuration is proven by building a *response* and asking the real verifier to read it:
    // that exercises the same parser a real assertion goes through, so a certificate this
    // platform cannot use fails here and not at the first sign-in.
    let config = omnion_identity::sso::saml::SamlConfig {
        issuer: issuer.clone(),
        audience: audience.clone(),
        certificate_pem: certificate,
        email_attribute: text("email_attribute").unwrap_or_else(|| "email".to_owned()),
        group_attribute: text("group_attribute"),
        display_name_attribute: text("display_name_attribute"),
    };
    let probe = format!(
        "<samlp:Response xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\">\
         <saml:Assertion xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\"/></samlp:Response>"
    );
    let parsed = omnion_identity::sso::saml::verify_response(&probe, &config);
    let certificate_usable = match &parsed {
        // The probe carries no signature, so the refusal names the *signature* check rather than
        // the certificate. Anything else would be a real parse failure of the configuration.
        Err(error) => {
            let message = error.to_string();
            !message.contains("not signed") && !message.contains("no assertion")
        }
        Ok(_) => true,
    };

    if !certificate_usable {
        return (
            "failed",
            format!("the configured certificate could not be read: {}", describe(&parsed)),
            None,
        );
    }

    let note = if secret_present {
        String::new()
    } else {
        " (SAML usually needs no client secret — the certificate is the credential)"
            .to_owned()
    };
    (
        "ok",
        format!("the assertion reader accepts this configuration{note}"),
        Some(json!({ "issuer": issuer, "audience": audience })),
    )
}

/// The error text of a probe result, for the message.
fn describe<T>(result: &Result<T, omnion_identity::IdentityError>) -> String {
    match result {
        Ok(_) => "it parsed".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// The endpoints the panel shows next to a successful test.
fn endpoints_json(discovery: &Discovery) -> Value {
    json!({
        "issuer": discovery.issuer,
        "authorization_endpoint": discovery.authorization_endpoint,
        "token_endpoint": discovery.token_endpoint,
        "jwks_uri": discovery.jwks_uri,
        "userinfo_endpoint": discovery.userinfo_endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_is_refused_rather_than_guessed() {
        assert!(ProviderKind::parse("oidc").is_ok());
        assert!(ProviderKind::parse("saml").is_ok());
        assert!(ProviderKind::parse("OAuth2").is_err());
        assert!(ProviderKind::parse("").is_err());
    }

    #[test]
    fn a_provider_without_a_secret_reference_reads_as_absent_rather_than_broken() {
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "public".into(),
            kind: ProviderKind::Oidc,
            name: "Public client".into(),
            config: json!({}),
            secret_ref: Some("  ".into()),
            scopes: vec!["openid".into()],
            group_claim: None,
            default_role_id: None,
            jit_enabled: false,
            enabled: false,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let body = ProviderBody::new(&provider, 7, None);
        assert!(!body.secret_present);
        assert_eq!(body.sign_in_count, 7);
    }

    #[test]
    fn a_defined_secret_is_reported_present_without_its_value_leaving_the_row() {
        // The body carries the *name* and a boolean; there is no field a secret could ride in.
        let provider = AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "confidential".into(),
            kind: ProviderKind::Oidc,
            name: "Confidential client".into(),
            config: json!({}),
            secret_ref: Some("OMNION_SSO_TEST_PRESENT".into()),
            scopes: vec![],
            group_claim: None,
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let body = ProviderBody::new(&provider, 0, None);
        assert_eq!(body.secret_ref.as_deref(), Some("OMNION_SSO_TEST_PRESENT"));
        assert!(!body.secret_present, "the variable is not set in this process");
        let encoded = serde_json::to_string(&body).expect("the body serializes");
        assert!(!encoded.contains("client_secret"));
    }

    #[test]
    fn the_event_page_is_clamped_to_a_sane_window() {
        for (asked, expected) in [(0, 1), (5, 5), (10_000, MAX_EVENT_PAGE)] {
            assert_eq!(asked.clamp(1, MAX_EVENT_PAGE), expected);
        }
    }
}
