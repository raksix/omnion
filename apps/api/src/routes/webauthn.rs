//! `/api/v1/auth/webauthn` — passkeys: enrolment for the signed-in account, and the ceremony
//! that finishes a sign-in (REQ-006, slice 3b).
//!
//! The route family is deliberately split in two halves:
//!
//! - **Enrolment** (`register/begin`, `register/complete`, and the `passkeys` list/removal) runs
//!   behind a session: a passkey belongs to the account at the keyboard, and an administrator
//!   cannot create one for somebody else — only reset the factors of another account
//!   (`/api/v1/iam/users/{id}/reset-mfa`, which demands a step-up).
//! - **The sign-in half** (`authenticate/begin`, `authenticate/complete`) runs *before* a
//!   session exists, exactly like `auth/mfa/verify`: the password check answers a challenge
//!   token, and the passkey ceremony that consumes it turns that half-finished sign-in into a
//!   session. Both halves therefore answer the same session cookie a password sign-in does.
//!
//! Everything a browser calls `navigator.credentials.*` with is verified by
//! [`omnion_identity::webauthn`]: the challenge this server issued, the origin it accepts (with
//! the documented loopback exception the QA stack relies on), the relying party hash, and the
//! signature over `authenticatorData || SHA-256(clientDataJSON)`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::mfa;
use omnion_identity::signin;
use omnion_identity::webauthn;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::auth::start_session;
use crate::routes::iam::record;
use crate::routes::iam_security::{FactorBody, require_step_up};
use crate::state::AppState;

/// Product name a passkey is labelled with in the browser's own dialog.
const RP_NAME: &str = "Omnion";

/// How long a browser may take to run the ceremony (the server-side window is
/// [`webauthn::CHALLENGE_TTL_MINUTES`], which is the one that decides).
const CEREMONY_TIMEOUT_MS: i64 = 120_000;

/// The algorithms offered when a credential is created: P-256 first (every authenticator), then
/// Ed25519.
const ALGORITHMS: [i64; 2] = [webauthn::cose::ALG_ES256, webauthn::cose::ALG_EDDSA];

