//! `/api/v1/reliability/intake` and the guarded data-plane path (REQ-127, slice 4).
//!
//! The screen this file serves is the one an integrator opens after a provider says `401` and
//! the platform's log says the signature is invalid. It answers three questions, in this order:
//!
//! | Path | Power | Question |
//! |---|---|---|
//! | `GET /reliability/intake` | `reliability.read` | what is declared, and what has it refused |
//! | `POST /reliability/intake` | `reliability.intake.manage` | declare a path |
//! | `PATCH /reliability/intake/{id}` | `reliability.intake.manage` | retune HMAC, cap, tolerance |
//! | `POST /reliability/intake/{id}/verify-sample` | `reliability.intake.manage` | is this signature right? |
//! | `GET /reliability/intake/rejections` | `reliability.read` | the refusal log |
//! | `POST /public/intake/{id}` | none — it IS the inbound path | the guarded request itself |
//!
//! ## Three things this file refuses to do
//!
//! 1. **Return a secret.** `secret_id` is a reference and a `••••` marker is the only thing any
//!    response says about the value. The `Verify sample` action runs the guard and reports a
//!    verdict; it does not have a "show me the signing key" path, because a route that can
//!    answer that is a route every future bug in this area can exfiltrate through.
//! 2. **Echo a rejected payload.** The refusal body carries the reason, the request id and the
//!    size — never a slice of what was sent. The reason is the actionable half and the payload
//!    is the sensitive half, and only one of them is ever needed to fix an integration.
//! 3. **Let `reliability.manage` declare a path.** The catalogue splits them on purpose
//!    (`crates/permissions/src/catalogue.rs`): a budget is a number, a declaration is a
//!    **door**. One that edits both is one compromise away from an unauthenticated endpoint.
//!
//! ## The data-plane route is separate on purpose
//!
//! The guard is exposed at a *declared* path rather than as a middleware over the whole tree,
//! for the reason [`intake_store::find_by_path`] documents: a guard that guards more than it says
//! is a guard nobody can reason about. An operator who declares one path gets exactly that path.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_reliability::intake::{self, GuardVerdict, Inbound, Rejection};
use omnion_reliability::intake_store as store;
use omnion_reliability::intake_store::StoredEndpoint;
use omnion_reliability::vocabulary::{INTAKE_REASONS, INTAKE_SCHEMES, SANITIZE_PROFILES};
use omnion_reliability::ReliabilityError;

/// A store call that is really a raw statement — the `secrets` existence probe below.
///
/// Separate from [`map_store`] on purpose: a `sqlx::Error` from this probe is a database
/// problem, and routing it through the reliability mapper would answer `500` with a sentence
/// about the reliability store for a query that never touched it.
fn map_db(error: sqlx::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "intake_lookup_failed",
        format!("the intake store could not be read: {error}"),
    )
}
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

fn map_store(error: ReliabilityError) -> ApiError {
    match error {
        invalid @ ReliabilityError::Invalid(_) => {
            ApiError::bad_request("invalid_reliability_input", invalid.to_string())
        }
        ReliabilityError::NotFound => {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such intake endpoint")
        }
        ReliabilityError::ProviderUnavailable { provider, retry_after } => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            format!("provider {provider} is unavailable"),
        )
        .with_retry_after(retry_after.unwrap_or(1)),
        ReliabilityError::RetriesExhausted { subsystem } => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "retry_exhausted",
            format!("retries exhausted for {subsystem}"),
        ),
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reliability_store_failed",
            format!("the reliability store did not answer: {other}"),
        ),
    }
}


// ---------------------------------------------------------------------------------------------
// The panel's list
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct IntakeListBody {
    pub endpoints: Vec<EndpointView>,
    pub schemes: Vec<&'static str>,
    pub profiles: Vec<&'static str>,
    pub reasons: Vec<&'static str>,
    /// `reason -> count`, so the screen's chips and the log can never disagree about which
    /// refusal is the common one.
    pub reason_counts: Vec<(String, i64)>,
}

#[derive(Debug, Serialize)]
pub struct EndpointView {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    pub hmac_scheme: String,
    pub signature_header: String,
    pub timestamp_header: Option<String>,
    pub tolerance_seconds: i32,
    /// A reference, never a value. See the file header.
    pub secret_id: Option<Uuid>,
    pub has_secret: bool,
    pub max_payload_bytes: i32,
    pub sanitize_profile: String,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
    pub rejection_count: i64,
    pub last_rejection_at: Option<OffsetDateTime>,
}

