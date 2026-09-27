//! `/api/v1/auth/sso` — the browser half of enterprise sign-in (REQ-006, slice 4b-2;
//! docs/07-IAM.md §11).
//!
//! Four handlers make up the round trip: `providers` lists what a person may sign in with, `start`
//! sends the browser to the provider, `callback` (and its posted twin `saml/callback`) receives
//! what comes back and turns it into a session. None carries a permission guard, because none has
//! a session — that is the point of a sign-in route, and it is also why each one re-derives its
//! own trust:
//!
//! * **A challenge, not a session.** `start` writes a single-use, hashed, ten-minute challenge
//!   bound to one provider; the callback presents nothing else. A callback with no live challenge
//!   is refused *before* any signature is examined, so a replayed, forged or cross-provider
//!   callback costs an attacker nothing — we never get far enough to look at a signature.
//! * **The organization comes from the challenge.** The slug is resolved inside the installation's
//!   organization, and the challenge row carries the organization the sign-in started in, so a
//!   callback cannot be aimed at a second tenant.
//! * **Every refusal is recorded.** Each attempt writes an `auth_provider_events` row with a
//!   machine-readable reason, so "why could that person not get in" is a screen question rather
//!   than an archaeology exercise.
//!
//! The verification itself is not here: `crates/identity/src/sso` proved the signature, the
//! issuer, the audience, the window and the nonce, and reduced the result to one [`Identity`].
//! This module's job is the HTTP, the session and the claim → role binding.

use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::header::{CONTENT_TYPE, LOCATION, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::sso::challenges::{self, SsoChallenge};
use omnion_identity::sso::claims::{self, Identity};
use omnion_identity::sso::oidc::{self, Discovery, HttpClient, MetadataCache, VerifiedAssertion};
use omnion_identity::sso::providers::{self, AuthProvider, ProviderKind};
use omnion_identity::sso::saml::{self, SamlConfig};
use omnion_identity::sso::provisioning::{self, ProvisionOutcome};
use omnion_identity::security;
use omnion_permissions::bindings;
use omnion_permissions::model::Scope;
use omnion_permissions::roles as role_store;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::auth::start_session;
use crate::routes::iam::record;
use crate::state::AppState;

/// The panel paths a sign-in may return to.
///
/// The `return_to` a challenge carries is resolved against this list, so a crafted value cannot
/// turn the callback into an open redirect — and the list is deliberately short: the sign-in
/// returns to the panel, never to a site's own content.
const RETURN_TO_PATHS: [&str; 4] = ["/", "/analytics", "/media", "/sites"];

/// The discovery/key cache, shared by every sign-in of the process.
fn metadata_cache() -> &'static Arc<MetadataCache> {
    static CACHE: OnceLock<Arc<MetadataCache>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(MetadataCache::new()))
}

/// The HTTP client, built once. A sign-in that waits a minute for metadata has already failed.
fn http_client() -> &'static HttpClient {
    static CLIENT: OnceLock<HttpClient> = OnceLock::new();
    CLIENT.get_or_init(HttpClient::new)
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Query of the sign-in start and of the public provider list.
#[derive(Debug, Deserialize)]
pub struct SsoQuery {
    /// Panel path to return to after the sign-in.
    #[serde(default)]
    pub return_to: Option<String>,
}

/// Query of the SAML relay page.
///
/// `state` is the challenge `start` issued. It is *not* read from the query of the same name on
/// any other route: the OIDC flows carry their state to the provider, not to us, so this is the
/// only place a SAML `RelayState` is introduced.
///
/// `return_to` is accepted and ignored. The redirect that reaches this page carries it, and
/// dropping it here would make the URL look lossy; the panel path the callback honours is the one
/// stored on the challenge, so reading it from the query would only add a way to be tampered with.
#[derive(Debug, Deserialize)]
pub struct SamlPageQuery {
    /// Panel path to return to after the sign-in (accepted, not used — see above).
    #[serde(default)]
    #[allow(dead_code)]
    pub return_to: Option<String>,
    /// The challenge this page must hand back to the callback.
    #[serde(default)]
    pub state: Option<String>,
}

/// The query a provider sends back on the OIDC/OAuth2 callback.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    /// The state this server issued.
    #[serde(default)]
    pub state: String,
    /// The authorization code.
    #[serde(default)]
    pub code: String,
    /// The `id_token`, when the provider sent one instead of a code.
    #[serde(default)]
    pub id_token: Option<String>,
    /// An error the provider reported instead of a code.
    #[serde(default)]
    pub error: Option<String>,
    /// The provider's own description of that error.
    #[serde(default)]
    pub error_description: Option<String>,
}

/// The posted form of a SAML response.
#[derive(Debug, Deserialize)]
pub struct SamlForm {
    /// The `SAMLResponse` field (base64).
    #[serde(rename = "SAMLResponse")]
    pub saml_response: String,
    /// The `RelayState` this server issued.
    #[serde(rename = "RelayState", default)]
    pub relay_state: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The providers a person may sign in with, for the sign-in screen.
///
/// Deliberately thin: slug, label, kind and the start URL, nothing else. A sign-in screen that
/// rendered a provider's configuration would be leaking it to anybody who can reach `/login`.
pub async fn list_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SsoQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_for_request(state.db().pool(), &headers).await?;
    let list = providers::list_enabled(state.db().pool(), organization_id).await?;

    let rows = list
        .iter()
        .map(|provider| {
            json!({
                "slug": provider.slug,
                "name": provider.name,
                "kind": provider.kind.as_str(),
                "start_url": format!("/api/v1/auth/sso/{}/start", provider.slug),
            })
        })
        .collect::<Vec<_>>();

    let _ = query;
    Ok(Json(
        json!({ "organization_id": organization_id, "providers": rows }),
    ))
}

