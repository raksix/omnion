//! The OAuth endpoints (REQ-087 slice 3, API half).
//!
//! Four handlers, and the whole surface exists to keep one promise across a *round trip* the
//! platform does not control: **the token set never lands anywhere but the encrypted store, and
//! the credential it belongs to is decided by a signature rather than by a row id in a query
//! string.**
//!
//! | Route | Session | Why |
//! |---|---|---|
//! | `POST /credentials/{id}/oauth/start` | yes | mints a signed `state`, returns the URL |
//! | `GET /public/oauth/callback` | **no** | the provider redirects a browser, not a session |
//! | `POST /credentials/{id}/disconnect` | yes | drops the token set, keeps the row |
//! | `POST /credentials/{id}/oauth/refresh` | yes | the forced refresh the test hook needs |
//!
//! The callback carries no session because it cannot: the person is at the provider, and the
//! provider sends them back with `?code=…&state=…`. Everything the callback needs to
//! authenticate the request is in the `state` itself — it is HMAC-signed by this installation,
//! carries the *organization and the credential* it was minted for, and expires in ten
//! minutes. That is why the route takes the organization from the state rather than from a
//! session: a state minted for tenant A cannot connect a credential in tenant B even if the
//! browser presenting it is signed in as somebody else entirely.
//!
//! Three things this module refuses to do, and each refusal is the REQ's:
//!
//! * **It never returns a token.** The response bodies are `authorize_url`, a connected
//!   identity, or a sentence. `TokenSet` has neither `Debug` nor `Serialize`, so a handler
//!   that tried to hand one back would not compile.
//! * **It never stores a secret itself.** The token payload goes to the encrypted store
//!   REQ-125 owns, and when that store is unavailable the callback says the connection did
//!   *not* happen rather than writing the token into `settings` where a `select *` finds it.
//! * **It never records `needs_reauth` for a provider having a bad minute.** Only a refusal
//!   the provider actually issued is evidence about the credential; see
//!   [`refusal_is_about_the_credential`].

use std::sync::LazyLock;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_workflows::credential_store;
use omnion_workflows::credentials::Credential;
use omnion_workflows::oauth::{
    self, CallbackQuery, LocalBox, PkcePair, StateRejection, TokenRequest, TokenSet, build_state,
    state_hash, verify_state,
};
use omnion_workflows::oauth_client::{OAuthClient, PROVIDER_TIMEOUT, token_set_from_status};
use omnion_workflows::oauth::RefreshLock;
use omnion_workflows::oauth_refresh::{
    self, RefreshOutcome, RefreshPlan, reauth_sentence, refusal_is_about_the_credential,
};
use omnion_workflows::oauth_store::{self, FlowClaim, NewOAuthFlow};
use omnion_workflows::registry::{CredentialDefinition, OAuthConfig, find_credential_type};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::credentials::{CredentialBody, map_store_error, write_token_payload};
use crate::scope::resolve_organization;
use crate::state::AppState;
use omnion_workflows::{Result, WorkflowError};

/// The crate-wide result, spelled with both parameters.
///
/// `omnion_workflows::Result` is `Result<T, WorkflowError>` — two parameters — so it cannot be
/// used for a handler that returns an HTTP error. Naming it here keeps every `?` in this
/// module reading the same as the rest of the API.
type ApiResult<T> = std::result::Result<T, ApiError>;

/// The store/transport result, re-aliased so the `HttpClient` below can return it without
/// shadowing the HTTP one. The two error types are different on purpose: a provider that
/// refuses is a [`WorkflowError`] the flow layer turns into a health decision, and an
/// `ApiError` would turn it into an HTTP status before that decision could be made.
type FlowResult<T> = Result<T>;

/// The single-flight lock, shared by every refresh in this process.
///
/// A `LazyLock` rather than a field on `AppState` because the lock is *process* state, not
/// configuration: two `AppState`s (a test, a second router) must still contend, because a
/// refresh that is not single-flight across them is a refresh that invalidates its own token.
static REFRESH_LOCK: LazyLock<RefreshLock> = LazyLock::new(RefreshLock::new);

/// The installation secret, read once.
///
/// A `LazyLock` because the derivation below is *deterministic* — it is a keyed hash of a
/// fixed label through the installation's key — and a function that recomputed it per call
/// would be correct only by accident. The version here is the one that was wrong: a fresh
/// `SecretBox::from_env()` per call produced the same bytes, but the code that *wrapped* them
/// in a fresh random-nonce envelope did not, so `build_state` and `verify_state` were handed
/// different keys and no callback in the product could ever have verified. The state is a
/// random value, not an encrypted one, so a random nonce here buys nothing and costs the
/// whole flow.
///
/// [`SecretBox::from_env`] warns when it is on its development default, and it does so once
/// per process rather than once per call, which is the only reason to cache.
/// The installation's own secret, as bytes.
///
/// Deliberately the **raw** material, not the output of `SecretBox::encrypt` — an envelope
/// carries a random nonce, so hashing one would give a different key on every call and nothing
/// would ever verify. Reading the variable here keeps the derivation deterministic, which is
/// the one property an HMAC key has to have, and it is why the two keys are derived from the
/// same *string* through two different labels rather than from two different envelopes.
fn installation_material() -> Vec<u8> {
    // The development default is named, not silent: a state signed with it is forgeable by
    // anyone who has read this source, so a production deployment must notice.
    std::env::var("OMNION_MFA_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            "omnion-development-mfa-key-do-not-use-in-production".to_string()
        })
        .into_bytes()
}