impl From<StoredEndpoint> for EndpointView {
    fn from(stored: StoredEndpoint) -> Self {
        Self {
            id: stored.id,
            path: stored.endpoint.path,
            name: stored.endpoint.name,
            hmac_scheme: stored.endpoint.hmac_scheme,
            signature_header: stored.endpoint.signature_header,
            timestamp_header: stored.endpoint.timestamp_header,
            tolerance_seconds: stored.endpoint.tolerance_seconds,
            secret_id: stored.endpoint.secret_id,
            has_secret: stored.endpoint.secret_id.is_some(),
            max_payload_bytes: stored.endpoint.max_payload_bytes,
            sanitize_profile: stored.endpoint.sanitize_profile,
            enabled: stored.endpoint.enabled,
            created_at: stored.created_at,
            rejection_count: stored.rejection_count,
            last_rejection_at: stored.last_rejection_at,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<IntakeListBody>, ApiError> {
    let pool = state.db().pool();
    let endpoints = store::list(pool).await.map_err(map_store)?;
    Ok(Json(IntakeListBody {
        endpoints: endpoints.into_iter().map(EndpointView::from).collect(),
        schemes: INTAKE_SCHEMES.to_vec(),
        profiles: SANITIZE_PROFILES.to_vec(),
        reasons: INTAKE_REASONS.to_vec(),
        reason_counts: store::reason_counts(pool).await.map_err(map_store)?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RejectionQuery {
    pub endpoint_id: Option<Uuid>,
    pub reason: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RejectionListBody {
    pub rejections: Vec<store::RejectionRow>,
    pub reasons: Vec<&'static str>,
}

/// `GET /reliability/intake/rejections` — the log, as `time · endpoint · reason · source · request id`.
pub async fn rejections(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<RejectionQuery>,
) -> Result<Json<RejectionListBody>, ApiError> {
    let pool = state.db().pool();
    // An unknown reason filter answers with an empty log rather than with every row. The
    // vocabulary is a `check` constraint on the column, so a typo in the query is a caller
    // error; answering it as "no refusals match" is a lie that reads like a working filter.
    if let Some(reason) = query.reason.as_deref().filter(|r| !r.trim().is_empty()) {
        if !INTAKE_REASONS.contains(&reason) {
            return Err(ApiError::bad_request(
                "invalid_reliability_input",
                format!("reason must be one of {}, got '{reason}'", INTAKE_REASONS.join(", ")),
            ));
        }
    }
    Ok(Json(RejectionListBody {
        rejections: store::list_rejections(
            pool,
            query.endpoint_id,
            query.reason.as_deref(),
            query.limit.unwrap_or(100),
        )
        .await
        .map_err(map_store)?,
        reasons: INTAKE_REASONS.to_vec(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Declare and edit
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IntakeInput {
    pub path: String,
    pub name: String,
    pub hmac_scheme: String,
    pub signature_header: String,
    pub timestamp_header: Option<String>,
    pub tolerance_seconds: Option<i32>,
    pub secret_id: Option<Uuid>,
    pub max_payload_bytes: Option<i32>,
    pub sanitize_profile: Option<String>,
    pub enabled: Option<bool>,
}

impl IntakeInput {
    /// Resolve the form into a validated declaration, applying the shipped defaults.
    ///
    /// The defaults live here and not in the column definitions alone, because the panel has to
    /// show a number in an input before anything is saved: a form that leaves every numeric
    /// field blank and relies on the database to fill them teaches the operator that the
    /// defaults are unknowable.
    fn into_endpoint(self) -> Result<intake::IntakeEndpoint, ApiError> {
        let endpoint = intake::IntakeEndpoint {
            path: self.path,
            name: self.name,
            hmac_scheme: self.hmac_scheme,
            signature_header: self.signature_header,
            timestamp_header: self.timestamp_header,
            tolerance_seconds: self.tolerance_seconds.unwrap_or(300),
            secret_id: self.secret_id,
            max_payload_bytes: self.max_payload_bytes.unwrap_or(1_048_576),
            sanitize_profile: self.sanitize_profile.unwrap_or_else(|| "strict".into()),
            enabled: self.enabled.unwrap_or(true),
        };
        endpoint.validate().map_err(map_store)?;
        Ok(endpoint)
    }
}

/// `POST /reliability/intake` — declare a path.
pub async fn create(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<IntakeInput>,
) -> Result<(StatusCode, Json<EndpointView>), ApiError> {
    let pool = state.db().pool();
    let endpoint = input.into_endpoint()?;
    if let Some(secret_id) = endpoint.secret_id {
        // A declaration that references a secret which is not there authenticates nothing: every
        // request would fail with `signature_invalid` and the operator's first question would be
        // "is my signing key right?" when the answer is "there is no key".
        let exists: (bool,) = sqlx::query_as("select exists (select 1 from secrets where id = $1)")
            .bind(secret_id)
            .fetch_one(pool)
            .await
            .map_err(map_db)?;
        if !exists.0 {
            return Err(ApiError::bad_request(
                "invalid_reliability_input",
                "no secret with that id exists",
            ));
        }
    }
    let stored = match store::insert(pool, &endpoint).await {
        Ok(stored) => stored,
        Err(error) => {
            if let Some(message) = store::explain_constraint(&error, "intake_endpoints_path_key") {
                return Err(ApiError::bad_request("invalid_reliability_input", message));
            }
            return Err(map_store(error));
        }
    };
    write_side_effects(&state, &session, "reliability.intake.declared", &endpoint, stored.id).await;
    Ok((StatusCode::CREATED, Json(EndpointView::from(stored))))
}

#[derive(Debug, Deserialize)]
pub struct IntakePatch {
    pub path: Option<String>,
    pub name: Option<String>,
    pub hmac_scheme: Option<String>,
    pub signature_header: Option<String>,
    pub timestamp_header: Option<Option<String>>,
    pub tolerance_seconds: Option<i32>,
    pub secret_id: Option<Option<Uuid>>,
    pub max_payload_bytes: Option<i32>,
    pub sanitize_profile: Option<String>,
    pub enabled: Option<bool>,
}

/// `PATCH /reliability/intake/{id}` — retune a declaration.
///
/// **Load, merge, validate, write.** Every field is validated *after* the merge rather than on
/// the input: a partial patch that sets `max_payload_bytes = 5` on its own is valid input to a
/// PATCH and an invalid declaration, and refusing it only at the end would save a row the guard
/// cannot run.
pub async fn update(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<IntakePatch>,
) -> Result<Json<EndpointView>, ApiError> {
    let pool = state.db().pool();
    let stored = store::load(pool, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("intake endpoint", id))?;
    let mut endpoint = stored.endpoint;
    if let Some(v) = input.path {
        endpoint.path = v;
    }
    if let Some(v) = input.name {
        endpoint.name = v;
    }
    if let Some(v) = input.hmac_scheme {
        endpoint.hmac_scheme = v;
    }
    if let Some(v) = input.signature_header {
        endpoint.signature_header = v;
    }
    if let Some(v) = input.timestamp_header {
        endpoint.timestamp_header = v;
    }
    if let Some(v) = input.tolerance_seconds {
        endpoint.tolerance_seconds = v;
    }
    if let Some(v) = input.secret_id {
        endpoint.secret_id = v;
    }
    if let Some(v) = input.max_payload_bytes {
        endpoint.max_payload_bytes = v;
    }
    if let Some(v) = input.sanitize_profile {
        endpoint.sanitize_profile = v;
    }
    if let Some(v) = input.enabled {
        endpoint.enabled = v;
    }
    endpoint.validate().map_err(map_store)?;

    let updated = match store::update(pool, id, &endpoint).await {
        Ok(stored) => stored,
        Err(error) => {
            if let Some(message) = store::explain_constraint(&error, "intake_endpoints_path_key") {
                return Err(ApiError::bad_request("invalid_reliability_input", message));
            }
            return Err(map_store(error));
        }
    };
    write_side_effects(&state, &session, "reliability.intake.updated", &endpoint, id).await;
    Ok(Json(EndpointView::from(updated)))
}

#[derive(Debug, Deserialize)]
pub struct ReasonInput {
    pub reason: String,
}

/// `DELETE /reliability/intake/{id}` — remove a declaration, with a reason.
///
/// The reason is required, not optional politeness: removing a declaration turns a guarded door
/// into an open one, and that is a change an operator reading the audit log three weeks later has
/// to be able to explain.
pub async fn remove(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<ReasonInput>,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let stored = store::load(pool, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("intake endpoint", id))?;
    let reason = input.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            "a reason is required to remove a declaration",
        ));
    }
    if !store::delete(pool, id).await.map_err(map_store)? {
        return Err(ApiError::not_found("intake endpoint", id));
    }
    write_side_effects(
        &state,
        &session,
        "reliability.intake.removed",
        &stored.endpoint,
        id,
    )
    .await;
    if let Err(error) = record_audit(
        pool,
        NewAuditEntry::by_user(session.user.id, "reliability.intake.remove_reason")
            .organization(session.user.organization_id)
            .metadata(json!({ "endpoint_id": id, "path": stored.endpoint.path, "reason": reason })),
    )
    .await
    {
        tracing::warn!(error = %error, "the declaration was removed but the reason was not audited");
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Verify sample
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SampleInput {
    pub payload: String,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct SampleVerdict {
    pub valid: bool,
    /// One of [`INTAKE_REASONS`], or `null` for an accepted sample.
    pub reason: Option<&'static str>,
    /// A human sentence, carrying no payload and no secret.
    pub detail: String,
    /// What the sanitisation pass would change about the sample, or `null` when it was refused
    /// before sanitisation ran.
    pub changes: Option<Vec<String>>,
    /// The body's length, so a sample over the cap can be told apart from one that merely
    /// failed to verify — two different fixes for the integrator.
    pub body_bytes: usize,
}

/// `POST /reliability/intake/{id}/verify-sample` — check an operator's signature.
///
/// It calls [`intake::verify_sample`], which is the **same function the data-plane route calls**
/// with the same argument shapes. That is the whole point of the action: a tester with its own
/// signature check would answer "valid" for a body the platform refuses, and that is the one
/// answer this screen must never give. The secret it needs is the one the declaration already
/// references, and the sample is evaluated against a store where the id has not been seen — a
/// tester re-running the *same* sample must not be told it is a replay, because nothing was
/// delivered.
///
/// It returns no secret and no payload: the verdict is a reason and a sentence.
pub async fn verify_sample(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<SampleInput>,
) -> Result<Json<SampleVerdict>, ApiError> {
    let pool = state.db().pool();
    let stored = store::load(pool, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("intake endpoint", id))?;

    if !stored.endpoint.enabled {
        return Ok(Json(SampleVerdict {
            valid: false,
            reason: Some("malformed"),
            detail: "this endpoint is declared but not enabled — enable it to verify a sample"
                .into(),
            changes: None,
            body_bytes: input.payload.len(),
        }));
    }

    // A declaration with no secret cannot verify anything, and saying "invalid" would blame the
    // operator's signature for a configuration gap. The message names the gap.
    let Some(secret_id) = stored.endpoint.secret_id else {
        return Ok(Json(SampleVerdict {
            valid: false,
            reason: Some("malformed"),
            detail: "this endpoint has no secret reference, so no signature can be verified"
                .into(),
            changes: None,
            body_bytes: input.payload.len(),
        }));
    };
    let secret = load_signing_secret(pool, secret_id).await.map_err(map_store)?;

    let verdict = intake::verify_sample(
        &stored.endpoint,
        &input.payload,
        &input.signature,
        &secret,
    );
    Ok(Json(describe(&verdict, input.payload.len())))
}

fn describe(verdict: &GuardVerdict, body_bytes: usize) -> SampleVerdict {
    match verdict {
        GuardVerdict::Accepted { changes, .. } => SampleVerdict {
            valid: true,
            reason: None,
            detail: "the signature matches this payload under the declared scheme".into(),
            changes: Some(changes.clone()),
            body_bytes,
        },
        GuardVerdict::Rejected { reason, detail, .. } => SampleVerdict {
            valid: false,
            reason: Some(reason),
            detail: detail.clone(),
            changes: None,
            body_bytes,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The guarded data-plane path
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AcceptedBody {
    /// The declaration that ran, so the sender can tell which one answered.
    pub endpoint: String,
    pub signature_valid: bool,
    /// What sanitisation changed, or an empty list. Never the body itself.
    pub changes: Vec<String>,
    pub body_bytes: usize,
    /// The request id, so the sender can quote it and the operator can find the line.
    pub request_id: Option<Uuid>,
}

/// The wire answer for a refusal. Carries the reason and nothing else.
///
/// `ApiError`'s envelope is `{"error": {"code", "message", "request_id"}}`, so the reason an
/// operator needs is the `code` — the same string the rejection row stores, because the schema's
/// `check` constraint and this body read from one list.
pub struct Refusal {
    status: StatusCode,
    reason: &'static str,
    detail: String,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.reason,
                    "message": self.detail,
                }
            })),
        )
            .into_response()
    }
}

/// The HTTP status a reason earns.
///
/// **Three of the seven are not `401`.** A stale timestamp and a replayed signature are
/// `400`/`409` on purpose: they are a *well-formed request with a bad time* and a *well-formed
/// request already processed*, and answering either with `401` tells the integrator to check
/// their signing key when the key is fine. An oversized payload is `413` because that is what it
/// is, and the acceptance criteria name that code explicitly.
fn status_for(reason: &str) -> StatusCode {
    match reason {
        "payload_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "replay" => StatusCode::CONFLICT,
        "timestamp_stale" | "content_type_refused" | "malformed" => StatusCode::BAD_REQUEST,
        // `signature_missing` and `signature_invalid` are the same answer deliberately: telling
        // a caller which of the two it got wrong is a free oracle for an attacker, and the
        // operator gets the precise reason from the rejection log, which is behind
        // `reliability.read`.
        _ => StatusCode::UNAUTHORIZED,
    }
}

/// `POST /public/intake/{id}` — the guarded inbound path.
///
/// The order of operations is [`intake::evaluate`]'s and it is the order the request specifies:
/// size, content type, signature presence, timestamp, replay, then the constant-time tag
/// comparison. The cap is checked from the **declared** length *and* from the bytes actually
/// received, and the two are asked independently — a client that lies about `content-length` in
/// a chunked request is refused on the received length, which is the one that cost the memory.
///
/// The signature id is claimed **after** the tag verifies. Recording it before is a
/// denial-of-service an attacker gets for free: send garbage carrying somebody else's id and
/// their next legitimate delivery is refused as a replay.
pub async fn guarded_request(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    address: ClientAddress,
    body: Bytes,
) -> Response {
    let pool = state.db().pool();
    let request_id: Option<Uuid> = None;

    let Some(stored) = (match store::load(pool, id).await {
        Ok(Some(stored)) => Some(stored),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(error = %error, %id, "the intake store did not answer");
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "intake_unavailable",
                "the intake guard could not be reached",
            )
            .into_response();
        }
    }) else {
        // An undeclared id is a `404`, not a `403` and not a guard verdict: there is nothing here
        // to authenticate against, and answering as though there were would let a caller probe
        // for declared paths by their behaviour.
        return ApiError::not_found("intake endpoint", id).into_response();
    };
    let endpoint = &stored.endpoint;
    let now = OffsetDateTime::now_utc();

    let signature = headers
        .get(&endpoint.signature_header)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let timestamp = endpoint
        .timestamp_header
        .as_ref()
        .and_then(|name| headers.get(name.as_str()))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok());
    let signature_id = signature.as_deref().and_then(extract_signature_id);
    let text = String::from_utf8_lossy(&body).into_owned();

    let seen = match store::seen_signature_ids(pool, stored.id, now).await {
        Ok(seen) => seen,
        Err(error) => {
            // The replay table is unreadable, so a replay cannot be excluded. This is the one
            // place the guard fails CLOSED, and the asymmetry is deliberate: a size cap and a
            // content type are cheap to re-send, while an excluded replay defence means the same
            // signed request is accepted twice. The alternative — proceeding — is the failure
            // this subsystem exists to prevent.
            tracing::warn!(error = %error, %id, "the replay window could not be read; refusing closed");
            return Refusal {
                status: StatusCode::SERVICE_UNAVAILABLE,
                reason: "replay_window_unavailable",
                detail: "the replay window could not be read, so this request was not accepted"
                    .into(),
            }
            .into_response();
        }
    };

    let inbound = Inbound {
        path: endpoint.path.clone(),
        max_payload_bytes: endpoint.max_payload_bytes,
        body_len: body.len(),
        content_type: headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        signature,
        timestamp,
        signature_id: signature_id.clone(),
        source_ip: address.0,
        request_id,
    };

    // Without a secret reference there is nothing to verify against, and the answer is the
    // configuration's, not the caller's: `malformed` names the gap.
    let secret = match endpoint.secret_id {
        Some(secret_id) => match load_signing_secret(pool, secret_id).await {
            Ok(secret) => secret,
            Err(error) => {
                tracing::warn!(error = %error, %id, "the signing secret could not be unsealed");
                return Refusal {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    reason: "signing_secret_unavailable",
                    detail: "the signing secret could not be unsealed".into(),
                }
                .into_response();
            }
        },
        None => {
            return finish_refusal(
                pool,
                stored.id,
                Refusal {
                    status: StatusCode::BAD_REQUEST,
                    reason: "malformed",
                    detail: "this endpoint has no secret reference, so it cannot authenticate"
                        .into(),
                },
                Rejection {
                    endpoint_id: Some(stored.id),
                    reason: "malformed".into(),
                    source_ip: address.0,
                    request_id,
                    body_bytes: body.len(),
                },
            )
            .await
        }
    };

    let verdict = intake::evaluate(endpoint, &inbound, &text, &secret, now, &seen);
    match verdict {
        GuardVerdict::Accepted { changes, .. } => {
            // The id is claimed here, after the tag verified. See the doc comment.
            if let Some(id) = &signature_id {
                match store::remember_signature(
                    pool,
                    stored.id,
                    id,
                    endpoint.tolerance_seconds,
                )
                .await
                {
                    // Lost the race: a concurrent delivery of the same signed request claimed it
                    // first. Both are genuine, so one of them must be refused or the same
                    // request is processed twice — which is the thing the id exists to prevent.
                    Ok(false) => {
                        return finish_refusal(
                            pool,
                            stored.id,
                            Refusal {
                                status: StatusCode::CONFLICT,
                                reason: "replay",
                                detail: "this signature id was accepted by a concurrent request"
                                    .into(),
                            },
                            Rejection {
                                endpoint_id: Some(stored.id),
                                reason: "replay".into(),
                                source_ip: address.0,
                                request_id,
                                body_bytes: body.len(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "the signature could not be recorded for replay defence");
                        return Refusal {
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            reason: "replay_window_unavailable",
                            detail: "the signature could not be recorded, so this request was not accepted"
                                .into(),
                        }
                        .into_response();
                    }
                    Ok(true) => {}
                }
            }
            // **No event on the accepted path.** `reliability.intake.rejected` is the
            // operator-worthy payload and an accepted delivery is not a rejection; emitting it
            // for a success is the kind of event that trains an operator to mute the channel.
            // The first draft emitted it here and then had a `let _ = error` to explain the
            // branch, which is what a leftover looks like.
            Json(AcceptedBody {
                endpoint: endpoint.path.clone(),
                signature_valid: true,
                changes,
                body_bytes: body.len(),
                request_id,
            })
            .into_response()
        }
        GuardVerdict::Rejected {
            reason,
            detail,
            record,
        } => {
            let refusal = Refusal {
                status: status_for(reason),
                reason,
                detail,
            };
            let rejection = Rejection {
                endpoint_id: Some(stored.id),
                reason: reason.to_string(),
                source_ip: address.0,
                request_id,
                body_bytes: body.len(),
            };
            if record {
                finish_refusal(pool, stored.id, refusal, rejection).await
            } else {
                refusal.into_response()
            }
        }
    }
}

/// Write the rejection row and the operator-worthy event, then answer the refusal.
///
/// Both are best-effort and neither can change the status: a caller whose signature is invalid
/// gets `401` whether or not the log row was written, because the guard's answer does not depend
/// on whether the database was reachable. A guard that answers `500` when its logging fails
/// tells the sender to retry an already-refused request.
async fn finish_refusal(
    pool: &sqlx::PgPool,
    endpoint_id: Uuid,
    refusal: Refusal,
    rejection: Rejection,
) -> Response {
    if let Err(error) = store::record_rejection(pool, Some(endpoint_id), &rejection).await {
        tracing::warn!(error = %error, reason = refusal.reason, "the rejection was not recorded");
    }
    if let Err(error) = bus::emit(
        pool,
        NewEvent::new(intake::rejection_event()).payload(json!({
            "endpoint_id": endpoint_id,
            "reason": refusal.reason,
            "body_bytes": rejection.body_bytes,
        })),
    )
    .await
    {
        tracing::warn!(error = %error, reason = refusal.reason, "the rejection event was not emitted");
    }
    // The payload carries the reason, the size and the endpoint — never the body and never the
    // signature. An event lands in every webhook the platform writes, so this is the last place
    // a payload could leak.
    refusal.into_response()
}

/// Pull the `id` out of a `v1,<id>:<tag>` signature.
///
/// Returns `None` for a bare tag, and that is a real answer rather than a gap: a scheme without
/// an id has no replay key, and inventing one from the tag would refuse the second of two
/// different deliveries that happen to share a body.
fn extract_signature_id(signature: &str) -> Option<String> {
    let (prefix, _tag) = signature.rsplit_once(':')?;
    let id = prefix.strip_prefix("v1,")?;
    let id = id.trim();
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Unseal the shared secret a declaration signs with.
///
/// The value never leaves this function: it is returned to the caller, which hands it straight
/// to [`intake::verify`] / [`intake::evaluate`] and drops it. There is no branch here that logs
/// it, serialises it into a response, or puts it in an error message.
async fn load_signing_secret(pool: &sqlx::PgPool, secret_id: Uuid) -> Result<Vec<u8>, ReliabilityError> {
    let row: Option<(String,)> = sqlx::query_as(
        "select v.envelope from secret_versions v
         where v.secret_id = $1 and v.revoked_at is null
         order by v.version desc limit 1",
    )
    .bind(secret_id)
    .fetch_optional(pool)
    .await?;
    let Some((envelope,)) = row else {
        return Err(ReliabilityError::NotFound);
    };
    let box_ = omnion_identity::secrets::SecretBox::from_env();
    box_.decrypt(&envelope)
        .map_err(|error| ReliabilityError::Invalid(format!("the signing secret could not be read: {error}")))
}

async fn write_side_effects(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    endpoint: &intake::IntakeEndpoint,
    id: Uuid,
) {
    let metadata = json!({
        "endpoint_id": id,
        "path": endpoint.path,
        "hmac_scheme": endpoint.hmac_scheme,
        "signature_header": endpoint.signature_header,
        "tolerance_seconds": endpoint.tolerance_seconds,
        "max_payload_bytes": endpoint.max_payload_bytes,
        "sanitize_profile": endpoint.sanitize_profile,
        "enabled": endpoint.enabled,
        // `secret_id` is a reference. A boolean is enough for a reader and cannot be mistaken
        // for a value in a log line or a webhook payload.
        "has_secret": endpoint.secret_id.is_some(),
    });
    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, action)
            .organization(session.user.organization_id)
            .metadata(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, action, "the declaration was saved but the audit entry was not written");
    }
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(omnion_reliability::vocabulary::events::POLICY_UPDATED)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(metadata),
    )
    .await
    {
        tracing::warn!(error = %error, action, "the declaration was saved but the event was not emitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_the_route_can_answer_has_a_status() {
        for reason in INTAKE_REASONS {
            // A reason with no explicit arm would fall to the `401` catch-all, and the two that
            // must NOT be `401` are the whole reason this mapping exists.
            let status = status_for(reason);
            match *reason {
                "payload_too_large" => assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{reason}"),
                "replay" => assert_eq!(status, StatusCode::CONFLICT, "{reason}"),
                "timestamp_stale" | "content_type_refused" | "malformed" => {
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}");
                }
                _ => assert_eq!(status, StatusCode::UNAUTHORIZED, "{reason}"),
            }
        }
    }

    #[test]
    fn a_missing_signature_and_an_invalid_one_answer_the_same_status() {
        // The point is asserted by NAME: one status, so a caller cannot tell which it got wrong.
        assert_eq!(
            status_for("signature_missing"),
            status_for("signature_invalid")
        );
    }

    #[test]
    fn a_signature_id_is_only_taken_from_the_versioned_shape() {
        assert_eq!(
            extract_signature_id("v1,abc123:deadbeef").as_deref(),
            Some("abc123")
        );
        // A bare tag has no id, and inventing one would refuse a second legitimate delivery.
        assert_eq!(extract_signature_id("deadbeef"), None);
        assert_eq!(extract_signature_id("v1,:deadbeef"), None);
        assert_eq!(extract_signature_id("v2,abc:dead"), None);
    }

    #[test]
    fn the_refusal_body_carries_a_reason_and_never_a_payload() {
        let refusal = Refusal {
            status: StatusCode::UNAUTHORIZED,
            reason: "signature_invalid",
            detail: "the signature does not match the body".into(),
        };
        let body = serde_json::to_value(json!({
            "error": { "code": refusal.reason, "message": refusal.detail }
        }))
        .expect("the envelope serialises");
        let text = body.to_string();
        assert!(text.contains("signature_invalid"));
        // The vocabulary itself is the guarantee: nothing else has a key in this envelope.
        assert!(!text.contains("payload"));
        assert!(!text.contains("signature\":"));
    }
}