/// Begin a sign-in: write the challenge and redirect the browser to the provider.
///
/// SAML gets a `GET` to the generated panel page (the browser posts the assertion straight back to
/// us, so there is no authorization endpoint to send anybody to); the `code` flows get the
/// authorization URL with `state`, PKCE and the redirect URI. The challenge's `state` is hashed
/// at rest and the PKCE verifier never leaves the server, so a browser that edits either of them
/// changes nothing the callback trusts.
pub async fn start(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    client: ClientAddress,
    Query(query): Query<SsoQuery>,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let organization_id = organization_for_request(pool, &headers).await?;
    let provider = live_provider(
        pool,
        organization_id,
        &slug,
        client.as_text(),
        user_agent(&headers),
    )
    .await?;

    let return_to = sanitize_return_to(query.return_to.as_deref());
    let flow = oidc::flow_of(provider.kind);
    let issued = challenges::issue(
        pool,
        provider.id,
        provider.organization_id,
        flow,
        &return_to,
    )
    .await?;

    // Opportunistic cleanup: a challenge nobody finished is dead weight, and this is the one
    // moment every sign-in passes through.
    if let Err(error) = challenges::purge_expired(pool).await {
        tracing::warn!(error = %error, "expired sign-in challenges could not be purged");
    }

    match provider.kind {
        ProviderKind::Saml => Ok(redirect(&format!(
            "/api/v1/auth/sso/{slug}/saml?return_to={return_to}&state={}",
            issued.state
        ))),
        ProviderKind::Oidc | ProviderKind::Oauth2 => {
            let discovery = discovery_for(&provider).await?;
            let client_id = client_id(&provider)?;
            let mut url = Url::parse(&discovery.authorization_endpoint).map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "provider_misconfigured",
                    format!("the provider's authorization endpoint is not a URL: {error}"),
                )
            })?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("response_type", "code");
                pairs.append_pair("client_id", &client_id);
                pairs.append_pair("redirect_uri", &redirect_uri(&provider)?);
                pairs.append_pair("state", &issued.state);
                if let Some(verifier) = issued.challenge.code_verifier.as_deref() {
                    pairs.append_pair("code_challenge", &oidc::pkce_challenge(verifier));
                    pairs.append_pair("code_challenge_method", "S256");
                }
                for scope in effective_scopes(&provider) {
                    pairs.append_pair("scope", &scope);
                }
            }
            tracing::info!(provider = %provider.slug, "sign-in started");
            Ok(redirect(url.as_str()))
        }
    }
}

/// The panel page that posts a SAML assertion back to the callback.
///
/// A SAML provider authenticates the *browser* and posts the assertion to our assertion consumer
/// service (ACS) URL, so there has to be a page on our side that does the POST. The relay state
/// is the challenge's own `state`, carried through the provider untouched — `start` puts it in
/// this page's URL, and the page hands it on as the form's `RelayState`.
///
/// Both values are HTML-escaped before they are written into the document. A `state` this server
/// issued is base64url, but a `return_to` is operator- and browser-supplied, and neither is
/// allowed to close the attribute it sits in.
pub async fn saml_page(
    Path(slug): Path<String>,
    Query(query): Query<SamlPageQuery>,
) -> Response {
    // The relay page is reached from `start`, which already put the challenge in the URL. The
    // `return_to` is *not* read here on purpose: the page is a transport for the challenge, and
    // the panel path the callback honours is the one on the challenge row, read back when it is
    // claimed — so editing the URL here cannot retarget the sign-in.
    let state = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(html_escape);
    let action = html_escape(&format!("/api/v1/auth/sso/{slug}/callback"));
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Signing you in</title>
  <meta name="robots" content="noindex" />
  <meta name="referrer" content="same-origin" />
</head>
<body>
  <h1 id="sso-title">Signing you in</h1>
  <p id="sso-status">Handing the assertion back to the platform.</p>
  <form id="sso-form" method="post" action="{action}">
    <input type="hidden" name="RelayState" value="{}" />
    <noscript>
      <button type="submit" id="sso-continue">Continue</button>
    </noscript>
  </form>
</body>
</html>"#,
        state.as_deref().unwrap_or("")
    );
    // The page is the transport for the challenge and nothing else: the return path the callback
    // honours is the one on the challenge row, read back when it is claimed, so a browser cannot
    // talk the callback into a different destination by editing the form.
    let script = r#"<script>document.getElementById('sso-form').submit();</script>"#;
    (
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        format!("{html}{script}"),
    )
        .into_response()
}

/// Escape the five characters that can end an HTML attribute or start a tag.
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The OIDC/OAuth2 callback.
pub async fn callback(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
    client: ClientAddress,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let organization_id = organization_for_request(pool, &headers).await?;
    let provider = providers::find_provider_by_slug(pool, organization_id, &slug)
        .await?
        .ok_or_else(provider_unknown)?;

    // A provider that reports its own error is not a failure of ours: the person was told why, and
    // the reason is recorded before anything else is attempted.
    if let Some(code) = query.error.as_deref() {
        log_event(
            pool,
            &provider,
            None,
            None,
            "refused",
            Some(&format!("provider_error:{code}")),
            &[],
            client.as_text(),
            user_agent(&headers),
        )
        .await;
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "provider_error",
            query
                .error_description
                .clone()
                .unwrap_or_else(|| format!("the provider refused the sign-in ({code})")),
        ));
    }

    let challenge = claim_challenge(
        pool,
        &provider,
        &query.state,
        client.as_text(),
        user_agent(&headers),
    )
    .await?;

    if !provider.enabled {
        challenges::consume(pool, challenge.id).await?;
        return Err(provider_disabled());
    }

    let identity = match resolve_code_flow(&provider, &challenge, &query).await {
        Ok(identity) => identity,
        Err(error) => {
            // The challenge is spent whatever happened: a failed attempt must not be retryable
            // with the same code, or the callback becomes an oracle.
            challenges::consume(pool, challenge.id).await?;
            log_event(
                pool,
                &provider,
                None,
                None,
                "refused",
                Some(error.code()),
                &[],
                client.as_text(),
                user_agent(&headers),
            )
            .await;
            return Err(error);
        }
    };

    challenges::consume(pool, challenge.id).await?;
    finish_sign_in(
        &state,
        &provider,
        identity,
        &challenge.return_to,
        headers,
        client,
    )
    .await
}

