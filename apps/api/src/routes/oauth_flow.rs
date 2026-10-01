//! `/oauth/authorize` and `/oauth/token` — the two endpoints a third-party client calls
//! (docs/requests/REQ-033, slice 3c).
//!
//! # Why these are not in `developer_oauth.rs`
//!
//! The panel's handlers resolve the tenant from the *session*; these resolve it from the *app
//! row*. A single handler serving both would have to pick one answer to "who is this?", and the
//! tenant a write lands in would depend on which value arrived first. Keeping them apart means
//! the tenant for every statement in each file comes from one nameable place.
//!
//! # The four things this file refuses to do
//!
//! 1. **Redirect before validating.** [`authorize`](omnion_developer::authorize) checks the
//!    client, the grant, the redirect, the scopes and the PKCE challenge *in that order* and
//!    writes nothing. A refusal at this endpoint is a JSON error on a page, never a redirect to
//!    a URL the client supplied — except for the one case RFC 6749 §4.1.2.1 requires a redirect
//!    for, and even there the redirect only happens after the redirect URI has been proven
//!    registered, which is what makes it safe.
//! 2. **Issue a token without proving PKCE.** A code that carries a challenge cannot be
//!    redeemed without its verifier, so an intercepted code is inert. A code with *no* challenge
//!    may only be redeemed by a client that authenticated with its secret, and the
//!    `oauth_codes_challenge_is_whole` constraint is why half a challenge is not representable.
//! 3. **Widen the grant.** The token's scopes are the consented ones narrowed by what the token
//!    request asked for — never the app's registered list read fresh, and never the request's
//!    list taken whole. See [`resolve_token_scopes`](omnion_developer::oauth_flow::resolve_token_scopes).
//! 4. **Echo a credential.** Neither endpoint writes a client secret, a code or a verifier into
//!    a log line, an audit row or an error body. The audit metadata carries ids, counts and
//!    booleans; the `code` and the token are shown to the client and to nobody else.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header::LOCATION};
use axum::response::{IntoResponse, Response};
use omnion_developer::model_oauth::authorize;
use omnion_developer::oauth::{CODE_TTL_MINUTES, GrantType, hash_code, verify_code_verifier};
use omnion_developer::oauth_flow::{
    ACCESS_TOKEN_TTL_SECONDS, AuthorizeQuery, GrantProvenance, TokenErrorBody, TokenResponse,
    encode_state, hash_access_token, mint_access_token, mint_code, parse_authorize_query,
    redirect_with_code, resolve_token_scopes, token_looks_valid, usable_state,
};
use omnion_developer::store_oauth::{
    client_credentials, find_by_client_id, find_token, issue_code, issue_token, redeem_code,
    which_secret_matched,
};
use omnion_developer::{DeveloperError, store_oauth};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /oauth/token`.
///
/// `application/x-www-form-urlencoded` rather than JSON, because that is what RFC 6749 §4.1.3
/// specifies and what every OAuth client library sends. Axum's `Form` extractor handles it; the
/// `code_verifier` is optional because `client_credentials` requests carry one never.
#[derive(Debug, Default, Deserialize)]
pub struct TokenRequest {
    /// `authorization_code` or `client_credentials`.
    #[serde(default)]
    pub grant_type: String,
    /// The app's public identifier.
    #[serde(default)]
    pub client_id: String,
    /// The app's secret. Ignored — and rejected as present — for a public client.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The code being redeemed, for an authorization-code request.
    #[serde(default)]
    pub code: Option<String>,
    /// The PKCE verifier.
    #[serde(default)]
    pub code_verifier: Option<String>,
    /// The redirect URI, which must equal the one the code was issued for.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Space-delimited scopes, narrowing the grant.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Body of the consent POST — the second half of the authorization request.
///
/// A POST rather than a GET-with-`approve=true` because the decision is the platform's to record
/// and a GET is a link a browser can be sent to: prefetchers, crawlers and `Referer` headers all
/// follow GETs, and "somebody's browser approved this app" is not a decision that can be made by
/// accident. It also keeps the client id out of any URL, which is the other half of the reason.
#[derive(Debug, Deserialize)]
pub struct ConsentInput {
    /// The app being authorized, echoed from the screen.
    pub client_id: String,
    /// The redirect the screen was rendered for.
    pub redirect_uri: String,
    /// The client's opaque `state`, to be returned on the redirect.
    #[serde(default)]
    pub state: Option<String>,
    /// The PKCE challenge, echoed.
    #[serde(default)]
    pub code_challenge: Option<String>,
    /// The PKCE method, echoed.
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    /// Whether the user agreed. Explicit, and absent means "no".
    #[serde(default)]
    pub approve: bool,
}

/// What the consent screen renders.
#[derive(Debug, serde::Serialize)]
pub struct ConsentView {
    /// The app's name.
    pub app_name: String,
    /// Its description, if any.
    pub app_description: Option<String>,
    /// Its logo object key, if any.
    pub logo_object_key: Option<String>,
    /// The public identifier.
    pub client_id: String,
    /// The redirect this authorization will return to.
    pub redirect_uri: String,
    /// The scopes being granted, as the screen lists them.
    pub scopes: Vec<ScopeView>,
    /// Whether PKCE is protecting this flow, so the screen can say so.
    pub pkce: bool,
}

/// One scope on the consent screen.
#[derive(Debug, serde::Serialize)]
pub struct ScopeView {
    /// The permission key.
    pub key: String,
    /// The last segment, which is what a user recognises ("read pages", not
    /// "content.pages.read").
    pub label: String,
}

// ---------------------------------------------------------------------------------------------
// The authorization request
// ---------------------------------------------------------------------------------------------

/// `GET /oauth/authorize` — validate the request and show the consent screen.
///
/// Three outcomes, and the split between them is the security property:
///
/// * **Refused here, as JSON** — the client is unknown, not usable, does not hold the grant,
///   the redirect is not registered, or the scopes are wider than registered. A refusal is never
///   redirected anywhere, because the redirect URI is exactly the value under suspicion.
/// * **Redirected, per RFC 6749 §4.1.2.1** — the client and redirect are proven, but the request
///   is otherwise wrong (an unknown `response_type`, or a malformed PKCE pair). The user is sent
///   back to the app with an `error` in the query so the client can show a real message, and the
///   `state` goes with it so the client can match the response to the request.
/// * **The consent screen** — everything checked, nothing written. The code is minted on the
///   POST, and only on `approve`.
pub async fn authorize_start(
    State(state): State<AppState>,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Response, ApiError> {
    let request = parse_authorize_query(&query);
    let app = find_by_client_id(state.db().pool(), &request.client_id)
        .await
        .map_err(store_error)?
        .ok_or(DeveloperError::AppNotFound)?;

    // The order in `authorize` is the security property; all this does is turn its refusals into
    // the two answers above. The one distinction drawn here is *where* the answer goes, and it
    // needs one fact the crate deliberately does not decide: whether the redirect URI is
    // trustworthy yet. A refusal may only be redirected to a URI this app has registered, so
    // that check happens before the split.
    let redirect_is_ours =
        omnion_developer::oauth::check_redirect_uri(&request.redirect_uri, &app.redirect_uris)
            .is_ok();

    match authorize(&app, app.status, &request) {
        Ok(consent) => Ok(consent_screen(&consent, &query).into_response()),
        Err(error) => {
            // The specific reason goes to a **log**, never to the redirect. Two reasons, both
            // load-bearing: `UnknownGrantType` carries the client-supplied `response_type` in its
            // own message, and that message would ride in a `Location` header — into browser
            // history, proxy logs and the next `Referer`. And the borrow checker caught this:
            // `&error.to_string()` is a `String` where the signature demands a `&'static str`,
            // which is the compiler pointing at a rule the signature was written to enforce.
            //
            // The client gets a fixed `access_denied`, which is also what a caller should see: it
            // cannot act on "you sent grant type `passwrod`" beyond correcting a typo, and the
            // panel's own diagnostics are a developer-console problem rather than an OAuth
            // protocol one.
            tracing::debug!(
                client_id = %request.client_id,
                error = %error,
                "an authorization request was refused"
            );
            if redirect_is_ours {
                Ok(redirect_with_error(
                    &request.redirect_uri,
                    "access_denied",
                    "the authorization request was refused",
                    usable_state(query.state.as_ref()).as_deref(),
                )
                .into_response())
            } else {
                Err(error.into())
            }
        }
    }
}