/// Record an event without letting a webhook problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// Enrolment (a signed-in account)
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/auth/webauthn/register/begin`.
#[derive(Debug, Default, Deserialize)]
pub struct LabelBody {
    /// The label the account holder recognises ("Work laptop").
    #[serde(default)]
    pub label: Option<String>,
}

/// Start a registration ceremony: answer the options a browser passes to
/// `navigator.credentials.create`.
///
/// The body is optional: a label is a convenience, not a requirement, and a client that posts
/// nothing at all still gets a usable ceremony.
pub async fn register_begin(
    State(state): State<AppState>,
    current: CurrentSession,
    body: Option<Json<LabelBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let pool = state.db().pool();
    let rp_id = webauthn::rp_id_from_env();

    let live = mfa::list_passkeys(pool, current.user.id).await?;
    if i64::try_from(live.len()).unwrap_or(0) >= mfa::MAX_PASSKEYS {
        return Err(ApiError::bad_request(
            "too_many_passkeys",
            format!(
                "this account already holds the maximum of {} passkeys — remove one first",
                mfa::MAX_PASSKEYS
            ),
        ));
    }

    let challenge = webauthn::create_challenge(
        pool,
        current.user.id,
        webauthn::PURPOSE_REGISTRATION,
        &rp_id,
    )
    .await?;

    // The user handle is opaque and stable; the browser shows the name and the display name.
    let user_handle = webauthn::encode_b64(current.user.id.as_bytes());
    let exclude: Vec<serde_json::Value> = live
        .iter()
        .filter_map(|factor| factor.credential_id.as_deref())
        .map(|id| json!({ "type": "public-key", "id": id }))
        .collect();

    Ok(Json(json!({
        "challenge": challenge,
        "rp": { "id": rp_id, "name": RP_NAME },
        "user": {
            "id": user_handle,
            "name": current.user.email,
            "displayName": current.user.display_name,
        },
        "pubKeyCredParams": ALGORITHMS
            .iter()
            .map(|alg| json!({ "type": "public-key", "alg": alg }))
            .collect::<Vec<_>>(),
        "timeout": CEREMONY_TIMEOUT_MS,
        "attestation": "none",
        "authenticatorSelection": {
            "residentKey": "preferred",
            "userVerification": "preferred",
        },
        "excludeCredentials": exclude,
        "label": body.label,
    })))
}

/// One credential as the browser serialises it (base64url for every buffer).
#[derive(Debug, Deserialize)]
pub struct CredentialBody {
    /// The credential id.
    pub id: String,
    /// `clientDataJSON`.
    #[serde(default)]
    pub client_data_json: String,
    /// `attestationObject` (registration only).
    #[serde(default)]
    pub attestation_object: String,
    /// `authenticatorData` (assertion only).
    #[serde(default)]
    pub authenticator_data: String,
    /// `signature` (assertion only).
    #[serde(default)]
    pub signature: String,
    /// Transports the browser reports.
    #[serde(default)]
    pub transports: Vec<String>,
}

/// Body of `POST /api/v1/auth/webauthn/register/complete`.
#[derive(Debug, Deserialize)]
pub struct RegisterCompleteBody {
    /// The ceremony challenge the begin call issued.
    pub challenge: String,
    /// Label for the new factor.
    #[serde(default)]
    pub label: Option<String>,
    /// What the browser produced.
    pub credential: CredentialBody,
}

/// Finish a registration ceremony: verify it and store the passkey.
pub async fn register_complete(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<RegisterCompleteBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let rp_id = webauthn::rp_id_from_env();
    let origins = webauthn::OriginPolicy::from_env();

    // Single use: the challenge is spent by the attempt, so a replay of the same ceremony is
    // refused even if it were captured.
    let taken = webauthn::take_challenge(
        pool,
        current.user.id,
        webauthn::PURPOSE_REGISTRATION,
        &body.challenge,
    )
    .await?;
    if taken.is_none() {
        return Err(ApiError::bad_request(
            "webauthn_challenge",
            "that ceremony challenge is unknown or has expired — start again",
        ));
    }

    let registration = webauthn::verify_registration(
        &body.credential.client_data_json,
        &body.credential.attestation_object,
        &body.challenge,
        &rp_id,
        &origins,
    )?;

    if registration.credential_id != body.credential.id {
        return Err(ApiError::bad_request(
            "webauthn_refused",
            "the credential id does not match the attested credential",
        ));
    }
    if mfa::passkey_exists(pool, &registration.credential_id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "credential_registered",
            "this passkey is already enrolled",
        ));
    }

    let transports: Vec<String> = {
        let mut list: Vec<String> = body
            .credential
            .transports
            .iter()
            .map(|transport| transport.to_lowercase())
            .filter(|transport| {
                matches!(
                    transport.as_str(),
                    "usb" | "nfc" | "ble" | "internal" | "hybrid" | "smart-card"
                )
            })
            .take(4)
            .collect();
        list.dedup();
        if list.is_empty() {
            list.push("internal".to_owned());
        }
        list
    };

    let label = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Passkey");
    let factor = mfa::create_passkey(
        pool,
        current.user.id,
        label,
        &registration.credential_id,
        &registration.public_key,
        registration.sign_count,
        &transports,
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_enrolled")
            .organization(current.user.organization_id)
            .target("user", current.user.id.to_string())
            .metadata(json!({
                "factor_id": factor.id,
                "kind": "webauthn",
                "algorithm": registration.algorithm,
                "transports": transports,
            })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.mfa_enrolled")
            .organization(current.user.organization_id)
            .actor(Some(current.user.id))
            .payload(json!({ "user_id": current.user.id, "kind": "webauthn" })),
    )
    .await;

    let passkeys = mfa::list_passkeys(pool, current.user.id).await?.len();
    Ok(Json(json!({
        "factor": FactorBody::from(factor),
        "algorithm": registration.algorithm,
        "passkeys": passkeys,
    })))
}

/// List the caller's own passkeys.
pub async fn list_passkeys(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let factors = mfa::list_passkeys(state.db().pool(), current.user.id).await?;
    Ok(Json(json!({
        "passkeys": factors
            .into_iter()
            .map(FactorBody::from)
            .collect::<Vec<FactorBody>>(),
    })))
}

/// Remove one of the caller's own passkeys (a step-up is demanded, like every factor removal).
pub async fn revoke_passkey(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(factor_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let factor = mfa::find_factor(state.db().pool(), current.user.id, factor_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "factor_not_found",
                "this account has no such second factor",
            )
        })?;
    if factor.kind != "webauthn" {
        return Err(ApiError::bad_request(
            "not_a_passkey",
            "that factor is not a passkey — use the second-factor route for it",
        ));
    }

    require_step_up(&current, "passkey_revoke")?;

    let revoked = mfa::revoke_factor(state.db().pool(), current.user.id, factor_id).await?;
    if !revoked {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "factor_not_found",
            "this account has no such second factor",
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.mfa_removed")
            .organization(current.user.organization_id)
            .target("user", current.user.id.to_string())
            .metadata(json!({ "factor_id": factor_id, "kind": "webauthn" })),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("iam.mfa_removed")
            .organization(current.user.organization_id)
            .actor(Some(current.user.id))
            .payload(json!({ "user_id": current.user.id, "kind": "webauthn" })),
    )
    .await;

    Ok(Json(
        json!({ "factor_id": factor_id, "kind": "webauthn", "revoked": true }),
    ))
}

// ---------------------------------------------------------------------------------------------
// The sign-in half (no session yet)
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/auth/webauthn/authenticate/begin`.
#[derive(Debug, Deserialize)]
pub struct AssertionBeginBody {
    /// The sign-in challenge a password check answered with.
    pub challenge: String,
}