/// The SAML callback: a posted assertion.
pub async fn saml_callback(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    client: ClientAddress,
    Form(form): Form<SamlForm>,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let organization_id = organization_for_request(pool, &headers).await?;
    let provider = providers::find_provider_by_slug(pool, organization_id, &slug)
        .await?
        .ok_or_else(provider_unknown)?;

    let challenge = claim_challenge(
        pool,
        &provider,
        &form.relay_state,
        client.as_text(),
        user_agent(&headers),
    )
    .await?;

    if !provider.enabled {
        challenges::consume(pool, challenge.id).await?;
        return Err(provider_disabled());
    }

    let document = decode_saml(&form.saml_response)?;
    let config = SamlConfig {
        issuer: config_text(&provider, "issuer").unwrap_or_default(),
        audience: config_text(&provider, "audience").unwrap_or_default(),
        certificate_pem: config_text(&provider, "certificate_pem").unwrap_or_default(),
        email_attribute: config_text(&provider, "email_attribute").unwrap_or_else(|| "email".into()),
        group_attribute: config_text(&provider, "group_attribute"),
        display_name_attribute: config_text(&provider, "display_name_attribute"),
    };

    let assertion = match saml::verify_response(&document, &config) {
        Ok(assertion) => assertion,
        Err(error) => {
            challenges::consume(pool, challenge.id).await?;
            let reason = error.to_string();
            log_event(
                pool,
                &provider,
                None,
                None,
                "refused",
                Some(&reason),
                &[],
                client.as_text(),
                user_agent(&headers),
            )
            .await;
            return Err(ApiError::bad_request("saml_refused", reason));
        }
    };

    challenges::consume(pool, challenge.id).await?;

    finish_sign_in(
        &state,
        &provider,
        Identity {
            subject: assertion.subject_id,
            email: assertion.email,
            display_name: assertion.display_name,
            groups: assertion.groups,
            attributes: assertion.attributes,
        },
        &challenge.return_to,
        headers,
        client,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The shared tail: an identity becomes a session
// ---------------------------------------------------------------------------------------------

/// Provision (or find) the account, apply the mapped roles, and open the session.
///
/// Everything that can fail from here on is recorded with its own reason: a sign-in that fails
/// *after* the assertion was verified is a support question, and "it didn't work" is not an answer.
async fn finish_sign_in(
    state: &AppState,
    provider: &AuthProvider,
    identity: Identity,
    return_to: &str,
    headers: HeaderMap,
    client: ClientAddress,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let ip_address = client.as_text();
    let agent = user_agent(&headers);

    let provisioned = match provisioning::provision(pool, provider, &identity).await {
        Ok(provisioned) => provisioned,
        Err(error) => {
            log_event(
                pool,
                provider,
                None,
                Some(&identity.subject),
                "refused",
                Some(&error.to_string()),
                &[],
                ip_address.clone(),
                agent.clone(),
            )
            .await;
            return Err(ApiError::forbidden(
                "provisioning_refused",
                "this provider is not allowed to create an account for you",
            ));
        }
    };
    let user = provisioned.user;

    // A deactivated account stays deactivated: a provider sign-in must never undo an
    // administrator's decision, however convincing the assertion was.
    if !user.is_active() {
        log_event(
            pool,
            provider,
            Some(user.id),
            Some(&identity.subject),
            "refused",
            Some("account_disabled"),
            &[],
            ip_address.clone(),
            agent.clone(),
        )
        .await;
        return Err(ApiError::forbidden(
            "account_disabled",
            "this account is not active",
        ));
    }

    // The IP policy is the organization's, and a provider sign-in is a sign-in: a denied address
    // stays denied however the identity was proved.
    if let Some(organization_id) = user.organization_id
        && let IpVerdict::Denied { reason, rule } = address_verdict(pool, organization_id, ip_address.as_deref().unwrap_or_default()).await?
    {
        log_event(
            pool,
            provider,
            Some(user.id),
            Some(&identity.subject),
            "refused",
            Some("address_blocked"),
            &[],
            ip_address.clone(),
            agent.clone(),
        )
        .await;
        return Err(ApiError::forbidden(
            "address_blocked",
            "sign-ins from this address are refused by the security policy",
        )
        .with_details(json!({ "reason": reason, "rule": rule })));
    }

    provisioning::touch_account(pool, user.id, provider, &identity).await?;
    let roles = apply_mapped_roles(pool, provider, &identity, user.organization_id, user.id).await?;

    log_event(
        pool,
        provider,
        Some(user.id),
        Some(&identity.subject),
        provisioned.outcome.as_str(),
        Some(provisioned.outcome.as_str()),
        &roles,
        ip_address.clone(),
        agent.clone(),
    )
    .await;
    providers::touch_provider(pool, provider.id).await?;

    record(
        state,
        NewAuditEntry::by_user(user.id, "iam.sso_sign_in")
            .target("auth_provider", provider.id.to_string())
            .metadata(json!({
                "slug": provider.slug,
                "kind": provider.kind.as_str(),
                "external_subject": identity.subject,
                "roles_applied": roles,
                "outcome": provisioned.outcome.as_str(),
            }))
            .ip_address(ip_address.clone())
            .organization(Some(provider.organization_id)),
    )
    .await?;

    if provisioned.outcome == ProvisionOutcome::Created {
        // A provisioned account is a fact the rest of the platform reacts to: `user.created` is
        // what gives it the default member binding and a personal group, so a directory-driven
        // join looks exactly like a self-service one to everything downstream.
        let _ = bus::emit(
            pool,
            NewEvent::new("user.created")
                .organization(Some(provider.organization_id))
                .actor(Some(user.id))
                .payload(json!({
                    "user_id": user.id,
                    "email": user.email,
                    "via": "sso",
                    "provider": provider.slug,
                })),
        )
        .await;
        let _ = bus::emit(
            pool,
            NewEvent::new("iam.user_provisioned")
                .organization(Some(provider.organization_id))
                .actor(Some(user.id))
                .payload(json!({
                    "user_id": user.id,
                    "provider_id": provider.id,
                    "slug": provider.slug,
                    "roles_applied": roles,
                })),
        )
        .await;
    }

    let response = start_session(
        state,
        &user,
        agent,
        ip_address.clone(),
        vec![provider.kind.as_str().to_owned()],
    )
    .await?;

    // The JSON body of a session sign-in is not what a browser needs: it needs to be *at* the
    // panel. The session cookie travels on the response untouched, so the redirect lands the
    // person already signed in.
    Ok(with_location(response, &panel_path(return_to)))
}

// ---------------------------------------------------------------------------------------------
// The `code` flow
// ---------------------------------------------------------------------------------------------

/// Exchange the code, verify the token, and reduce it to one identity.
async fn resolve_code_flow(
    provider: &AuthProvider,
    challenge: &SsoChallenge,
    query: &CallbackQuery,
) -> Result<Identity, ApiError> {
    let discovery = discovery_for(provider).await?;

    // An implicit flow posts the token itself, so the code exchange is skipped — but the
    // signature and every registered claim are checked exactly the same way, and PKCE does not
    // apply because there was no code to bind it to.
    let verified = if let Some(token) = query
        .id_token
        .as_deref()
        .filter(|value| !value.is_empty())
        && query.code.is_empty()
    {
        // An implicit flow has no code, so there is no code binding to check at all.
        verify_token(token, &discovery, provider, None).await?
    } else {
        if query.code.is_empty() {
            return Err(ApiError::bad_request(
                "missing_code",
                "the provider sent neither a code nor a token",
            ));
        }
        let token = exchange_code(provider, &discovery, &query.code, challenge).await?;
        let id_token = token
            .get("id_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let access_token = token
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // A token issued *for this code* carries that code's hash (OIDC Core §3.1.3.6). The check is
        // the second half of the code binding — the first is PKCE, which the token endpoint
        // enforces when we send the verifier — and a provider that omits `c_hash` is not refused
        // for it: the claim is a RECOMMENDED, and an optional claim cannot be a mandatory rule.
        let verified = verify_token(&id_token, &discovery, provider, Some(&query.code)).await?;
        // A generic OAuth2 provider may send no ID token; the userinfo endpoint is then the
        // identity, and it is read with the access token rather than the (absent) ID token.
        if verified.claims.get("sub").is_none()
            && let (Some(access), Some(userinfo)) = (access_token, discovery.userinfo_endpoint.as_ref())
        {
            return fetch_userinfo(userinfo, &access).await;
        }
        verified
    };

    identity_from_verified(provider, verified)
}

/// POST the code at the token endpoint and read the token response.
///
/// A confidential client authenticates with Basic; a public client authenticates with PKCE alone,
/// which is why the secret is optional all the way down here — and why a missing secret is
/// reported as a *configuration* problem rather than a signing failure at the provider.
async fn exchange_code(
    provider: &AuthProvider,
    discovery: &Discovery,
    code: &str,
    challenge: &SsoChallenge,
) -> Result<Value, ApiError> {
    let client_id = client_id(provider)?;
    let secret = provisioning::resolve_client_secret(provider)
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "provider_misconfigured",
                error.to_string(),
            )
        })?;

    let mut form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={client_id}",
        url_encode(code),
        url_encode(&redirect_uri(provider)?),
    );
    if let Some(verifier) = challenge.code_verifier.as_deref() {
        form.push_str(&format!("&code_verifier={}", url_encode(verifier)));
    }

    let answer = match secret.as_deref() {
        Some(secret) => {
            http_client()
                .post_form_with_basic(&discovery.token_endpoint, &form, &client_id, secret)
                .await
        }
        None => http_client().post_form(&discovery.token_endpoint, &form).await,
    };
    answer.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "token_exchange_failed",
            format!("the provider refused the code exchange: {error}"),
        )
    })
}