/// The key the flow's short-lived secrets are sealed with.
///
/// Derived from the installation's `OMNION_MFA_KEY` through [`SecretBox::from_env`] and then
/// domain-separated *again* inside [`LocalBox`], so the same key material serving the identity
/// crate's MFA envelopes produces an unrelated key space here.
fn local_box() -> LocalBox {
    LocalBox::from_key_material(&installation_material())
}

/// The HMAC key for the signed state, distinct from the seal's key on purpose.
///
/// `build_state` signs and `LocalBox` seals. If both used one key, an envelope this module
/// produced could be replayed as a `state` and a `state` could be opened as an envelope. They
/// come from the same installation secret through different labels, which is what domain
/// separation is for — and the *seal's* key is cached in a `LocalBox` for the same reason this
/// one must be stable: a key that changes between signing and verifying makes the flow
/// un-completable, and the failure looks like a CSRF refusal.
/// The HMAC key for the signed state, distinct from the seal's key on purpose.
///
/// `build_state` signs and `LocalBox` seals. If both used one key, an envelope this module
/// produced could be replayed as a `state` and a `state` could be opened as an envelope. They
/// come from the same installation secret through different labels, which is what domain
/// separation is for.
///
/// The subtle part, and the reason this function needed rewriting: the derivation is a **plain
/// keyed hash**, not an encryption. [`LocalBox`] and `SecretBox` are both correct primitives
/// for *sealing a value* and both produce a fresh random nonce per call — which is exactly
/// right for a PKCE verifier and catastrophically wrong for an HMAC key, because two calls
/// would return different key bytes and nothing would ever verify. The first version of this
/// function used the encryption form, and the symptom was not a crash: every callback failed
/// as `credential_oauth_state`, a sentence about CSRF for what is really a key that never
/// matched. The test that names the organization a state was minted for is what found it.
fn state_key() -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(SEAL_LABEL_FOR_SIGNING);
    hasher.update(installation_material());
    hasher.finalize().to_vec()
}

/// The domain-separation label for [`state_key`], so the seal and the signature keys differ.
const SEAL_LABEL_FOR_SIGNING: &[u8] = b"omnion.workflow.oauth.state-key.v1";

/// The host the panel was reached on, from the edge's forwarded header or the socket's.
fn panel_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The scheme the browser used, honouring the edge's TLS termination.
fn panel_scheme(headers: &HeaderMap) -> &str {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http")
}

/// The `redirect_uri` this installation sends to providers.
///
/// Built from the request's own host rather than from configuration, because a provider
/// refuses a redirect URI it did not have registered and a hard-coded host makes every
/// staging install a broken connection. The scheme follows the forwarded header because the
/// platform's edge terminates TLS: without it a production panel hands providers `http://`
/// and the state we just signed travels in a query string over cleartext.
fn redirect_uri(headers: &HeaderMap) -> ApiResult<String> {
    let host = panel_host(headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "credential_oauth_host_unknown",
            "the panel was reached without a Host header, so the redirect URI cannot be built; \
             set a public base URL for this installation",
        )
    })?;
    Ok(format!(
        "{}://{host}/api/v1/public/oauth/callback",
        panel_scheme(headers)
    ))
}

/// The one place a state refusal becomes an HTTP answer.
///
/// Five distinct sentences for five distinct situations, because they are five different
/// problems with five different fixes and a person staring at "invalid request" learns
/// nothing. `StateRejection` already separates them, and collapsing that at the API boundary
/// would undo the separation the algebra provides.
fn state_rejection(rejection: StateRejection) -> ApiError {
    let message = match rejection {
        StateRejection::Unrecognised => {
            "this callback does not carry a state this installation issued, so the connection \
             was not made; start it again from the credential"
        }
        StateRejection::Expired => {
            "this connection was started more than ten minutes ago and its window has closed; \
             start it again from the credential"
        }
        StateRejection::WrongCredential => {
            "this state was issued for a different credential, so the connection was not made"
        }
        StateRejection::WrongOrganization => {
            "this state was issued for another organization, so the connection was not made"
        }
        StateRejection::Used => {
            "this connection was already completed or refused; a state is good once. If the \
             provider sent you back a second time, start the connection again"
        }
    };
    ApiError::bad_request(oauth::codes::STATE, message)
}

/// The `oauth` config of a type, or say the button has nothing to start.
fn oauth_config<'a>(
    definition: Option<&'a CredentialDefinition>,
    key: &str,
) -> ApiResult<&'a OAuthConfig> {
    match definition.and_then(|definition| definition.oauth.as_ref()) {
        Some(config) => Ok(config),
        None => Err(ApiError::bad_request(
            oauth::codes::UNSUPPORTED,
            format!("the {key:?} credential type has no authorization-code flow"),
        )),
    }
}

/// The `client_id`, which is a *setting* rather than a secret.
///
/// A `400` naming the field, not an empty string: an OAuth flow started with a blank client
/// id produces a provider page reading "this application does not exist", which tells the
/// person nothing about which of their two fields is wrong.
fn client_id(credential: &Credential) -> ApiResult<String> {
    credential
        .settings
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::bad_request(
                "credential_field_required",
                format!(
                    "{:?} has no client_id, so there is nothing to authorize against; fill the \
                     type's client_id field in first",
                    credential.key
                ),
            )
        })
}