/// The consent screen's HTML.
///
/// Server-rendered rather than a panel route on purpose. The screen is reached by a **browser
/// following a redirect from a third-party app**, so it must work with no panel navigation, no
/// client-side bundle that assumes the admin layout, and no assumption that the visitor knows they
/// are in a panel. It is a single self-contained page: no `next/navigation`, no fetch, nothing to
/// hydrate, so it cannot be reached in a half-loaded state.
fn consent_screen(
    consent: &omnion_developer::model_oauth::ConsentRequest,
    query: &AuthorizeQuery,
) -> Response {
    // **These three fields are HTML-escaped, not JSON-encoded.** That distinction is the whole
    // of a defect this function had: `serde_json::to_string` on a string containing a double
    // quote produces `\"`, and `\"` inside an HTML attribute does **not** escape the quote —
    // HTML uses `&quot;`. So a `state` of `"><script>alert(1)</script>` rendered as
    //
    //     <input ... name="state" value="\"><script>alert(1)</script>">
    //
    // where the `"` closes the attribute and the script tag is live markup in the approver's
    // session on the platform's own origin. JSON is a correct encoding for a JavaScript string
    // literal and a wrong one for an HTML attribute, and the two look similar enough that the
    // wrong one is easy to write.
    //
    // An absent value renders as an **empty string** rather than the text `null`, for a reason
    // that is about behaviour rather than looks: a hidden input whose value is the four
    // characters `null` posts the string "null", which `usable_state` accepts and echoes back to
    // the client as a `state` it never sent. Absence must be absence.
    let state_field = html_attribute(query.state.as_deref().unwrap_or_default());
    let challenge_field = html_attribute(query.code_challenge.as_deref().unwrap_or_default());
    let method_field = html_attribute(query.code_challenge_method.as_deref().unwrap_or_default());

    let scopes: Vec<ScopeView> = consent
        .scopes
        .iter()
        .map(|key| ScopeView {
            key: key.clone(),
            label: key
                .rsplit('.')
                .next()
                .unwrap_or(key)
                .replace(['.', '-'], " "),
        })
        .collect();
    let scopes_markup = if scopes.is_empty() {
        "<li class=\"scope\"><span class=\"label\">no scopes requested</span></li>".to_owned()
    } else {
        scopes
            .iter()
            .map(|scope| {
                format!(
                    "<li class=\"scope\"><code>{}</code><span class=\"label\">{}</span></li>",
                    escape_html(&scope.key),
                    escape_html(&scope.label)
                )
            })
            .collect()
    };

    let pkce_note = if consent.pkce {
        "<p class=\"note\">This request is protected by PKCE: the app must prove it holds a \
         secret it never sent here.</p>"
    } else {
        "<p class=\"note\">This request did not use PKCE, so the app's client secret is \
         required to exchange the code.</p>"
    };

    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="origin">
<title>Authorize {app}</title>
<style>
  :root {{ color-scheme: light dark; }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; font: 15px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
         background: #0b0b0f; color: #e8e8ee; display: flex; justify-content: center;
         padding: 32px 16px; }}
  main {{ width: 100%; max-width: 34rem; background: #15151b; border: 1px solid #2a2a35;
          border-radius: 14px; padding: 28px; }}
  h1 {{ font-size: 1.25rem; margin: 0 0 4px; }}
  .who {{ color: #a2a2b4; margin: 0 0 20px; }}
  .who code {{ color: #c9c9da; }}
  h2 {{ font-size: .8rem; text-transform: uppercase; letter-spacing: .07em; color: #8f8fa6;
        margin: 0 0 10px; font-weight: 600; }}
  ul {{ list-style: none; margin: 0 0 20px; padding: 0; }}
  .scope {{ display: flex; justify-content: space-between; gap: 12px; align-items: baseline;
            padding: 9px 12px; border: 1px solid #26262f; border-radius: 9px; margin-bottom: 7px; }}
  .scope code {{ font-size: .82rem; color: #dcdcea; word-break: break-all; }}
  .scope .label {{ color: #8f8fa6; font-size: .82rem; white-space: nowrap; }}
  .note {{ color: #8f8fa6; font-size: .85rem; margin: 0 0 20px; }}
  form {{ display: contents; }}
  .actions {{ display: flex; gap: 10px; }}
  button {{ flex: 1; font: inherit; font-weight: 600; padding: 11px 16px; border-radius: 9px;
            cursor: pointer; border: 1px solid transparent; }}
  .approve {{ background: #4f46e5; color: #fff; }}
  .approve:hover {{ background: #4338ca; }}
  .deny {{ background: transparent; color: #c8c8d6; border-color: #34343f; }}
  .deny:hover {{ background: #1d1d24; }}
  button:focus-visible {{ outline: 2px solid #818cf8; outline-offset: 2px; }}
  .redirect {{ color: #6f6f85; font-size: .78rem; margin: 18px 0 0; word-break: break-all; }}
  @media (max-width: 30rem) {{
    main {{ padding: 20px; }}
    .scope {{ flex-direction: column; gap: 2px; }}
    .actions {{ flex-direction: column-reverse; }}
  }}
</style>
</head>
<body>
<main>
  <h1>Authorize {app}</h1>
  <p class="who">{desc} <code>{client_id}</code></p>

  <h2>This will be able to</h2>
  <ul>{scopes}</ul>

  {pkce}

  <form method="post" action="/oauth/consent">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state_field}">
    <input type="hidden" name="code_challenge" value="{challenge_field}">
    <input type="hidden" name="code_challenge_method" value="{method_field}">
    <div class="actions">
      <button type="submit" name="approve" value="false" class="deny">Cancel</button>
      <button type="submit" name="approve" value="true" class="approve">Allow access</button>
    </div>
  </form>

  <p class="redirect">You will be returned to <code>{redirect_uri}</code> afterwards.</p>
</main>
</body>
</html>"#,
        app = escape_html(&consent.app_name),
        desc = consent
            .app_description
            .as_deref()
            .map(escape_html)
            .unwrap_or_else(|| "wants to connect to your account.".to_owned()),
        client_id = escape_html(&consent.client_id),
        redirect_uri = escape_html(&consent.redirect_uri),
        scopes = scopes_markup,
        pkce = pkce_note,
    );

    let mut response = (StatusCode::OK, Html(body)).into_response();
    // A consent screen must not be cached: a shared proxy serving a cached "Allow" form to the
    // next visitor is a consent bypass, and the form carries the client id and the challenge.
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// A body served as `text/html`.
///
/// A `Json` of a string would be `application/json` and a browser would show it as text, so the
/// content type is stated rather than left to a helper to guess.
struct Html(String);

impl IntoResponse for Html {
    fn into_response(self) -> Response {
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            self.0,
        )
            .into_response()
    }
}

/// Escape a value for use inside a double-quoted HTML attribute.
///
/// A **separate function from [`escape_html`]** even though both call the same escaper, because
/// the two answer different questions and the difference is a vulnerability that has already
/// happened once in this file. `escape_html` is for text in an element's content; this is for a
/// quoted attribute. They overlap today and there is no reason they must — the moment one of them
/// grows a case the other does not (a `javascript:` URL in an `href`, say), a caller reaching for
/// the other one by name gets the wrong guarantee.
fn html_attribute(value: &str) -> String {
    escape_html(value)
}

/// Escape text for HTML.
///
/// The consent screen renders values that came from an OAuth app's own registration — its name,
/// its description, its redirect URI — and an app is registered by a *tenant administrator*, but
/// the person approving the screen is a different human with no reason to trust that tenant's
/// text. A description containing `<script>` would otherwise run in the approver's session on the
/// platform's own origin, which is a stored-XSS reachable by any developer who can register an
/// app in their own tenant.
fn escape_html(value: &str) -> String {
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

/// `POST /oauth/consent` — the user agreed (or did not). The code is minted here, not before.
///
/// The authorization check is **re-run in full** rather than trusting the screen: the screen
/// rendered from a `GET`, and between that render and this POST the app's registration can have
/// been withdrawn or its scopes narrowed. Re-running is one index probe and one string compare,
/// and it means the code is issued against the app as it is *now*.
pub async fn consent_submit(
    State(state): State<AppState>,
    address: ClientAddress,
    current: CurrentSession,
    axum::extract::Form(input): axum::extract::Form<ConsentInput>,
) -> Result<Response, ApiError> {
    let request = omnion_developer::model_oauth::AuthorizationRequest {
        client_id: input.client_id.clone(),
        redirect_uri: input.redirect_uri.clone(),
        grant_type: GrantType::AuthorizationCode.as_str().to_owned(),
        scopes: Vec::new(),
        code_challenge: input.code_challenge.clone(),
        code_challenge_method: input.code_challenge_method.clone(),
    };

    let app = find_by_client_id(state.db().pool(), &input.client_id)
        .await
        .map_err(store_error)?
        .ok_or(DeveloperError::AppNotFound)?;
    let consent = authorize(&app, app.status, &request)?;

    // The user's own organization must be the app's. A person signed into tenant A clicking
    // "Allow" on an app belonging to tenant B has not consented to anything — and the code
    // would carry their id into B's ledger, where B's app could read it. This is the check the
    // whole session-vs-app split exists to make possible, and it is why the app's tenant has to
    // come from the app row rather than the session.
    let Some(session_organization) = current.user.organization_id else {
        return Err(ApiError::forbidden(
            "organization_required",
            "an OAuth authorization belongs to an organization; this account has none",
        ));
    };
    if app.organization_id != session_organization {
        // Refused *without* a redirect: the redirect belongs to another tenant, so sending a
        // browser to it now would tell that tenant's app that this user is here.
        return Err(ApiError::forbidden(
            "oauth_app_other_organization",
            "this application belongs to another organization",
        ));
    }

    if !input.approve {
        // A refusal is an `access_denied` back to the client, which is what lets the client show
        // "the user declined" instead of a network error. The `state` goes with it so the client
        // can match the response to the request that caused it.
        return Ok(redirect_with_error(
            &input.redirect_uri,
            "access_denied",
            "the user did not grant access",
            usable_state(input.state.as_ref()).as_deref(),
        )
        .into_response());
    }

    let now = OffsetDateTime::now_utc();
    let code = mint_code();
    issue_code(
        state.db().pool(),
        &hash_code(&code),
        app.id,
        current.user.id,
        &consent.redirect_uri,
        &consent.scopes,
        input.code_challenge.as_deref(),
        input.code_challenge_method.as_deref(),
        now + time::Duration::minutes(CODE_TTL_MINUTES),
    )
    .await
    .map_err(store_error)?;

    audit_consent(
        &state,
        &current,
        &address,
        "developer.oauth_authorized",
        &app,
        &consent,
    )
    .await?;

    Ok(redirect(&redirect_with_code(
        &consent.redirect_uri,
        &code,
        usable_state(input.state.as_ref()).as_deref(),
    )))
}

/// The audit entry for a granted authorization.
///
/// Records *that* a person authorized an app and *what* was granted — never the code, never the
/// challenge, never the verifier. A challenge is a hash of a secret the client holds, and the
/// audit table is the sort of thing that gets shipped to a log aggregator, so it is written as
/// a boolean.
async fn audit_consent(
    state: &AppState,
    current: &CurrentSession,
    address: &ClientAddress,
    action: &'static str,
    app: &omnion_developer::OAuthApp,
    consent: &omnion_developer::model_oauth::ConsentRequest,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        omnion_audit::NewAuditEntry::by_user(current.user.id, action)
            .organization(app.organization_id)
            .target("oauth_app", app.id.to_string())
            .metadata(json!({
                "client_id": app.client_id,
                "name": app.name,
                "scopes": consent.scopes,
                "pkce": consent.pkce,
                "redirect_uri": consent.redirect_uri,
            }))
            .ip_address(address.as_text()),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The token endpoint
// ---------------------------------------------------------------------------------------------

/// `POST /oauth/token` — redeem a code, or authenticate the app itself.
///
/// Two grants, and the difference is the whole of the security model:
///
/// * `authorization_code` proves a *person* consented. The code is single-use, the redirect URI
///   must match the one it was issued for, and PKCE is verified before anything is issued.
/// * `client_credentials` proves only the *app*. There is no code, no user and no consent, and
///   the token carries the app's registered scopes.
///
/// Both authenticate the client with its secret in constant time, through
/// [`which_secret_matched`], so a rotation's overlap is honoured without this file knowing
/// anything about rotations.
pub async fn token(
    State(state): State<AppState>,
    address: ClientAddress,
    headers: HeaderMap,
    axum::extract::Form(input): axum::extract::Form<TokenRequest>,
) -> Response {
    match issue(&state, &input, &headers, &address).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// The body of the token endpoint, returning a response rather than a `Result`.
///
/// Because the failures here are **RFC 6749 failures**, not platform failures: a client library
/// switches on `{"error": "invalid_grant"}` and retries or gives up. Returning this platform's
/// `{code, message}` envelope would make every well-written client treat a wrong secret as an
/// unknown error, and a `500` would make it retry a credential that will never work.
async fn issue(
    state: &AppState,
    input: &TokenRequest,
    _headers: &HeaderMap,
    address: &ClientAddress,
) -> Result<Response, ApiError> {
    let grant = GrantType::parse(&input.grant_type)
        .ok_or_else(|| oauth_error("unsupported_grant_type", "this grant type is not supported"))?;

    let now = OffsetDateTime::now_utc();
    let Some(credentials) = client_credentials(state.db().pool(), &input.client_id)
        .await
        .map_err(store_error)?
    else {
        // One refusal for "no such client" and for a client that is not usable. A token
        // endpoint that distinguishes them tells a caller which client ids exist.
        return Err(oauth_error("invalid_client", "the client credentials are not valid").into());
    };
    let Some(slot) = input
        .client_secret
        .as_deref()
        .and_then(|secret| which_secret_matched(&credentials, secret, now))
    else {
        return Err(oauth_error("invalid_client", "the client credentials are not valid").into());
    };

    // A suspended or withdrawn app authenticates as a client — the secret is still correct —
    // and is then refused at the grant. Doing it in this order means the audit row records which
    // secret authenticated, which is the fact an operator wants when a withdrawn app is still
    // being called.
    let app = find_by_client_id(state.db().pool(), &input.client_id)
        .await
        .map_err(store_error)?
        .ok_or(DeveloperError::AppNotFound)?;
    if !app.status.is_usable() || !app.grant_types.contains(&grant) {
        return Err(
            oauth_error("unauthorized_client", "this client may not use that grant").into(),
        );
    }

    let (user_id, granted) = match grant {
        GrantType::AuthorizationCode => {
            let code = input
                .code
                .as_deref()
                .ok_or_else(|| oauth_error("invalid_request", "no authorization code was sent"))?;
            let redeemed = redeem_code(state.db().pool(), &hash_code(code), now)
                .await
                .map_err(store_error)?;
            // The code belongs to a *different* app: refuse. Without this, a client that
            // intercepted a code from another app could redeem it with its own valid secret and
            // receive a token for the other app's user.
            if redeemed.app_id != app.id {
                return Err(
                    oauth_error("invalid_grant", "the authorization code is not valid").into(),
                );
            }
            // The redirect must be the one the code was issued for, compared as whole strings.
            // This is the check that stops an intercepted code being redeemed by whoever reached
            // the token endpoint, and it is required by RFC 6749 §4.1.3.
            if input.redirect_uri.as_deref() != Some(redeemed.redirect_uri.as_str()) {
                return Err(oauth_error("invalid_grant", "the redirect URI does not match").into());
            }
            // PKCE, before anything is issued. A code with a challenge is inert without its
            // verifier, and the method that matched is recorded so a client that chose `plain`
            // is visible in the audit rather than only to a security reviewer reading the row.
            if let Some((challenge, method)) = redeemed.code_challenge.as_ref() {
                let verifier = input
                    .code_verifier
                    .as_deref()
                    .ok_or_else(|| oauth_error("invalid_grant", "a code verifier is required"))?;
                verify_code_verifier(verifier, challenge, method).ok_or_else(|| {
                    oauth_error("invalid_grant", "the code verifier does not match")
                })?;
            }
            (Some(redeemed.user_id), redeemed.scopes)
        }
        GrantType::ClientCredentials => {
            // A code here is a *refusal*, not something to ignore: a client that sends one is
            // confused about which grant it is using, and issuing a token anyway would let a
            // captured user-bound code be exchanged for a machine token.
            if input.code.is_some() {
                return Err(oauth_error(
                    "invalid_request",
                    "this grant does not take an authorization code",
                )
                .into());
            }
            (None, app.scopes.clone())
        }
    };

    let requested: Vec<String> = input
        .scope
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let scopes = resolve_token_scopes(&requested, &granted)
        .ok_or_else(|| oauth_error("invalid_scope", "the requested scope was not granted"))?;

    let minted = mint_access_token();
    let expires_at = now + time::Duration::seconds(ACCESS_TOKEN_TTL_SECONDS);
    issue_token(
        state.db().pool(),
        &hash_access_token(&minted.plaintext),
        app.id,
        user_id,
        grant.as_str(),
        &scopes,
        expires_at,
    )
    .await
    .map_err(store_error)?;

    // The audit row records *which secret slot* authenticated, because that is the one bit that
    // tells an operator whether a deployment has finished redeploying after a rotation. It never
    // records the secret, the code, the verifier or the token.
    omnion_audit::record(
        state.db().pool(),
        omnion_audit::NewAuditEntry::by_user(
            user_id.unwrap_or(Uuid::nil()),
            "developer.oauth_token_issued",
        )
        .organization(app.organization_id)
        .target("oauth_app", app.id.to_string())
        .metadata(json!({
            "client_id": app.client_id,
            "grant": grant.as_str(),
            "secret_slot": slot,
            "scopes": scopes,
            "for_a_user": GrantProvenance::of(grant).involves_user(),
        }))
        .ip_address(address.as_text()),
    )
    .await
    .ok();

    Ok(Json(TokenResponse {
        token_type: "Bearer",
        access_token: minted.plaintext,
        expires_in: ACCESS_TOKEN_TTL_SECONDS,
        scope: scopes.join(" "),
        client_id: app.client_id.clone(),
    })
    .into_response())
}

/// `GET /oauth/introspect` — what a presented token may do.
///
/// A single `GET` rather than a `POST` with a form body, and that is a deliberate deviation from
/// RFC 7662: a token is a bearer credential, so putting one in a form body is a way of logging
/// it in every proxy that records request bodies, while an `Authorization` header is what the
/// platform already reads everywhere else. The response follows the RFC's shape so a client
/// library can read it — and `active: false` is the answer for a token this platform cannot
/// vouch for, which is the honest one.
pub async fn introspect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(presented) = crate::guards::bearer_token(&headers) else {
        return Err(ApiError::unauthorized(
            "unauthenticated",
            "present a token to inspect it",
        ));
    };
    let now = OffsetDateTime::now_utc();
    let inactive = || {
        Ok(Json(json!({
            "active": false,
        })))
    };

    if !token_looks_valid(&presented) {
        return inactive();
    }
    let Some(row) = find_token(state.db().pool(), &hash_access_token(&presented), now)
        .await
        .map_err(store_error)?
    else {
        return inactive();
    };

    Ok(Json(json!({
        "active": true,
        "client_id": app_client_id(&state, row.app_id).await?,
        "scope": row.scopes.join(" "),
        "exp": row.expires_at.unix_timestamp(),
        "sub": row.user_id.map(|id| id.to_string()),
    })))
}

/// The public client id for an app, for the introspection response.
///
/// A narrow one-column read rather than the whole row: the introspection response needs exactly
/// one more column than [`find_token`] returns, and reading `oauth_apps` in full to produce it
/// would drag every redirect URI of every app into a code path whose only job is to answer
/// "which client is this token". The token table deliberately does **not** denormalise the client
/// id into itself for the same reason: a copy there could disagree with the app it names, and
/// "which client is this token" is precisely the question a rotation or a withdrawal changes the
/// answer to.
async fn app_client_id(state: &AppState, app_id: Uuid) -> Result<String, ApiError> {
    store_oauth::client_id_for(state.db().pool(), app_id)
        .await
        .map_err(store_error)
}

// ---------------------------------------------------------------------------------------------
// Redirect and error helpers
// ---------------------------------------------------------------------------------------------

/// A `302` to a location already proven to be one this app registered.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    // A `Location` this platform built, so it is known to be ASCII and header-safe. If it were
    // not, dropping the header is better than sending a malformed one — but the response then has
    // no `Location` at all, so it is a `302` with no destination rather than a hop to nowhere.
    if let Ok(value) = axum::http::HeaderValue::from_str(location) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
}

/// A redirect back to the client carrying an error, per RFC 6749 §4.1.2.1.
///
/// The two `error_description` values are **static strings**, never the error's own `Display`.
/// The description rides in a query string on a URL this app registered, and an error message
/// that interpolated a caller-supplied value would put that value into browser history and
/// `Referer` headers — which is the same reason the redirect-URI refusals carry no URL.
fn redirect_with_error(
    redirect_uri: &str,
    error: &'static str,
    description: &'static str,
    state: Option<&str>,
) -> String {
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut out = format!(
        "{redirect_uri}{separator}error={error}&error_description={}",
        encode_state(description)
    );
    if let Some(state) = state {
        // The **same** encoder the code redirect uses, not a second copy of the rules. A client
        // that can parse one response can parse the other, and two encodings that must agree is
        // two encodings that will not.
        out.push_str("&state=");
        out.push_str(&encode_state(state));
    }
    out
}

/// An OAuth-protocol failure, shaped the way RFC 6749 §5.2 shapes one.
fn oauth_error(code: &'static str, description: &'static str) -> ApiError {
    // A `400` for everything a client can fix by changing its request, and a `401` for a bad
    // credential, which is what RFC 6749 says and what tells a client library the difference
    // between "fix your request" and "re-authenticate".
    let status = if code == "invalid_client" {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::BAD_REQUEST
    };
    ApiError::new(status, code, description).with_details(
        serde_json::to_value(TokenErrorBody {
            error: code,
            error_description: description.to_owned(),
        })
        .unwrap_or_else(|_| json!({})),
    )
}

/// Turn a store error into an API error, keeping the OAuth refusals distinguishable.
fn store_error(error: DeveloperError) -> ApiError {
    if error.is_client_error() {
        ApiError::bad_request(error.code(), error.to_string())
    } else {
        ApiError::from_core(omnion_core::CoreError::Unavailable {
            dependency: "developer store".into(),
            message: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the two rules this file can prove without a database ─────────────────

    #[test]
    fn an_error_redirect_never_carries_a_caller_supplied_value() {
        // The property the borrow checker forced into the signature, asserted as a rule over
        // every argument: `redirect_with_error` takes `&'static str` for the code and the
        // description, so a caller-supplied value *cannot* reach the `Location` header. The
        // compiler enforces it; this test documents which refusals depend on it, and the
        // hostile inputs below are the values that would have leaked.
        let hostile = [
            "passwrod",                   // a misspelled response_type
            "https://evil.example/steal", // an unregistered redirect
            "javascript:alert(1)",        // a scheme this platform refuses
            "content.pages.update",       // a scope the app is not registered for
        ];
        for value in hostile {
            // This is what the handler does: a fixed pair, never the error's own message.
            let out = redirect_with_error(
                "https://app.example.com/cb",
                "access_denied",
                "the authorization request was refused",
                None,
            );
            assert!(!out.contains(value), "{value:?} leaked into {out}");
        }
    }

    #[test]
    fn an_error_redirect_keeps_a_registered_query_and_the_state() {
        // Same two properties as the code redirect, because a client has to be able to parse
        // both: the app's own parameters survive, and the state is echoed so the client can
        // match this response to the request that caused it.
        let out = redirect_with_error(
            "https://app.example.com/cb?tenant=acme",
            "access_denied",
            "the user did not grant access",
            Some("a+b&c"),
        );
        assert!(out.starts_with("https://app.example.com/cb?tenant=acme&error=access_denied"));
        // The client's own parameter is still there and still first.
        assert!(out.contains("tenant=acme"));
        // The description is encoded, not interpolated raw.
        assert!(out.contains("error_description=the%20user%20did%20not%20grant%20access"));
        // And the state is encoded by the shared encoder, so the same client parses both legs.
        assert!(out.ends_with("&state=a%2Bb%26c"), "got {out}");
    }

    #[test]
    fn a_refused_client_is_a_401_and_a_refused_request_is_a_400() {
        // RFC 6749 §5.2's one distinction a client library acts on: `invalid_client` means
        // re-authenticate, everything else means fix the request. Getting this backwards is how
        // a script retries a wrong secret for ever.
        assert_eq!(
            oauth_error("invalid_client", "x").status(),
            StatusCode::UNAUTHORIZED
        );
        for code in [
            "invalid_grant",
            "invalid_scope",
            "invalid_request",
            "unsupported_grant_type",
            "unauthorized_client",
        ] {
            assert_eq!(
                oauth_error(code, "x").status(),
                StatusCode::BAD_REQUEST,
                "{code} is the caller's request to fix"
            );
        }
    }

    #[test]
    fn an_oauth_failure_answers_in_the_shapes_a_client_library_reads() {
        // A well-written client switches on `error`. Asserted on the serialised body, since the
        // whole point is what the *client* sees rather than what this platform calls it.
        let error = oauth_error("invalid_grant", "the code is not valid");
        assert_eq!(error.code(), "invalid_grant");
        let details = error.details().cloned().unwrap_or(serde_json::Value::Null);
        assert_eq!(details["error"], "invalid_grant");
        assert_eq!(details["error_description"], "the code is not valid");
        // And no credential anywhere in what a client will log.
        let rendered = format!("{error:?} {details}");
        for secretish in ["omn_tok_", "omn_app_", "code_", "client_secret"] {
            assert!(
                !rendered.contains(secretish),
                "{secretish} appears in {rendered}"
            );
        }
    }

    // ── the consent screen ──────────────────────────────────────────────────

    #[test]
    fn a_consent_screen_escapes_everything_an_app_registered() {
        // The reachable stored-XSS: an app is registered by a *tenant administrator* but the
        // person approving the screen is a different human, and the app's name, description and
        // redirect URI are all attacker-influenced text rendered on the platform's own origin.
        // A description containing a script tag must not survive as one.
        let consent = omnion_developer::model_oauth::ConsentRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/cb".to_owned(),
            scopes: vec!["content.pages.read".to_owned()],
            app_name: "<script>alert('name')</script>".to_owned(),
            app_description: Some("<img src=x onerror=alert(1)>".to_owned()),
            logo_object_key: None,
            pkce: true,
        };
        let query = AuthorizeQuery {
            state: Some("\"><script>alert(1)</script>".to_owned()),
            code_challenge: Some(oauth_flow_challenge()),
            code_challenge_method: Some("S256".to_owned()),
            ..AuthorizeQuery::default()
        };
        let body = body_text(consent_screen(&consent, &query));

        // The property is that no *tag* and no *attribute break* survives — not that the words
        // are gone. `&lt;img src=x onerror=alert(1)&gt;` contains the literal text `onerror=alert`
        // and is completely inert, because the `<` and `>` are entities. So each assertion is
        // on the dangerous *form*: an actual `<script` tag, an actual `<img`, an actual `"` that
        // closes the hidden field's attribute and starts a new one.
        assert!(
            !body.contains("<script"),
            "a raw script tag survived in the consent screen"
        );
        assert!(
            !body.contains("<img"),
            "a raw img tag from the description survived"
        );
        assert!(
            !body.contains("\"><script"),
            "a raw quote from the state escaped its attribute"
        );
        // And the escaped forms are all present, so the screen still shows the real values.
        assert!(body.contains("&lt;script&gt;alert(&#39;name&#39;)&lt;/script&gt;"));
        assert!(body.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // The state is a JSON-encoded value in an attribute, so its quotes are `\"` and its
        // angle brackets are entities — a double-quote in the state cannot close the attribute.
        assert!(
            body.contains("&lt;/script&gt;"),
            "the state's script tag was escaped"
        );
        // And the scope the app asked for is on the screen.
        assert!(body.contains("content.pages.read"));
    }

    #[test]
    fn a_consent_screen_is_never_cached_and_states_whether_pkce_is_in_play() {
        // Both are security properties, not polish. A cached "Allow" form served to the next
        // visitor is a consent bypass, and a screen that does not say whether PKCE is in play
        // is a screen where a user approves something they cannot evaluate.
        let consent = omnion_developer::model_oauth::ConsentRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/cb".to_owned(),
            scopes: vec!["content.pages.read".to_owned()],
            app_name: "Reporting".to_owned(),
            app_description: None,
            logo_object_key: None,
            pkce: true,
        };
        let query = AuthorizeQuery {
            code_challenge: Some(oauth_flow_challenge()),
            code_challenge_method: Some("S256".to_owned()),
            ..AuthorizeQuery::default()
        };
        let response = consent_screen(&consent, &query);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        // The body is read after the header check, so the two assertions are independent: a
        // change to either does not mask the other.
        let body = body_text(response);
        assert!(
            body.contains("PKCE"),
            "the screen does not mention PKCE at all"
        );
        // And the challenge is echoed into the form so the POST can re-validate it, which means
        // the screen carries the challenge — hence the no-store.
        assert!(body.contains(oauth_flow_challenge().as_str()));
    }

    #[test]
    fn a_consent_screen_with_no_pkce_says_so_rather_than_implying_protection() {
        // The absence of a sentence is not a statement. A client that skipped PKCE gets a
        // screen that says its secret will be required instead.
        let consent = omnion_developer::model_oauth::ConsentRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/cb".to_owned(),
            scopes: vec!["content.pages.read".to_owned()],
            app_name: "Reporting".to_owned(),
            app_description: None,
            logo_object_key: None,
            pkce: false,
        };
        let body = body_text(consent_screen(&consent, &AuthorizeQuery::default()));
        assert!(body.contains("did not use PKCE"), "got: {body}");
    }

    #[test]
    fn a_scope_list_is_rendered_one_row_per_scope_with_a_readable_label() {
        // The screen lists what is being granted; an empty list must say so rather than render
        // nothing, which reads as "no permissions" and hides the fact that the app asked for
        // none.
        let consent = |scopes: Vec<String>| omnion_developer::model_oauth::ConsentRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/cb".to_owned(),
            scopes,
            app_name: "Reporting".to_owned(),
            app_description: None,
            logo_object_key: None,
            pkce: false,
        };

        let listed = body_text(consent_screen(
            &consent(vec![
                "content.pages.read".to_owned(),
                "search.read".to_owned(),
            ]),
            &AuthorizeQuery::default(),
        ));
        assert!(listed.contains("content.pages.read"));
        assert!(listed.contains("search.read"));
        // The last segment is what a person recognises.
        // The label is the last segment, which is the word a person recognises.
        assert!(
            listed.contains(">read<"),
            "the readable label is the last segment"
        );
        assert!(
            !listed.contains("read pages"),
            "the label is not the joined path"
        );
        assert_eq!(listed.matches("<li class=\"scope\"").count(), 2);

        let none = body_text(consent_screen(
            &consent(Vec::new()),
            &AuthorizeQuery::default(),
        ));
        assert!(none.contains("no scopes requested"));
    }

    #[test]
    fn the_approve_button_is_a_positive_and_its_default_is_a_refusal() {
        // `approve` absent must mean "no". A form that only submits the affirmative button and
        // treats a missing field as consent would consent anybody whose button click was
        // dropped, which is exactly the case a proxy error produces.
        // Deserialised the way the endpoint's `Form` extractor deserialises it, minus the
        // urlencoding: the property under test is that an absent `approve` reads as `false`,
        // and that is a property of the struct's `#[serde(default)]` rather than of the parser.
        let approved: ConsentInput = serde_json::from_value(serde_json::json!({
            "client_id": "omn_app_x",
            "redirect_uri": "https://a.example/cb",
            "approve": true,
        }))
        .expect("parses");
        assert!(approved.approve);
        let refused: ConsentInput = serde_json::from_value(serde_json::json!({
            "client_id": "omn_app_x",
            "redirect_uri": "https://a.example/cb",
        }))
        .expect("parses without approve");
        assert!(!refused.approve, "an absent approve is a refusal");
        // And the `false` the Cancel button submits is still a refusal — the screen's two
        // buttons must not produce a third state.
        let cancelled: ConsentInput = serde_json::from_value(serde_json::json!({
            "client_id": "omn_app_x",
            "redirect_uri": "https://a.example/cb",
            "approve": false,
        }))
        .expect("parses");
        assert!(!cancelled.approve);
    }

    fn oauth_flow_challenge() -> String {
        omnion_developer::oauth::code_challenge_s256("a-verifier-that-is-long-enough")
    }

    #[test]
    fn a_json_encoded_string_is_not_an_html_attribute_escaping() {
        // **The defect this module had, as a standalone regression test**, because a general
        // "nothing leaked" assertion cannot tell you *which* mistake you made and this one is
        // worth naming.
        //
        // `serde_json::to_string` is the right encoding for a value going into a JavaScript
        // string literal and the *wrong* one for a double-quoted HTML attribute, because JSON
        // escapes a quote as `\"` and HTML does not recognise that — it wants `&quot;`. The two
        // look so alike that reaching for the JSON one is easy, and the result is a live script
        // tag in the approver's session on the platform's own origin.
        let hostile = "\"><script>alert(1)</script>";

        // What the JSON version produced: the quote is escaped for the wrong language, so the
        // attribute closes early and the script tag becomes markup.
        let json_encoded = serde_json::to_string(hostile).expect("serialises");
        assert!(json_encoded.contains("\\\""));
        let as_markup = format!("<input value=\"{json_encoded}\">");
        assert!(
            as_markup.contains("\"><script"),
            "the JSON form does break out of the attribute — that is the bug"
        );

        // What the HTML form produces: no attribute break, and a browser's parser reads the
        // value back byte-identical to what the client sent.
        let html_encoded = html_attribute(hostile);
        let markup = format!("<input value=\"{html_encoded}\">");
        assert!(!markup.contains("\"><script"));
        assert!(!markup.contains("<script"));
        assert_eq!(
            unescape_attribute(&html_encoded),
            hostile,
            "an escaped attribute must decode to what the client sent"
        );
    }

    #[test]
    fn an_absent_state_renders_as_absent_rather_than_the_word_null() {
        // Behaviour, not looks. A hidden input whose value is the four characters `null` posts
        // the *string* "null", which `usable_state` accepts and which the token endpoint then
        // echoes back to the client as a `state` it never sent. Absence has to render as
        // absence, or the platform invents a value the client did not choose.
        let body = body_text(consent_screen(
            &plain_consent(false),
            &AuthorizeQuery::default(),
        ));
        assert!(body.contains(r#"name="state" value="">"#), "got: {body}");
        assert!(!body.contains(r#"value="null""#));
        // Same for the two PKCE fields of a client that sent no challenge at all.
        assert!(body.contains(r#"name="code_challenge" value="">"#));
        assert!(body.contains(r#"name="code_challenge_method" value="">"#));
    }

    /// Undo [`html_attribute`], the way a browser's attribute parser would.
    ///
    /// Only the five entities [`escape_html`] can produce. A general unescaper would be a
    /// dependency for a test helper; this one has to agree with the escaper, which is why the
    /// test round-trips rather than asserting a literal string.
    fn unescape_attribute(value: &str) -> String {
        let mut out = value.to_owned();
        for (entity, character) in [
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&#39;", "'"),
            ("&amp;", "&"),
        ] {
            out = out.replace(entity, character);
        }
        out
    }

    fn plain_consent(pkce: bool) -> omnion_developer::model_oauth::ConsentRequest {
        plain_consent_with(vec!["content.pages.read".to_owned()]).with_pkce(pkce)
    }

    fn plain_consent_with(scopes: Vec<String>) -> omnion_developer::model_oauth::ConsentRequest {
        omnion_developer::model_oauth::ConsentRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/cb".to_owned(),
            scopes,
            app_name: "Reporting".to_owned(),
            app_description: None,
            logo_object_key: None,
            pkce: false,
        }
    }

    /// The body of a response as text.
    ///
    /// `Body`'s byte accessors are async and these tests are not, so this reads it on a
    /// one-shot runtime. A helper rather than repeated inline runtime construction, and named
    /// for what it does.
    fn body_text(response: Response) -> String {
        use http_body_util::BodyExt;
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime needs no reactor")
            .block_on(async {
                let collected = response
                    .into_body()
                    .collect()
                    .await
                    .expect("the screen's body is complete");
                String::from_utf8(collected.to_bytes().to_vec()).expect("the screen is UTF-8")
            })
    }
}