/// Verify a token's signature against the published keys, then its registered claims.
///
/// `code` is the authorization code this sign-in exchanged, when there was one. It is what the
/// token's `c_hash` is checked against; an implicit flow has no code and therefore no code
/// binding. The PKCE verifier is deliberately *not* a parameter: it is proven at the token
/// endpoint, which is where the verifier is sent and where a provider that does not accept it
/// refuses the exchange. Re-checking it here — or worse, comparing it against a claim the
/// provider cannot compute — would be a second, weaker copy of a guarantee that already holds.
async fn verify_token(
    token: &str,
    discovery: &Discovery,
    provider: &AuthProvider,
    code: Option<&str>,
) -> Result<VerifiedAssertion, ApiError> {
    if token.is_empty() {
        return Err(ApiError::bad_request(
            "missing_token",
            "the provider sent no identity token",
        ));
    }

    let cache = metadata_cache();
    let key = format!("{}:jwks", provider.id);
    let jwks = match cache.get(&key) {
        Some(document) => document,
        None => {
            let document = http_client()
                .get_json(&discovery.jwks_uri)
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "provider_unreachable",
                        format!("the provider's signing keys are not readable: {error}"),
                    )
                })?;
            cache.put(&key, document.clone());
            document
        }
    };

    let header = oidc::JwtHeader::parse(token)
        .map_err(|error| ApiError::bad_request("token_refused", error.to_string()))?;

    if let Err(error) = oidc::verify_signature(token, &header, &oidc::parse_jwks(&jwks)) {
        // A rotated key is the one signature failure that is not an attack: forget the cache and
        // try once more against whatever the provider publishes now, before refusing.
        if let Ok(fresh) = http_client().get_json(&discovery.jwks_uri).await {
            if oidc::verify_signature(token, &header, &oidc::parse_jwks(&fresh)).is_ok() {
                cache.put(&key, fresh);
                return verify_claims_only(token, discovery, provider, code);
            }
        }
        cache.invalidate(&key);
        return Err(ApiError::bad_request("token_refused", error.to_string()));
    }

    verify_claims_only(token, discovery, provider, code)
}