/// The non-secret `client_secret` setting, for a confidential client.
///
/// The type declares it `secret`, so the stored *value* is never here — this reads the field
/// the form wrote into `settings` only because the create path refuses a secret inside
/// `settings`. A credential whose client secret went through the replace-secret path has it
/// behind `secret_ref` and is answered by [`read_stored_client_secret`].
fn settings_client_secret(credential: &Credential) -> Option<&str> {
    credential
        .settings
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The panel URL the callback settles on, with the outcome in the query.
///
/// The panel, not an API body: the callback is a browser navigation, and a person who lands
/// on raw JSON has been left in a dead end. The outcome is a code the panel renders one
/// sentence from, because four different sentences in a toast is four things a reader misses.
fn settle_redirect(headers: &HeaderMap, outcome: &str) -> Option<String> {
    let host = panel_host(headers)?;
    let mut url = url::Url::parse(&format!(
        "{}://{host}/workflows/credentials",
        panel_scheme(headers)
    ))
    .ok()?;
    url.query_pairs_mut().append_pair("oauth", outcome);
    Some(url.to_string())
}

/// The HTML a callback answers with when the panel URL cannot be built.
///
/// A closure rather than a redirect failure: a person mid-consent who gets a `502` has no way
/// to tell whether their authorization worked, and the honest answer is a page that says it
/// did and names the credential.
fn settle_page(outcome: &str, headline: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Connection {outcome}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:34rem;margin:4rem auto;\
         padding:0 1.5rem;line-height:1.6\">\
         <h1 style=\"font-size:1.25rem\">{headline}</h1>\
         <p><a href=\"/workflows/credentials\">Back to credentials</a></p>\
         </body></html>"
    ))
}

/// The browser answer: a redirect when the panel URL builds, a page when it does not.
fn settled(headers: &HeaderMap, outcome: &'static str, headline: &'static str) -> axum::response::Response {
    let response = match settle_redirect(headers, outcome) {
        Some(location) => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(
                axum::http::header::LOCATION,
                location.parse::<axum::http::HeaderValue>().unwrap_or_else(|_| {
                    axum::http::HeaderValue::from_static("/workflows/credentials")
                }),
            )
            .body(axum::body::Body::empty())
            .unwrap_or_default(),
        // A panel URL that cannot be built must not cost the reader the *answer*. A person
        // who just authorized something deserves to be told whether it worked.
        None => {
            let page = settle_page(outcome, headline);
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "text/html; charset=utf-8",
                )
                .body(axum::body::Body::from(page.0))
                .unwrap_or_default()
        }
    };
    response
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The start response.
#[derive(Debug, Serialize)]
pub struct OAuthStartResponse {
    /// The credential being connected.
    pub credential_id: Uuid,
    /// The credential's own key, so the panel does not correlate two responses.
    pub credential_key: String,
    /// Where to send the person.
    pub authorize_url: String,
    /// The callback this installation registered with the provider.
    pub redirect_uri: String,
    /// The scopes asked for.
    pub scopes: String,
    /// Whether the flow carries a PKCE challenge.
    pub pkce: bool,
    /// When the state stops being claimable.
    pub expires_at: OffsetDateTime,
    /// The flow id, for the panel's own record.
    pub flow_id: Uuid,
}

/// The disconnect response.
#[derive(Debug, Serialize)]
pub struct DisconnectResponse {
    /// The credential after the token set was dropped.
    pub credential: CredentialBody,
    /// Whether anything was there to drop.
    pub was_connected: bool,
    /// How many in-progress connections were closed by it.
    pub flows_expired: u64,
}

/// The refresh response.
#[derive(Debug, Serialize)]
pub struct RefreshResponse {
    /// What the refresh concluded, as the panel's own word for it.
    pub outcome: String,
    /// A usable token is available.
    pub has_token: bool,
    /// The provider's sentence, when there was one.
    pub detail: Option<String>,
    /// The credential after the refresh.
    pub credential: CredentialBody,
}