/// Answer the options a browser passes to `navigator.credentials.get` for a half-finished
/// sign-in. The challenge token is *read*, not consumed: only a verified assertion spends it.
pub async fn authenticate_begin(
    State(state): State<AppState>,
    Json(body): Json<AssertionBeginBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.db().pool();
    let Some(user_id) =
        signin::peek_challenge(pool, &body.challenge, signin::PURPOSE_LOGIN).await?
    else {
        return Err(ApiError::unauthorized(
            "invalid_challenge",
            "this sign-in challenge is no longer valid — sign in again",
        ));
    };

    let rp_id = webauthn::rp_id_from_env();
    let passkeys = mfa::list_passkeys(pool, user_id).await?;
    if passkeys.is_empty() {
        return Err(ApiError::bad_request(
            "no_passkey",
            "this account holds no passkey — use a code from an enrolled factor instead",
        ));
    }

    let challenge =
        webauthn::create_challenge(pool, user_id, webauthn::PURPOSE_AUTHENTICATION, &rp_id).await?;

    let allow: Vec<serde_json::Value> = passkeys
        .iter()
        .filter_map(|factor| {
            factor.credential_id.as_deref().map(|id| {
                let transports: Vec<&str> = factor.transports.iter().map(String::as_str).collect();
                if transports.is_empty() {
                    json!({ "type": "public-key", "id": id })
                } else {
                    json!({ "type": "public-key", "id": id, "transports": transports })
                }
            })
        })
        .collect();

    Ok(Json(json!({
        "challenge": challenge,
        "rpId": rp_id,
        "allowCredentials": allow,
        "timeout": CEREMONY_TIMEOUT_MS,
        "userVerification": "preferred",
    })))
}

/// Body of `POST /api/v1/auth/webauthn/authenticate/complete`.
#[derive(Debug, Deserialize)]
pub struct AssertionCompleteBody {
    /// The sign-in challenge token the password check answered with.
    pub challenge: String,
    /// The ceremony challenge the begin call issued.
    pub ceremony_challenge: String,
    /// What the browser produced.
    pub credential: CredentialBody,
}