/// The registered-claim half, once the signature has held.
fn verify_claims_only(
    token: &str,
    discovery: &Discovery,
    provider: &AuthProvider,
    code: Option<&str>,
) -> Result<VerifiedAssertion, ApiError> {
    let claims = oidc::decode_claims(token)
        .map_err(|error| ApiError::bad_request("token_refused", error.to_string()))?;

    // The token is bound to *this* sign-in by the `c_hash` of the code it was issued for
    // (OIDC Core §3.1.3.6). A hash for a different code means the token was minted for somebody
    // else's sign-in — a refusal. A *missing* hash is not: the claim is a RECOMMENDED, and plenty
    // of providers omit it, so its absence is not evidence of an attack and the PKCE binding the
    // token endpoint already enforced stands on its own.
    if let Some(code) = code.map(str::trim).filter(|value| !value.is_empty())
        && let Some(hash) = claims.values.get("c_hash").and_then(Value::as_str)
        && hash != oidc::code_hash(code)
    {
        return Err(ApiError::bad_request(
            "token_refused",
            "the token was not issued for this sign-in",
        ));
    }

    oidc::verify_claims(
        &claims,
        &discovery.issuer,
        &client_id(provider)?,
        None,
        OffsetDateTime::now_utc().unix_timestamp(),
    )
    .map_err(|error| ApiError::bad_request("token_refused", error.to_string()))?;

    let subject = claims
        .values
        .get("sub")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("token_refused", "the token names no subject"))?
        .to_owned();
    Ok(VerifiedAssertion {
        claims: claims.values,
        subject,
    })
}

/// Read the userinfo endpoint of a provider that sends no ID token.
async fn fetch_userinfo(endpoint: &str, access_token: &str) -> Result<Identity, ApiError> {
    let document = http_client()
        .get_json_with_bearer(endpoint, access_token)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                format!("the provider's userinfo endpoint did not answer: {error}"),
            )
        })?;

    let claims = document.as_object().cloned().ok_or_else(|| {
        ApiError::bad_request("token_refused", "the userinfo answer is not an object")
    })?;
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    Ok(VerifiedAssertion { claims, subject }
        .pipe(identity_from_claims_only)
        .map_err(|error| ApiError::bad_request("claims_refused", error.to_string()))?)
}

/// Reduce a verified assertion to the protocol-neutral identity.
fn identity_from_verified(
    provider: &AuthProvider,
    verified: VerifiedAssertion,
) -> Result<Identity, ApiError> {
    let email_claim = config_text(provider, "email_claim");
    claims::identity_from_claims(
        &verified.claims,
        email_claim.as_deref(),
        provider.group_claim.as_deref(),
    )
    .map_err(|error| ApiError::bad_request("claims_refused", error.to_string()))
}

/// The claim reduction of [`identity_from_verified`] without a provider row.
fn identity_from_claims_only(verified: VerifiedAssertion) -> Result<Identity, omnion_identity::IdentityError> {
    claims::identity_from_claims(&verified.claims, None, None)
}

/// A tiny `pipe`, so the userinfo branch reads as one expression.
trait Pipe: Sized {
    /// Apply `f` to `self`.
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}

// ---------------------------------------------------------------------------------------------
// Role mapping
// ---------------------------------------------------------------------------------------------

/// Apply the claim → role rules and answer the role names that were actually attached.
///
/// Every binding goes through `grant_if_missing`, so a person who signs in twice through the same
/// provider does not collect a second copy of the same role, and the rows are the ordinary ones
/// the panel's member tabs already show.
async fn apply_mapped_roles(
    pool: &PgPool,
    provider: &AuthProvider,
    identity: &Identity,
    organization_id: Option<Uuid>,
    user_id: Uuid,
) -> Result<Vec<String>, ApiError> {
    let Some(organization_id) = organization_id else {
        return Ok(Vec::new());
    };

    let mappings = claims::mappings_from_config(&provider.config)
        .map_err(|error| ApiError::bad_request("invalid_request", error.to_string()))?;
    let mut slugs = claims::resolve_roles(identity, &mappings);
    slugs.dedup();

    let mut applied: Vec<String> = Vec::new();
    for slug in slugs {
        // A role is found the way the rest of the platform finds one: this organization's own
        // role if it defines one, otherwise the platform's base role of that name. Asking for
        // *only* the organization's own role is what made a claim → role mapping silently do
        // nothing for every tenant — the base roles are seeded at platform scope, so a directory
        // mapping `editors` → `editor` matched nothing and the person signed in with no role and
        // no error, which is the worst possible outcome for a feature whose whole point is the
        // mapping.
        let role = match role_store::find_role_by_key(pool, Some(organization_id), &slug).await? {
            Some(role) => role,
            None => match role_store::find_role_by_key(pool, None, &slug).await? {
                Some(role) => role,
                None => {
                    // A rule naming a role nobody has must not fail the sign-in: the person still
                    // gets in, and the operator sees a warning instead of an outage.
                    tracing::warn!(
                        provider = %provider.slug,
                        role = %slug,
                        "a claim maps to a role this organization and the platform do not define"
                    );
                    continue;
                }
            },
        };
        if grant(pool, role.id, user_id, organization_id).await? {
            applied.push(slug);
        }
    }

    // The provider's own default role rides the same path, so "every sign-in gets Editor" is one
    // ordinary row rather than a special case in the sign-in code.
    if let Some(default_role) = claims::default_role_id(&provider.config, provider.default_role_id)
        && grant(pool, default_role, user_id, organization_id).await?
    {
        let name = role_store::find_role(pool, default_role)
            .await?
            .map_or_else(String::new, |role| role.key);
        if !name.is_empty() {
            applied.push(name);
        }
    }

    applied.dedup();
    Ok(applied)
}