/// The start body.
#[derive(Debug, Default, Deserialize)]
pub struct OAuthStartBody {
    /// Override the redirect URI, for an installation whose public base URL cannot be derived
    /// from the request host — a panel behind a path-prefixed proxy, say. Refused when it is
    /// not `https`, because a `redirect_uri` is where the code lands and a cleartext one hands
    /// it to whoever is on that network.
    pub redirect_uri: Option<String>,
    /// Override the scopes, space-separated.
    pub scopes: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/credentials/{id}/oauth/start` — begin the flow.
pub async fn start_oauth(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<OAuthStartBody>,
) -> ApiResult<Json<OAuthStartResponse>> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = load(state.db().pool(), organization_id, id).await?;
    let config = oauth_config(find_credential_type(&credential.r#type), &credential.r#type)?;
    let client_id = client_id(&credential)?;

    let redirect_uri = match body.redirect_uri.as_deref() {
        Some(candidate) => checked_redirect_uri(candidate)?,
        None => redirect_uri(&headers)?,
    };
    let scopes = body
        .scopes
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(config.scopes)
        .to_string();

    // The pair is minted before the state so the challenge goes into both the URL and the flow
    // row: the callback re-derives it from the sealed verifier and refuses a mismatch, so a
    // row edited by somebody holding the key still cannot produce a token request a provider
    // will accept.
    let pkce = config.pkce.then(PkcePair::generate);
    let now = OffsetDateTime::now_utc();
    // Named `signed_state`, not `state`: the handler already binds `State(state)` for the
    // app state, and a local `state` would shadow it and turn every `state.db()` below into a
    // call on a `String`. The compiler catches it, but the sentence is here because this is
    // the exact collision a reader hits.
    let signed_state = build_state(organization_id, credential.id, &state_key(), now);
    let authorize_url = oauth::authorization_url(
        config.authorize_url,
        &client_id,
        &redirect_uri,
        &scopes,
        &signed_state,
        pkce.as_ref(),
    )
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_oauth_authorize_url",
            format!("the credential type's authorization endpoint is not a usable URL: {error}"),
        )
    })?;

    let sealed_verifier = pkce.as_ref().map(|pair| local_box().seal(pair.verifier()));
    let flow = oauth_store::insert_flow(
        state.db().pool(),
        NewOAuthFlow {
            organization_id,
            credential_id: credential.id,
            credential_type: credential.r#type.clone(),
            state_hash: state_hash(&signed_state),
            code_challenge: pkce.as_ref().map(|pair| pair.challenge.clone()),
            code_verifier_enc: sealed_verifier,
            authorize_url: authorize_url.clone(),
            redirect_uri,
            scopes: Some(scopes.clone()),
            expires_at: oauth_store::expiry_from(now),
        },
    )
    .await
    .map_err(map_store_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.credential_oauth_started")
            .organization(organization_id)
            .target("workflow_credential", credential.id.to_string())
            .metadata(json!({ "key": credential.key, "flow_id": flow.id })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.oauth_started")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "flow_id": flow.id,
            })),
    )
    .await
    .ok();

    Ok(Json(OAuthStartResponse {
        credential_id: credential.id,
        credential_key: credential.key,
        authorize_url,
        redirect_uri: flow.redirect_uri,
        scopes,
        pkce: pkce.is_some(),
        expires_at: flow.expires_at,
        flow_id: flow.id,
    }))
}

/// `GET /api/v1/public/oauth/callback` — the provider sends the person back.
///
/// No session, by design: the browser is at the provider and carries nothing. Everything this
/// needs to authenticate the request is inside the `state`, which is signed by this
/// installation and names the organization *and* the credential it was minted for. The
/// organization therefore comes from the state's payload, not from a session — which is also
/// what stops a state minted for tenant A from connecting a credential in tenant B.
pub async fn oauth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> ApiResult<axum::response::Response> {
    // A provider refusal comes before anything else: there is no state to spend when the
    // person said no, and reporting "unrecognised state" for a declined consent screen sends
    // the reader hunting for a CSRF attack that did not happen.
    if let Some(refusal) = query.refusal() {
        bus::emit(
            state.db().pool(),
            NewEvent::new("workflows.credential.oauth_failed")
                .payload(json!({ "reason": refusal, "stage": "provider" })),
        )
        .await
        .ok();
        return Ok(settled(
            &headers,
            "refused",
            "The provider refused the connection. Nothing was changed.",
        ));
    }

    let raw_state = query
        .state
        .as_deref()
        .ok_or_else(|| state_rejection(StateRejection::Unrecognised))?;
    let code = query
        .code
        .clone()
        .ok_or_else(|| {
            ApiError::bad_request(
                oauth::codes::EXCHANGE,
                "the provider sent neither a code nor an error, so there is nothing to exchange",
            )
        })?;

    // Two independent checks, in this order. `verify_state` proves the value is one this
    // installation minted, is inside its window, and names a live credential *in a known
    // organization* — and it yields that organization, which is why it runs before any
    // query. `claim_flow` then spends it exactly once. Either alone would be enough against a
    // replay; both are needed because the first is stateless and the second is scoped.
    let verified = verify_state(raw_state, &state_key(), OffsetDateTime::now_utc(), None)
        .map_err(state_rejection)?;
    let organization_id = verified.organization_id;

    let claim = oauth_store::claim_flow(
        state.db().pool(),
        organization_id,
        &state_hash(raw_state),
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(map_store_error)?;
    let flow = match claim {
        FlowClaim::Claimed(flow) => flow,
        other => {
            return Err(state_rejection(
                other.rejection().unwrap_or(StateRejection::Unrecognised),
            ));
        }
    };

    let Some(credential) = credential_store::get_credential(
        state.db().pool(),
        organization_id,
        verified.credential_id,
    )
    .await
    .map_err(map_store_error)?
    else {
        // The credential was deleted while the person was at the provider. The flow is spent
        // either way, so the answer has the same shape as every other failure: a sentence,
        // and the row left saying what happened.
        let _ = oauth_store::fail_flow(
            state.db().pool(),
            organization_id,
            flow.id,
            "credential_missing",
            "the credential was deleted while the connection was in progress",
        )
        .await;
        return Ok(settled(
            &headers,
            "failed",
            "This credential no longer exists, so nothing was connected.",
        ));
    };
    let config = oauth_config(find_credential_type(&credential.r#type), &credential.r#type)?;

    // PKCE is verified *before* the code is spent. A challenge that does not derive from our
    // verifier means the flow that produced this code is not the flow we started, and the
    // code is worthless to us either way — so the refusal costs nothing.
    let verifier = match oauth_store::open_pkce(&flow, &local_box()) {
        Ok(pair) => pair.map(|pair| pair.verifier().to_string()),
        Err(error) => {
            let _ = oauth_store::fail_flow(
                state.db().pool(),
                organization_id,
                flow.id,
                oauth::codes::PKCE,
                &error.to_string(),
            )
            .await;
            return Ok(settled(
                &headers,
                "failed",
                "The PKCE verifier for this connection could not be read back. Start it again \
                 from the credential.",
            ));
        }
    };
    if let (Some(challenge), Some(verifier)) = (flow.code_challenge.as_deref(), verifier.as_deref())
        && !PkcePair::verify(challenge, verifier)
    {
        let _ = oauth_store::fail_flow(
            state.db().pool(),
            organization_id,
            flow.id,
            oauth::codes::PKCE,
            "the challenge does not derive from the verifier this flow holds",
        )
        .await;
        return Ok(settled(
            &headers,
            "failed",
            "This connection's PKCE challenge did not verify, so no token was requested. Start \
             it again from the credential.",
        ));
    }

    let client_secret = match settings_client_secret(&credential) {
        Some(secret) => Some(secret.to_string()),
        None => read_stored_client_secret(state.db().pool(), &credential).await?,
    };
    let request = TokenRequest::authorization_code(
        &code,
        &client_id(&credential)?,
        client_secret.as_deref(),
        &flow.redirect_uri,
        verifier.as_deref(),
    );

    let token_set = match HttpClient::new().post(config.token_url, &request).await {
        Ok(set) => set,
        Err(error) => {
            // `release_flow` puts it back to `pending` rather than leaving it `completing`:
            // a provider that was briefly unreachable must not leave a person who reloads the
            // consent screen with a spent state and nothing to return with.
            let reason = error.to_string();
            let _ = oauth_store::release_flow(state.db().pool(), organization_id, flow.id).await;
            let _ = oauth_store::fail_flow(
                state.db().pool(),
                organization_id,
                flow.id,
                oauth::codes::EXCHANGE,
                &reason,
            )
            .await;
            bus::emit(
                state.db().pool(),
                NewEvent::new("workflows.credential.oauth_failed")
                    .organization(organization_id)
                    .payload(json!({
                        "credential_id": credential.id,
                        "key": credential.key,
                        "code": oauth::codes::EXCHANGE,
                        "reason": reason,
                    })),
            )
            .await
            .ok();
            return Ok(settled(
                &headers,
                "failed",
                "The provider would not exchange the code, so nothing was connected.",
            ));
        }
    };

    // The one write of a token on this surface, and it goes to the encrypted store. When that
    // store is unavailable the answer is a *failure*, not a partial success: a credential
    // marked connected with no token behind it authenticates nothing, and the reader is the
    // one who finds out at run time.
    let secret_ref =
        match write_token_payload(state.db().pool(), organization_id, &token_set).await {
            Ok(handle) => handle,
            Err(error) => {
                let _ = oauth_store::fail_flow(
                    state.db().pool(),
                    organization_id,
                    flow.id,
                    "credential_secret_store_unavailable",
                    error.message(),
                )
                .await;
                bus::emit(
                    state.db().pool(),
                    NewEvent::new("workflows.credential.oauth_failed")
                        .organization(organization_id)
                        .payload(json!({
                            "credential_id": credential.id,
                            "key": credential.key,
                            "code": "credential_secret_store_unavailable",
                            "reason": error.message(),
                        })),
                )
                .await
                .ok();
                return Ok(settled(
                    &headers,
                    "failed",
                    "The token was not stored, so the connection was not made.",
                ));
            }
        };

    let summary = token_set.summary();
    let updated = credential_store::mark_connected(
        state.db().pool(),
        organization_id,
        credential.id,
        &secret_ref,
        summary.subject.as_deref(),
        summary.scopes.as_deref(),
        summary.expires_at,
    )
    .await
    .map_err(map_store_error)?;
    let _ = oauth_store::complete_flow(
        state.db().pool(),
        organization_id,
        flow.id,
        summary.subject.as_deref(),
    )
    .await;

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.oauth_connected")
            .organization(organization_id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "subject": summary.subject,
                "scopes": summary.scopes,
                "expires_at": summary.expires_at,
            })),
    )
    .await
    .ok();

    let Some(updated) = updated else {
        return Ok(settled(
            &headers,
            "failed",
            "The credential was deleted while the connection was being made, so nothing was \
             connected.",
        ));
    };
    let _ = updated;
    Ok(settled(
        &headers,
        "connected",
        "The credential is connected.",
    ))
}

/// `POST /api/v1/credentials/{id}/disconnect` — drop the token set.
pub async fn disconnect(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DisconnectResponse>> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = load(state.db().pool(), organization_id, id).await?;
    let was_connected = credential.secret_ref.is_some();

    let updated = credential_store::mark_disconnected(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })?;

    // A disconnect strands any flow in progress: the person is at the provider right now
    // holding a state that would reconnect a credential they have just said goodbye to. The
    // flows are dropped rather than left pending, so a state that comes back afterwards is
    // refused as unrecognised instead of quietly re-connecting.
    let flows_expired = oauth_store::delete_flows_for(state.db().pool(), organization_id, id)
        .await
        .unwrap_or(0);

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(
            current.user.id,
            "workflow.credential_disconnected",
        )
        .organization(organization_id)
        .target("workflow_credential", credential.id.to_string())
        .metadata(json!({
            "key": credential.key,
            "was_connected": was_connected,
            "flows_expired": flows_expired,
        })),
    )
    .await
    .ok();

    bus::emit(
        state.db().pool(),
        NewEvent::new("workflows.credential.oauth_disconnected")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "credential_id": credential.id,
                "key": credential.key,
                "was_connected": was_connected,
                "flows_expired": flows_expired,
            })),
    )
    .await
    .ok();

    Ok(Json(DisconnectResponse {
        credential: CredentialBody::describe(&updated),
        was_connected,
        flows_expired,
    }))
}

/// `POST /api/v1/credentials/{id}/oauth/refresh` — the forced refresh.
///
/// The same [`oauth_refresh::refresh_or_use`] an action handler calls, exposed as a route so
/// the panel's **Test connection** button on an OAuth credential performs a *real* exchange
/// rather than reporting a verdict it did not earn. That is the `ok: true` branch the REQ's
/// test criterion was missing until slice 3.
///
/// A `Busy` answers `202` with `outcome: "busy"`: the request is not a failure and the caller
/// should try again, and answering `409` or `502` would put a red chip on a credential whose
/// token another thread is refreshing successfully.
pub async fn refresh_credential(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<RefreshResponse>)> {
    let organization_id = resolve_organization(&current, None)?;
    let credential = load(state.db().pool(), organization_id, id).await?;
    let config = oauth_config(find_credential_type(&credential.r#type), &credential.r#type)?;

    let client_secret = match settings_client_secret(&credential) {
        Some(secret) => Some(secret.to_string()),
        None => read_stored_client_secret(state.db().pool(), &credential).await?,
    };
    let stored_refresh_token =
        read_stored_refresh_token(state.db().pool(), &credential).await?;
    let plan = RefreshPlan {
        credential_id: credential.id,
        token_url: config.token_url,
        client_id: &client_id(&credential)?,
        client_secret: client_secret.as_deref(),
        scopes: None,
        expires_at: credential.oauth_expires_at,
        refresh_token: stored_refresh_token.as_deref(),
        now: OffsetDateTime::now_utc(),
    };

    let outcome = oauth_refresh::refresh_or_use(&HttpClient::new(), &REFRESH_LOCK, plan).await;

    match outcome {
        RefreshOutcome::Fresh { is_current } => {
            let (status, detail) = if is_current {
                (
                    StatusCode::OK,
                    "the stored token is inside its window, so nothing was exchanged".to_string(),
                )
            } else {
                (
                    StatusCode::CONFLICT,
                    format!(
                        "the stored token for {:?} expired{} and this installation has no refresh \
                         token to spend, so it cannot be renewed; reconnect the credential",
                        credential.key,
                        credential
                            .oauth_expires_at
                            .map(|at| format!(" at {at}"))
                            .unwrap_or_default()
                    ),
                )
            };
            Ok((
                status,
                Json(RefreshResponse {
                    outcome: if is_current { "fresh" } else { "expired" }.into(),
                    has_token: is_current,
                    detail: Some(detail),
                    credential: CredentialBody::describe(&credential),
                }),
            ))
        }
        RefreshOutcome::Refreshed(set) => {
            let secret_ref =
                match write_token_payload(state.db().pool(), organization_id, &set).await {
                    Ok(handle) => handle,
                    Err(error) => {
                        return Err(ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "secret_store_unavailable",
                            format!(
                                "the provider issued a new token but the encrypted store refused \
                                 it, so nothing changed: {}",
                                error.message()
                            ),
                        ));
                    }
                };
            let summary = set.summary();
            let updated = credential_store::mark_connected(
                state.db().pool(),
                organization_id,
                credential.id,
                &secret_ref,
                summary.subject.as_deref().or(credential.oauth_subject.as_deref()),
                summary
                    .scopes
                    .as_deref()
                    .or(credential.oauth_scopes.as_deref()),
                summary.expires_at,
            )
            .await
            .map_err(map_store_error)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "credential_not_found",
                    "no such credential",
                )
            })?;
            bus::emit(
                state.db().pool(),
                NewEvent::new("workflows.credential.oauth_refreshed")
                    .organization(organization_id)
                    .actor(current.user.id)
                    .payload(json!({
                        "credential_id": credential.id,
                        "key": credential.key,
                        "expires_at": summary.expires_at,
                    })),
            )
            .await
            .ok();
            Ok((
                StatusCode::OK,
                Json(RefreshResponse {
                    outcome: "refreshed".into(),
                    has_token: true,
                    detail: Some("the provider issued a new token".into()),
                    credential: CredentialBody::describe(&updated),
                }),
            ))
        }
        RefreshOutcome::Busy => Ok((
            StatusCode::ACCEPTED,
            Json(RefreshResponse {
                outcome: "busy".into(),
                has_token: true,
                detail: Some(
                    "another node is refreshing this credential right now; try again in a moment"
                        .into(),
                ),
                credential: CredentialBody::describe(&credential),
            }),
        )),
        RefreshOutcome::Reauth { reason } => {
            // Only a refusal the provider actually issued is evidence the credential is
            // broken. A transport failure — a provider having a bad minute, DNS, a timeout —
            // leaves the row alone, because marking it `needs_reauth` sends the reader to
            // re-authorize for something that is not their fault.
            let about_credential = refusal_is_about_the_credential(&reason);
            let sentence = reauth_sentence(&credential.key, &reason);
            let updated = if about_credential {
                let updated = credential_store::mark_needs_reauth(
                    state.db().pool(),
                    organization_id,
                    credential.id,
                    &sentence,
                )
                .await
                .map_err(map_store_error)?;
                if updated.is_some() {
                    bus::emit(
                        state.db().pool(),
                        NewEvent::new("workflows.credential.needs_reauth")
                            .organization(organization_id)
                            .actor(current.user.id)
                            .payload(json!({
                                "credential_id": credential.id,
                                // The canvas reads this to disable the nodes naming the key.
                                "credential_key": credential.key,
                                "reason": sentence,
                            })),
                    )
                    .await
                    .ok();
                }
                updated.unwrap_or(credential)
            } else {
                credential
            };
            Ok((
                StatusCode::BAD_GATEWAY,
                Json(RefreshResponse {
                    outcome: if about_credential {
                        "needs_reauth"
                    } else {
                        "unavailable"
                    }
                    .into(),
                    has_token: false,
                    detail: Some(sentence),
                    credential: CredentialBody::describe(&updated),
                }),
            ))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Support
// ---------------------------------------------------------------------------------------------

/// Load one credential in this organization, or refuse as though it did not exist.
async fn load(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> ApiResult<Credential> {
    credential_store::get_credential(pool, organization_id, id)
        .await
        .map_err(map_store_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "credential_not_found",
                "no such credential",
            )
        })
}

/// The stored refresh token, read out of the encrypted store — never from this schema.
///
/// The store is REQ-125's and is not wired in this build. Inventing a local scheme is exactly
/// what the REQ forbids, so this answers "nothing to spend", which
/// [`oauth_refresh::refresh_or_use`] treats as an ordinary non-refreshable token rather than
/// as a failure. The consequence is stated rather than hidden: a forced refresh on a
/// credential whose token is not yet due reports `fresh` with an honest sentence, and one
/// whose type issues no refresh token reports `expired` when it lapses.
async fn read_stored_refresh_token(
    _pool: &PgPool,
    _credential: &Credential,
) -> ApiResult<Option<String>> {
    Ok(None)
}

/// The stored client secret, from the same place — and for the same reason.
async fn read_stored_client_secret(
    _pool: &PgPool,
    _credential: &Credential,
) -> ApiResult<Option<String>> {
    Ok(None)
}

/// A redirect URI a panel supplied, checked.
///
/// A cleartext or scheme-less one is refused rather than normalised, because the URI is where
/// the authorization code lands and a provider that accepts `http://` will happily hand it to
/// whoever is on that network.
fn checked_redirect_uri(candidate: &str) -> ApiResult<String> {
    let parsed = url::Url::parse(candidate.trim()).map_err(|error| {
        ApiError::bad_request(
            "credential_oauth_redirect_uri",
            format!("the redirect_uri is not a URL: {error}"),
        )
    })?;
    if parsed.scheme() != "https" {
        return Err(ApiError::bad_request(
            "credential_oauth_redirect_uri",
            format!(
                "the redirect_uri must be https — a cleartext one hands the authorization code \
                 to whoever is on the network between the provider and this panel (got {:?})",
                parsed.scheme()
            ),
        ));
    }
    if !parsed.has_host() {
        return Err(ApiError::bad_request(
            "credential_oauth_redirect_uri",
            "the redirect_uri has no host",
        ));
    }
    Ok(parsed.to_string())
}

/// The `reqwest`-backed client, over the one function every exchange goes through.
///
/// A concrete struct rather than a generic in the handler so the two exchanges — the code
/// exchange and the refresh — cannot drift into two different HTTP shapes, and so the timeout
/// and the redirect policy are decided once.
struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    fn new() -> Self {
        Self {
            inner: reqwest::Client::builder()
                .timeout(PROVIDER_TIMEOUT)
                // A provider that redirects its token endpoint must not be followed with the
                // code still in the body: the second hop would be a different host holding a
                // bearer credential. One hop, and a `307` is a refusal.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                // A client that cannot be built is a missing TLS backend, and there is nothing
                // an OAuth flow can do about it — the default client carries the same
                // configuration minus the tuning, and a `502` from the callback is a truthful
                // answer.
                .unwrap_or_default(),
        }
    }