/// Finish a sign-in with a passkey; the response carries the session cookie.
pub async fn authenticate_complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: ClientAddress,
    Json(body): Json<AssertionCompleteBody>,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let rp_id = webauthn::rp_id_from_env();
    let origins = webauthn::OriginPolicy::from_env();
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let ip_address = client.as_text();

    let Some(user_id) =
        signin::peek_challenge(pool, &body.challenge, signin::PURPOSE_LOGIN).await?
    else {
        return Err(ApiError::unauthorized(
            "invalid_challenge",
            "this sign-in challenge is no longer valid — sign in again",
        ));
    };

    let Some(factor) = mfa::find_passkey(pool, user_id, &body.credential.id).await? else {
        return Err(ApiError::bad_request(
            "credential_unknown",
            "this account holds no such passkey",
        ));
    };
    let Some(public_key) = factor.public_key.as_deref() else {
        return Err(ApiError::bad_request(
            "credential_unknown",
            "this passkey has no stored public key",
        ));
    };

    // The ceremony challenge is spent by the attempt — a captured assertion cannot be replayed.
    let taken = webauthn::take_challenge(
        pool,
        user_id,
        webauthn::PURPOSE_AUTHENTICATION,
        &body.ceremony_challenge,
    )
    .await?;
    if taken.is_none() {
        return Err(ApiError::bad_request(
            "webauthn_challenge",
            "that ceremony challenge is unknown or has expired — start again",
        ));
    }

    let assertion = match webauthn::verify_assertion(
        &body.credential.client_data_json,
        &body.credential.authenticator_data,
        &body.credential.signature,
        &body.ceremony_challenge,
        &rp_id,
        &origins,
        public_key,
        factor.sign_count,
    ) {
        Ok(assertion) => assertion,
        Err(error) => {
            let Some(user) = omnion_identity::users::find_by_id(pool, user_id).await? else {
                return Err(ApiError::unauthorized(
                    "invalid_challenge",
                    "this sign-in challenge is no longer valid — sign in again",
                ));
            };
            signin::record_attempt(
                pool,
                &signin::AttemptRecord {
                    email: user.email.clone(),
                    user_id: Some(user.id),
                    organization_id: user.organization_id,
                    ip: ip_address.clone(),
                    user_agent: user_agent.clone(),
                    outcome: "failed",
                    reason: Some("passkey assertion did not verify".to_owned()),
                },
            )
            .await?;
            return Err(error.into());
        }
    };

    let Some(user_id) =
        signin::consume_challenge(pool, &body.challenge, signin::PURPOSE_LOGIN).await?
    else {
        return Err(ApiError::unauthorized(
            "invalid_challenge",
            "this sign-in challenge is no longer valid — sign in again",
        ));
    };
    let Some(user) = omnion_identity::users::find_by_id(pool, user_id).await? else {
        return Err(ApiError::unauthorized(
            "invalid_challenge",
            "this sign-in challenge is no longer valid — sign in again",
        ));
    };

    mfa::record_passkey_use(pool, factor.id, assertion.sign_count).await?;

    signin::record_attempt(
        pool,
        &signin::AttemptRecord {
            email: user.email.clone(),
            user_id: Some(user.id),
            organization_id: user.organization_id,
            ip: ip_address.clone(),
            user_agent: user_agent.clone(),
            outcome: "success",
            reason: Some("passkey".to_owned()),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(user.id, "iam.signin_succeeded")
            .organization(user.organization_id)
            .target("factor", factor.id.to_string())
            .metadata(json!({
                "method": "webauthn",
                "credential_id": body.credential.id,
                "user_verified": assertion.user_verified,
            })),
    )
    .await?;

    start_session(
        &state,
        &user,
        user_agent,
        ip_address,
        vec!["password".to_owned(), "webauthn".to_owned()],
    )
    .await
}