/// Grant one role at organization scope unless the person already holds it.
async fn grant(
    pool: &PgPool,
    role_id: Uuid,
    user_id: Uuid,
    organization_id: Uuid,
) -> Result<bool, ApiError> {
    let binding = bindings::grant_if_missing(
        pool,
        omnion_permissions::model::NewBinding {
            role_id,
            user_id,
            scope: Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await?;
    Ok(binding.is_some())
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The organization a public sign-in is addressed to.
///
/// A sign-in has no session, so there is no caller's organization to read. The resolution is the
/// same one the public content surface uses (`crate::routes::public::resolve_site`): the browser's
/// own host answers for its site, and a site belongs to an organization. A single-organization
/// installation — the first-run case and every QA stack — needs no host at all.
///
/// What it must never do is *guess*: with several organizations and no way to tell them apart, a
/// guess would let a sign-in link for one tenant complete against another. So the answer is an
/// honest `501` naming the fix (register a domain), not a coin toss.
async fn organization_for_request(
    pool: &PgPool,
    headers: &HeaderMap,
) -> Result<Uuid, ApiError> {
    let organizations = sqlx::query_scalar::<_, Uuid>(
        "select id from organizations order by created_at limit 2",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;

    match organizations.as_slice() {
        [only] => Ok(*only),
        [] => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_organization",
            "this installation has no organization yet",
        )),
        // Several organizations: the host decides, exactly as it does for a rendered page.
        _ => {
            let host = headers
                .get("x-forwarded-host")
                .or_else(|| headers.get(header::HOST))
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(',').next().unwrap_or(value).trim())
                .map(|value| value.split(':').next().unwrap_or(value).to_owned())
                .map(|value| value.to_ascii_lowercase())
                .filter(|value| !value.is_empty());

            let Some(host) = host else {
                return Err(multi_organization());
            };
            let organization_id = sqlx::query_scalar::<_, Uuid>(
                "select s.organization_id from sites s \
                 join site_domains d on d.site_id = s.id \
                 where lower(d.host) = $1 and s.organization_id is not null limit 1",
            )
            .bind(&host)
            .fetch_optional(pool)
            .await
            .map_err(|error| ApiError::from(omnion_identity::IdentityError::Database(error)))?;

            organization_id.ok_or_else(multi_organization)
        }
    }
}

/// The refusal a multi-organization installation answers with until a host is registered.
fn multi_organization() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "organization_required",
        "this installation holds several organizations; reach the panel on one of its own \
         hostnames so the sign-in knows which organization it is for",
    )
}

/// The enabled provider a slug names, or a refusal that says which of the two it was.
///
/// A disabled provider *is* an attempt worth recording: an operator who switched a provider off
/// and then sees "somebody tried to sign in with it" is looking at the one question the switch
/// raises. So this path writes an `auth_provider_events` row before refusing — a refusal is a
/// fact, not a silence.
async fn live_provider(
    pool: &PgPool,
    organization_id: Uuid,
    slug: &str,
    client: Option<String>,
    agent: Option<String>,
) -> Result<AuthProvider, ApiError> {
    let provider = providers::find_provider_by_slug(pool, organization_id, slug)
        .await?
        .ok_or_else(provider_unknown)?;
    if !provider.enabled {
        log_event(
            pool,
            &provider,
            None,
            None,
            "refused",
            Some("provider_disabled"),
            &[],
            client,
            agent,
        )
        .await;
        return Err(provider_disabled());
    }
    Ok(provider)
}

/// Claim the challenge a callback presents, recording the refusal when there is none.
async fn claim_challenge(
    pool: &PgPool,
    provider: &AuthProvider,
    state_value: &str,
    ip_address: Option<String>,
    agent: Option<String>,
) -> Result<SsoChallenge, ApiError> {
    match challenges::claim(pool, provider.id, state_value).await {
        Ok(challenge) => Ok(challenge),
        Err(error) => {
            log_event(
                pool,
                provider,
                None,
                None,
                "refused",
                Some("invalid_state"),
                &[],
                ip_address,
                agent,
            )
            .await;
            Err(ApiError::bad_request("invalid_state", error.to_string()))
        }
    }
}

/// The provider's discovery document, from the cache or from the network.
async fn discovery_for(provider: &AuthProvider) -> Result<Discovery, ApiError> {
    let cache = metadata_cache();
    let key = format!("{}:discovery", provider.id);

    if let Some(document) = cache.get(&key)
        && let Ok(discovery) = Discovery::from_value(&document)
    {
        return Ok(discovery);
    }

    let document = match config_text(provider, "issuer") {
        Some(issuer) => {
            let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
            http_client()
                .get_json(&url)
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "provider_unreachable",
                        format!("the provider's discovery document is not readable: {error}"),
                    )
                })?
        }
        // A provider that publishes no discovery document still has endpoints; they are entered
        // explicitly, so the "document" is assembled from the row rather than fetched.
        None => json!({
            "issuer": config_text(provider, "issuer").unwrap_or_default(),
            "authorization_endpoint": config_text(provider, "authorization_endpoint").unwrap_or_default(),
            "token_endpoint": config_text(provider, "token_endpoint").unwrap_or_default(),
            "jwks_uri": config_text(provider, "jwks_uri").unwrap_or_default(),
            "userinfo_endpoint": config_text(provider, "userinfo_endpoint"),
        }),
    };

    let discovery = Discovery::from_value(&document).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "provider_misconfigured",
            format!("the provider's discovery document is incomplete: {error}"),
        )
    })?;
    cache.put(&key, document);
    Ok(discovery)
}