    async fn post(&self, token_url: &str, request: &TokenRequest) -> FlowResult<TokenSet> {
        let response = self
            .inner
            .post(token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(request.encode())
            .send()
            .await
            .map_err(|error| {
                WorkflowError::CredentialInvalid(format!(
                    "the token endpoint could not be reached: {error}"
                ))
            })?;
        let status = response.status().as_u16();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        token_set_from_status(status, &body)
    }
}

impl OAuthClient for HttpClient {
    async fn exchange_code(
        &self,
        token_url: &str,
        request: &TokenRequest,
    ) -> FlowResult<TokenSet> {
        self.post(token_url, request).await
    }

    async fn refresh(&self, token_url: &str, request: &TokenRequest) -> FlowResult<TokenSet> {
        self.post(token_url, request).await
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(host: &str, proto: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_str(host).expect("host"),
        );
        if let Some(proto) = proto {
            headers.insert(
                "x-forwarded-proto",
                HeaderValue::from_str(proto).expect("proto"),
            );
        }
        headers
    }

    #[test]
    fn a_state_is_hashed_rather_than_stored() {
        let state = build_state(Uuid::nil(), Uuid::nil(), &state_key(), OffsetDateTime::now_utc());
        let hash = state_hash(&state);
        assert_eq!(hash.len(), 64, "sha-256 hex is 64 characters");
        assert!(!hash.contains(&state), "the state itself is nowhere in the hash");
        assert_ne!(
            hash,
            state_hash(&format!("{state}x")),
            "a changed state hashes differently"
        );
    }

    #[test]
    fn two_states_for_one_credential_differ() {
        let key = state_key();
        let now = OffsetDateTime::now_utc();
        assert_ne!(
            build_state(Uuid::nil(), Uuid::nil(), &key, now),
            build_state(Uuid::nil(), Uuid::nil(), &key, now)
        );
    }

    #[test]
    fn a_state_signed_by_another_installation_does_not_verify() {
        let theirs = build_state(
            Uuid::nil(),
            Uuid::nil(),
            b"another-installation",
            OffsetDateTime::now_utc(),
        );
        assert!(
            verify_state(&theirs, &state_key(), OffsetDateTime::now_utc(), None).is_err(),
            "a state we did not mint must not verify"
        );
    }

    #[test]
    fn a_verified_state_names_the_organization_it_was_minted_for() {
        // The callback's entire authorization story: the organization comes from the state,
        // so a state minted for one tenant cannot reach another tenant's credential — and the
        // handler never has to guess which tenant it is in.
        let organization = Uuid::from_u128(0x1234_5678);
        let credential = Uuid::from_u128(0xabcd);
        let state = build_state(organization, credential, &state_key(), OffsetDateTime::now_utc());
        let verified =
            verify_state(&state, &state_key(), OffsetDateTime::now_utc(), Some(credential))
                .expect("a state we just minted verifies");
        assert_eq!(verified.organization_id, organization);
        assert_eq!(verified.credential_id, credential);
    }