/// A configured string, trimmed and non-empty.
fn config_text(provider: &AuthProvider, field: &str) -> Option<String> {
    provider
        .config
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The client id of a provider.
fn client_id(provider: &AuthProvider) -> Result<String, ApiError> {
    config_text(provider, "client_id").ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "provider_misconfigured",
            "the provider has no `client_id` in its configuration",
        )
    })
}

/// The redirect URI: what the provider sends the browser back to.
fn redirect_uri(provider: &AuthProvider) -> Result<String, ApiError> {
    if let Some(explicit) = config_text(provider, "redirect_uri") {
        return Ok(explicit);
    }
    let base = std::env::var("OMNION_PUBLIC_URL")
        .unwrap_or_else(|_| "http://localhost:3200".to_owned())
        .trim_end_matches('/')
        .to_owned();
    Ok(format!("{base}/api/v1/auth/sso/{}/callback", provider.slug))
}

/// The scopes a provider requests: its own, or the kind's defaults.
fn effective_scopes(provider: &AuthProvider) -> Vec<String> {
    if provider.scopes.is_empty() {
        providers::default_scopes(provider.kind)
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect()
    } else {
        provider.scopes.clone()
    }
}

/// A `return_to` reduced to a path this server produced.
fn sanitize_return_to(value: Option<&str>) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return "/".to_owned();
    };
    // Accepted only if it is one of ours: an absolute URL, a scheme-relative one or anything with
    // a `..` in it is refused outright rather than normalized into something safe-looking.
    if value.starts_with('/') && !value.starts_with("//") && !value.contains("..") {
        let path = value.split(['?', '#']).next().unwrap_or("/");
        if RETURN_TO_PATHS.contains(&path) {
            return path.to_owned();
        }
    }
    "/".to_owned()
}

/// The panel path a completed sign-in lands on.
fn panel_path(return_to: &str) -> String {
    let path = sanitize_return_to(Some(return_to));
    if path == "/" {
        "/".to_owned()
    } else {
        format!("/admin{path}")
    }
}

/// A redirect response.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
}

/// A response with a `Location` added, keeping its body and — crucially — its cookie.
fn with_location(response: Response, location: &str) -> Response {
    let (mut parts, body) = response.into_parts();
    if let Ok(value) = HeaderValue::from_str(location) {
        parts.headers.insert(LOCATION, value);
    }
    Response::from_parts(parts, body)
}

/// The refusal of an unknown provider slug.
fn provider_unknown() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "provider_not_found",
        "this installation has no sign-in provider with that name",
    )
}

/// The refusal of a provider an operator switched off.
fn provider_disabled() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "provider_disabled",
        "that sign-in provider is switched off",
    )
}

/// The verdict of the address lists for one request.
async fn address_verdict(
    pool: &PgPool,
    organization_id: Uuid,
    ip_address: &str,
) -> Result<IpVerdict, ApiError> {
    let policy = security::ensure_policy(pool, organization_id).await?;
    let parsed = ip_address.parse().ok();
    Ok(security::check_ip(&policy, parsed))
}

/// The outcome of an address check, re-declared so the handler above reads it without naming the
/// security module's own type at every call site.
type IpVerdict = security::IpVerdict;