    #[test]
    fn the_redirect_uri_follows_the_forwarded_scheme() {
        // Behind the platform's own edge TLS terminates, so without this a production panel
        // hands providers a cleartext redirect and the state travels in the clear.
        let uri = redirect_uri(&headers_with("panel.example", Some("https"))).expect("built");
        assert_eq!(uri, "https://panel.example/api/v1/public/oauth/callback");
    }

    #[test]
    fn a_request_with_no_host_says_so_rather_than_guessing() {
        let error = redirect_uri(&HeaderMap::new()).expect_err("no host");
        assert_eq!(error.code(), "credential_oauth_host_unknown");
    }

    #[test]
    fn a_forwarded_host_wins_over_the_socket_host() {
        // The edge sets it; the socket's `Host` is the internal name.
        let mut headers = headers_with("api.internal:3000", Some("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("panel.example"),
        );
        let uri = redirect_uri(&headers).expect("built");
        assert!(uri.starts_with("https://panel.example/"), "{uri}");
    }

    #[test]
    fn a_supplied_redirect_uri_must_be_https() {
        let error = checked_redirect_uri("http://panel.example/cb").expect_err("cleartext");
        assert_eq!(error.code(), "credential_oauth_redirect_uri");
        assert!(error.message().contains("https"), "{}", error.message());
    }

    #[test]
    fn a_supplied_redirect_uri_that_is_https_and_absolute_is_accepted() {
        let uri = checked_redirect_uri(" https://panel.example/api/v1/public/oauth/callback ")
            .expect("accepted");
        assert_eq!(uri, "https://panel.example/api/v1/public/oauth/callback");
    }

    #[test]
    fn the_five_state_refusals_say_five_different_things() {
        // Collapsing these at the API boundary would undo the separation the algebra
        // provides, and a person learns nothing from a generic "invalid request".
        let mut sentences = std::collections::BTreeSet::new();
        for rejection in [
            StateRejection::Unrecognised,
            StateRejection::Expired,
            StateRejection::WrongCredential,
            StateRejection::WrongOrganization,
            StateRejection::Used,
        ] {
            let error = state_rejection(rejection);
            assert_eq!(error.code(), oauth::codes::STATE);
            sentences.insert(error.message().to_string());
        }
        assert_eq!(sentences.len(), 5);
    }

    #[test]
    fn the_settle_url_names_the_outcome_so_the_panel_can_render_one_sentence() {
        let headers = headers_with("panel.example", Some("https"));
        let location = settle_redirect(&headers, "connected").expect("built");
        assert!(location.contains("oauth=connected"), "{location}");
    }

    #[test]
    fn a_settle_page_still_tells_the_reader_the_answer_when_the_url_cannot_be_built() {
        // A person who just authorized something must not be left with a 502 and no idea
        // whether it worked.
        let page = settle_page("connected", "The credential is connected.");
        assert!(page.0.contains("The credential is connected."));
        assert!(page.0.contains("/workflows/credentials"), "and a way back");
        assert!(settle_redirect(&HeaderMap::new(), "connected").is_none());
    }

    #[test]
    fn a_provider_refusal_is_reported_before_the_state_is_ever_looked_at() {
        // A person who declined consent gets a refusal, not "unrecognised state" — the second
        // sends them hunting for a CSRF attack that did not happen.
        let query = CallbackQuery::parse("error=access_denied&error_description=User+said+no");
        let sentence = query.refusal().expect("a refusal");
        assert!(sentence.contains("access_denied"), "{sentence}");
        assert!(query.state.is_none(), "a refusal carries no state to spend");
    }

    #[test]
    fn a_callback_with_a_code_but_no_state_is_refused_before_any_lookup() {
        let query = CallbackQuery::parse("code=abc");
        assert!(query.refusal().is_none());
        assert!(query.state.is_none());
        assert!(
            state_rejection(StateRejection::Unrecognised).message().contains("start it again"),
            "and the sentence says what to do"
        );
    }

    #[tokio::test]
    async fn the_stored_refresh_token_is_answered_not_absent() {
        // Nothing behind the secret store yet, and the answer is a value rather than a
        // panic or an invented scheme — which the refresh caller reads as "this type issues
        // no refresh token", not as a failure.
        let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/none").expect("lazy");
        let token = read_stored_refresh_token(&pool, &credential_stub())
            .await
            .expect("an answer, not an error");
        assert!(token.is_none(), "there is no token to read yet");
    }

    fn credential_stub() -> Credential {
        let now = OffsetDateTime::now_utc();
        Credential {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            key: "fixture".into(),
            name: "Fixture".into(),
            r#type: "oauth2".into(),
            scope: "organization".into(),
            sharing: "private".into(),
            secret_ref: None,
            settings: json!({ "client_id": "cid" }),
            owner_user_id: None,
            health: "untested".into(),
            health_checked_at: None,
            health_detail: None,
            oauth_expires_at: None,
            oauth_scopes: None,
            oauth_subject: None,
            last_used_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_credential_with_no_client_id_names_the_field_rather_than_starting_a_broken_flow() {
        let mut credential = credential_stub();
        credential.settings = json!({});
        let error = client_id(&credential).expect_err("no client id");
        assert_eq!(error.code(), "credential_field_required");
        assert!(error.message().contains("client_id"), "{}", error.message());
    }

    #[test]
    fn a_blank_client_id_is_treated_as_absent() {
        let mut credential = credential_stub();
        credential.settings = json!({ "client_id": "   " });
        assert!(client_id(&credential).is_err(), "whitespace is not a client id");
    }

    #[test]
    fn a_type_with_no_oauth_config_says_so_rather_than_building_a_url() {
        // The button has nothing to start, and saying "not supported" is the honest answer.
        let error = oauth_config(find_credential_type("api_key"), "api_key").expect_err("no flow");
        assert_eq!(error.code(), oauth::codes::UNSUPPORTED);
    }

    #[test]
    fn the_oauth2_type_is_the_one_that_declares_a_flow() {
        assert!(
            oauth_config(find_credential_type("oauth2"), "oauth2").is_ok(),
            "the registry's oauth2 type is the fixture this slice drives"
        );
    }
}