/// Decode a base64 SAML response, refusing anything implausible before parsing.
fn decode_saml(encoded: &str) -> Result<String, ApiError> {
    if encoded.len() > saml::MAX_ASSERTION_BYTES {
        return Err(ApiError::bad_request(
            "saml_refused",
            "the assertion is implausibly large",
        ));
    }
    let bytes = base64_decode(encoded.trim())
        .ok_or_else(|| ApiError::bad_request("saml_refused", "the assertion is not base64"))?;
    if bytes.len() > saml::MAX_ASSERTION_BYTES {
        return Err(ApiError::bad_request(
            "saml_refused",
            "the assertion is implausibly large",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| ApiError::bad_request("saml_refused", "the assertion is not text"))
}

/// Percent-encode one form value.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Standard base64, tolerant of missing padding and of the URL-safe alphabet.
fn base64_decode(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let trimmed = value.trim();
    base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .ok()
}

/// The user agent of a request, for the log.
fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Write one `auth_provider_events` row.
///
/// The row is the whole support story for a sign-in, and it is deliberately narrow: an outcome, a
/// machine-readable reason, the external subject, the roles that were attached and where the
/// request came from. No claim payload, no token, no secret — the REQ's "never a secret" rule
/// applies here exactly as it does to the audit trail.
#[allow(clippy::too_many_arguments)]
async fn log_event(
    pool: &PgPool,
    provider: &AuthProvider,
    user_id: Option<Uuid>,
    external_subject: Option<&str>,
    outcome: &str,
    reason: Option<&str>,
    roles: &[String],
    ip_address: Option<String>,
    agent: Option<String>,
) {
    let result = sqlx::query(
        "insert into auth_provider_events \
             (provider_id, organization_id, user_id, external_subject, outcome, reason, \
              roles_applied, ip_address, user_agent) \
         values ($1, $2, $3, $4, $5, $6, $7, cast($8 as inet), $9)",
    )
    .bind(provider.id)
    .bind(provider.organization_id)
    .bind(user_id)
    .bind(external_subject)
    .bind(outcome)
    .bind(reason)
    .bind(roles)
    .bind(ip_address)
    .bind(agent)
    .execute(pool)
    .await;

    if let Err(error) = result {
        tracing::warn!(error = %error, "a provider sign-in could not be logged");
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    /// A provider row that names its group claim, so the group mapping is actually exercised.
    fn provider(kind: ProviderKind) -> AuthProvider {
        AuthProvider {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            slug: "okta".into(),
            kind,
            name: "Okta".into(),
            config: json!({ "client_id": "abc" }),
            secret_ref: None,
            scopes: vec![],
            group_claim: Some("groups".into()),
            default_role_id: None,
            jit_enabled: true,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_return_to_outside_the_known_paths_falls_back_to_the_root() {
        // The open-redirect guard: only a path this server produced survives.
        assert_eq!(sanitize_return_to(Some("/analytics")), "/analytics");
        assert_eq!(sanitize_return_to(Some("/media?tab=images")), "/media");
        assert_eq!(sanitize_return_to(None), "/");
        assert_eq!(sanitize_return_to(Some("")), "/");
        assert_eq!(sanitize_return_to(Some("//evil.example")), "/");
        assert_eq!(sanitize_return_to(Some("https://evil.example")), "/");
        assert_eq!(sanitize_return_to(Some("/admin/../etc")), "/");
        assert_eq!(sanitize_return_to(Some("/unknown/path")), "/");
    }

    #[test]
    fn a_completed_sign_in_lands_on_the_panel() {
        assert_eq!(panel_path("/"), "/");
        assert_eq!(panel_path("/analytics"), "/admin/analytics");
        assert_eq!(panel_path("https://evil.example"), "/");
    }

    #[test]
    fn form_values_are_percent_encoded_the_way_a_token_endpoint_expects() {
        assert_eq!(url_encode("abc123"), "abc123");
        assert_eq!(url_encode("a/b+c=d"), "a%2Fb%2Bc%3Dd");
        assert_eq!(url_encode("a b"), "a+b");
        assert_eq!(url_encode("~-._"), "~-._");
    }

    #[test]
    fn base64_reads_the_padded_unpadded_and_url_safe_alphabets() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        assert!(base64_decode("!!!not base64!!!").is_none());
    }

    #[test]
    fn a_saml_response_is_size_capped_before_it_is_decoded() {
        let huge = "A".repeat(saml::MAX_ASSERTION_BYTES + 1);
        let error = decode_saml(&huge).expect_err("an implausibly large assertion is refused");
        assert_eq!(error.code(), "saml_refused");
        assert!(decode_saml("!!!not base64!!!").is_err());
    }

    #[test]
    fn a_verified_assertion_reduces_to_an_identity() {
        let verified = VerifiedAssertion {
            claims: json!({
                "sub": "00u1",
                "email": "Person@Example.COM",
                "name": "Person",
                "groups": ["editors", "viewers"],
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
            subject: "00u1".into(),
        };
        let identity = identity_from_verified(&provider(ProviderKind::Oidc), verified)
            .expect("a complete assertion");
        assert_eq!(identity.email, "person@example.com");
        assert_eq!(identity.groups, vec!["editors", "viewers"]);
        assert_eq!(identity.display_name.as_deref(), Some("Person"));
    }

    #[test]
    fn a_token_without_an_email_is_refused_rather_than_provisioning_an_unreachable_account() {
        let verified = VerifiedAssertion {
            claims: json!({ "sub": "00u1", "preferred_username": "person" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            subject: "00u1".into(),
        };
        assert!(identity_from_verified(&provider(ProviderKind::Oidc), verified).is_err());
    }

    #[test]
    fn scopes_fall_back_to_the_kinds_defaults() {
        let mut row = provider(ProviderKind::Oidc);
        assert_eq!(
            effective_scopes(&row),
            vec!["openid".to_owned(), "profile".to_owned(), "email".to_owned()]
        );
        row.scopes = vec!["openid".into(), "groups".into()];
        assert_eq!(effective_scopes(&row), row.scopes);
    }

    #[test]
    fn a_provider_without_a_client_id_names_the_missing_field() {
        let mut row = provider(ProviderKind::Oidc);
        row.config = json!({});
        let error = client_id(&row).expect_err("no client id");
        assert_eq!(error.code(), "provider_misconfigured");
    }

    #[test]
    fn escaping_covers_everything_that_can_end_an_attribute() {
        assert_eq!(html_escape(r#"a"b'c<d>e&f"#), "a&quot;b&#39;c&lt;d&gt;e&amp;f");
        // A state we issued is base64url and passes through untouched — escaping a value that
        // cannot contain a dangerous character must not corrupt it either.
        assert_eq!(html_escape("aB3-_xyz"), "aB3-_xyz");
    }

    #[tokio::test]
    async fn the_saml_page_carries_the_challenge_and_not_the_return_path() {
        // The page exists to hand the challenge back to the callback. Carrying the return path
        // instead is what made every SAML sign-in fail with `invalid_state` before this was fixed,
        // because the callback claims a challenge from `RelayState` and a path is not one.
        let response = saml_page(
            Path("okta".to_owned()),
            Query(SamlPageQuery {
                return_to: Some("/media".to_owned()),
                state: Some("the-challenge".to_owned()),
            }),
        )
        .await;
        let (parts, body) = response.into_parts();
        assert_eq!(parts.status, StatusCode::OK);
        let page = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(page.contains(r#"value="the-challenge""#), "{page}");
        assert!(!page.contains(r#"value="/media""#), "{page}");
        assert!(page.contains(r#"action="/api/v1/auth/sso/okta/callback""#), "{page}");
    }

    #[tokio::test]
    async fn a_saml_page_asked_to_close_its_own_attribute_cannot() {
        // `state` is base64url when we issue it, but the route is public: a crafted value must not
        // be able to inject markup into the page the browser auto-submits.
        let response = saml_page(
            Path("okta".to_owned()),
            Query(SamlPageQuery {
                return_to: None,
                state: Some(r#"" autofocus onfocus="alert(1)" x="#.to_owned()),
            }),
        )
        .await;
        let (_, body) = response.into_parts();
        let page = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(!page.contains("onfocus=\"alert"), "{page}");
        assert!(page.contains("&quot;"), "{page}");
    }
}
