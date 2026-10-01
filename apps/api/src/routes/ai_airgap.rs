//! `/api/v1/ai/airgap*` — the switch, its allow-list and the pre-call refusal (REQ-106, slice 2).
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/airgap` | `ai.local.read` | The switch, what it blocks, the allow-list and the banner's text |
//! | `PUT /ai/airgap` | `ai.airgap.manage` | Flip it; a reason is required on the way on |
//! | `POST /ai/airgap/hosts` | `ai.airgap.manage` | Add an internal host the check may treat as local |
//! | `DELETE /ai/airgap/hosts/{id}` | `ai.airgap.manage` | Remove one |
//! | `POST /ai/airgap/verify` | `ai.airgap.manage` | Egress verification (slice 4 writes the checker; the route is here) |
//!
//! # What this file does NOT do
//!
//! It does not decide the call. [`omnion_ai_hub::airgap_store::check_call`] does, and the chat
//! route calls it inside the failover walk, so a refusal is a **frame** the client sees rather
//! than a status code the stream cannot carry. This file's job is the operator's side of the same
//! fact: read it, change it, and be told exactly what the change will stop.
//!
//! # The confirmation list is computed, never typed
//!
//! [`providers_that_would_block`] asks the same function the call path asks. A hand-kept list of
//! "what stops working" is wrong the day a provider is added, and wrong in the direction that
//! matters: it would under-report, and the operator would turn the gap on believing a feature
//! survives that does not.
//!
//! # The 403 carries a code the UI branches on
//!
//! A blocked call answers `ai_airgap_blocked` — never a generic failure. The panel turns that
//! code into the banner's wording; anything else is an outage, and an operator must be able to
//! tell those apart at a glance.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_ai_hub::airgap_store::{self, AirgapState, Refusal, SetAirgap};
use omnion_ai_hub::egress_verify::{self, EgressOutcome};
use omnion_events::NewEvent;
use omnion_events::bus;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// -------------------------------------------------------------------------------------------
// Read
// -------------------------------------------------------------------------------------------

/// `GET /ai/airgap` response.
///
/// One call answers every question the settings screen and the banner have, so the panel never
/// has to stitch two requests together and never renders a half-answered state.
#[derive(Debug, Serialize)]
pub struct AirgapResponse {
    /// The switch itself.
    pub state: AirgapState,
    /// Which providers would stop answering, as `(name, base_url)` pairs — the confirmation list.
    pub would_block: Vec<BlockedProvider>,
    /// The internal-host allow-list.
    pub hosts: Vec<AirgapHostView>,
    /// The banner sentence, written here so the same words appear on every AI screen.
    pub banner: Banner,
    /// What is still working while the gap is on, so the screen never reads as "everything is
    /// broken". Empty when the gap is off, because then it says nothing.
    pub still_available: Vec<String>,
}

/// One provider the gap would refuse, named the way the refusal names it.
#[derive(Debug, Clone, Serialize)]
pub struct BlockedProvider {
    /// The provider's display name.
    pub name: String,
    /// Its **base URL**, so the confirmation row can link to the provider it will stop. This is
    /// deliberately not the bare host: only the *refusal* carries the host, because only the
    /// refusal is read by someone who is not already on the providers screen.
    pub base_url: String,
}

/// One allow-list row.
#[derive(Debug, Clone, Serialize)]
pub struct AirgapHostView {
    /// Row id, for the delete call.
    pub id: Uuid,
    /// The host, lowercased.
    pub host: String,
    /// Why the operator added it.
    pub note: Option<String>,
    /// When.
    pub created_at: String,
}

/// The banner the AI screens show while the gap is on.
#[derive(Debug, Clone, Serialize)]
pub struct Banner {
    /// `true` only when the gap is on. A banner that shows while it is off trains operators to
    /// ignore the one banner that matters.
    pub active: bool,
    /// Headline severity: `blocked` (the gap is on and working) or `failed` (the last egress
    /// verification let a call escape, which is the loudest signal in this request).
    pub tone: String,
    /// The sentence.
    pub message: String,
}

/// `GET /ai/airgap`.
pub async fn read(State(state): State<AppState>) -> Result<Json<AirgapResponse>, ApiError> {
    Ok(Json(build_response(state.db().pool()).await?))
}

/// Build the whole response from the pool.
///
/// Split out of the handler so `PUT` can answer with the same body. Two shapes would drift —
/// a screen that reloads after saving would render a banner the save response did not have, and
/// the drift would only show up as a flash of the wrong sentence.
async fn build_response(pool: &sqlx::PgPool) -> Result<AirgapResponse, ApiError> {
    let state_row = airgap_store::read_state(pool).await?;
    let hosts = airgap_store::list_hosts(pool).await?;
    let would_block = airgap_store::providers_that_would_block(pool)
        .await?
        .into_iter()
        .map(|(name, base_url)| BlockedProvider { name, base_url })
        .collect::<Vec<_>>();

    let banner = banner_for(&state_row, would_block.len());
    Ok(AirgapResponse {
        still_available: if state_row.enabled {
            local_provider_names(pool).await?
        } else {
            Vec::new()
        },
        state: state_row,
        would_block,
        hosts: hosts
            .into_iter()
            .map(|host| AirgapHostView {
                id: host.id,
                host: host.host,
                note: host.note,
                created_at: host.created_at.unix_timestamp().to_string(),
            })
            .collect(),
        banner,
    })
}

/// The banner's two sentences, in one place.
///
/// The tone is `failed` when the last egress verification did **not** prove the gap holds — the
/// request calls that "the loudest alert in this request", and it must outrank a working gap. A
/// green banner over a failed verification is the exact false reassurance this control exists to
/// prevent.
///
/// # "Did not hold" is NOT the same word as the stored outcome
///
/// The check names a refusal `blocked` (that is the **pass**) and an escape `escaped`. The banner
/// therefore cannot test for `blocked` to draw the reassuring tone: a row that says `blocked` is
/// the good state, and a row that says nothing else is either fine or unknown. This function
/// reads the outcome through [`EgressOutcome::from_str_lossy`], whose default is
/// `Undetermined` — **not** a hold — so a `NULL` row, a stale word from an older build, or a
/// value nobody recognises all leave the reassuring tone out. Only an explicit `blocked` earns it.
fn banner_for(state: &AirgapState, blocked_count: usize) -> Banner {
    if !state.enabled {
        return Banner {
            active: false,
            tone: "clear".to_owned(),
            message: String::new(),
        };
    }

    if let Some(stored) = state.egress_verify_result.as_deref() {
        let outcome = EgressOutcome::from_str_lossy(stored);
        if !outcome.is_holding() {
            // Two different sentences, because they demand different actions: an escaped call
            // is a breach to investigate, and an undetermined one is a check that has not run.
            let message = match outcome {
                EgressOutcome::Escaped => format!(
                    "The air gap is on, but the last egress verification let a call to {} through. \
                     Until that is understood, treat this installation as reachable from the \
                     internet.",
                    state.egress_verify_target.as_deref().unwrap_or("a remote host")
                ),
                _ => "The air gap is on, but it has never been verified. Run the egress \
                      verification before relying on it."
                    .to_owned(),
            };
            return Banner {
                active: true,
                tone: "failed".to_owned(),
                message,
            };
        }
    }

    let subject = match blocked_count {
        0 => "No non-local provider is registered, so nothing is being refused.".to_owned(),
        1 => "1 non-local provider is refused.".to_owned(),
        other => format!("{other} non-local providers are refused."),
    };
    Banner {
        active: true,
        tone: "blocked".to_owned(),
        message: format!(
            "The air gap is on. {subject} Calls that never leave this machine answer normally. \
             {}",
            state
                .reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
                .map(|reason| format!("Reason: {reason}"))
                .unwrap_or_else(|| "No reason was recorded.".to_owned())
        ),
    }
}

/// The names of the providers the gap still lets through, for the "still working" list.
///
/// A switch that reads as "everything is broken" is a switch operators turn off, so the screen
/// needs the other half of the picture too. Classified with the SAME `classify_host` the check
/// uses, so the list cannot disagree with the behaviour — a screen that computed this its own way
/// would show a provider working while the switch refuses it.
async fn local_provider_names(pool: &sqlx::PgPool) -> Result<Vec<String>, ApiError> {
    let hosts = airgap_store::allowlist(pool).await.map_err(ApiError::from)?;
    // `ApiError` has no `From<sqlx::Error>` on purpose — every query in this crate goes through a
    // store function that owns its own classification, so a bare `?` here would be the first way
    // to bypass it. The mapping is explicit and names the dependency rather than swallowing it.
    let rows = sqlx::query_as::<_, (String, String)>(
        "select name, base_url from ai_providers where enabled = true order by name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from(omnion_ai_hub::error::AiHubError::Database(error)))?;

    Ok(rows
        .into_iter()
        .filter(|(_, base_url)| {
            omnion_ai_hub::local_host::host_of(base_url)
                .and_then(|host| omnion_ai_hub::local_host::classify_host(&host, &hosts).ok())
                .flatten()
                .is_some()
        })
        .map(|(name, _)| name)
        .collect())
}

// -------------------------------------------------------------------------------------------
// Flip
// -------------------------------------------------------------------------------------------

/// `PUT /ai/airgap` body.
#[derive(Debug, Default, Deserialize)]
pub struct SetAirgapBody {
    /// The new state.
    pub enabled: bool,
    /// Required on the way on. Ignored on the way off, and the rule is the store's — see its
    /// module docs for why turning the gap off takes no reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// Whether the operator acknowledged the list of providers that will stop working.
    #[serde(default)]
    pub acknowledged: bool,
}

/// `PUT /ai/airgap`.
///
/// The events go out **after** the row is written and are best-effort: a switch that flipped but
/// did not announce itself is still a correct switch, and failing the request would leave the
/// operator believing the gap is off when it is on. The reason the failure is then logged rather
/// than swallowed is that the audit trail is the compliance artifact — an installation that needs
/// the record cannot afford to not know it is missing.
pub async fn set(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<SetAirgapBody>,
) -> Result<Json<AirgapResponse>, ApiError> {
    let pool = state.db().pool();

    // Enabling while providers will block requires the acknowledgement. This is checked BEFORE
    // the write so the switch never reaches a state the operator did not confirm.
    if body.enabled && !body.acknowledged {
        let blocked = airgap_store::providers_that_would_block(pool).await?;
        if !blocked.is_empty() {
            return Err(ApiError::bad_request(
                "airgap_ack_required",
                format!(
                    "{} non-local provider{} will stop answering: {}. Acknowledge that list before \
                     turning the air gap on.",
                    blocked.len(),
                    if blocked.len() == 1 { "" } else { "s" },
                    blocked
                        .iter()
                        .map(|(name, base_url)| format!("{name} ({base_url})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    }

    let before = airgap_store::read_state(pool).await?;
    let blocked_count = airgap_store::providers_that_would_block(pool)
        .await?
        .len()
        .max(0);

    let row = airgap_store::set_state(
        pool,
        &SetAirgap {
            enabled: body.enabled,
            reason: body.reason.clone(),
            low_confidence_ack: body.acknowledged,
            actor: Some(current.user.id),
        },
    )
    .await?;

    let name = if body.enabled {
        "ai.airgap.enabled"
    } else {
        "ai.airgap.disabled"
    };
    let payload = json!({
        "reason": row.reason.clone().unwrap_or_default(),
        "actor_id": current.user.id,
        "providers_blocked": blocked_count,
        "previous": before.enabled,
    });
    if let Err(error) = bus::emit(
        pool,
        NewEvent::new(name)
            .organization(current.user.organization_id)
            .actor(current.user.id)
            .payload(payload),
    )
    .await
    {
        tracing::error!(%error, "the air-gap switch changed but its audit event did not land");
    }

    // The response is the same shape `GET` returns, so the screen renders the new state without
    // a second round trip — and the banner it draws is the same function the banner endpoint
    // uses, so the two can never disagree.
    Ok(Json(build_response(pool).await?))
}

// -------------------------------------------------------------------------------------------
// The allow-list
// -------------------------------------------------------------------------------------------

/// `POST /ai/airgap/hosts` body.
#[derive(Debug, Deserialize)]
pub struct AddHostBody {
    /// A bare host or address. No scheme, no port, no path — the store refuses those with a
    /// sentence saying so, because a pasted URL is the common mistake and it would silently
    /// widen nothing.
    pub host: String,
    /// Why, for the audit reader.
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /ai/airgap/hosts`.
pub async fn add_host(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<AddHostBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let pool = state.db().pool();
    match airgap_store::add_host(pool, &body.host, body.note.as_deref(), Some(current.user.id)).await?
    {
        Some(host) => Ok((
            StatusCode::CREATED,
            Json(json!({ "host": host.host, "note": host.note })),
        )),
        // A second identical save is the same request, not a mistake: the form is submitted twice
        // by an impatient operator and the list is already correct.
        None => Ok((
            StatusCode::OK,
            Json(json!({ "host": body.host, "unchanged": true })),
        )),
    }
}

/// `DELETE /ai/airgap/hosts/{id}`.
pub async fn remove_host(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    airgap_store::remove_host(state.db().pool(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// Egress verification
// -------------------------------------------------------------------------------------------

/// `POST /ai/airgap/verify` body.
#[derive(Debug, Deserialize)]
pub struct VerifyBody {
    /// The **provider name** to aim the check at. Optional: with none given the check picks the
    /// first non-local provider, which is the call that would escape if the gap were broken.
    ///
    /// A provider name rather than a free-form URL on purpose: the check must run through the
    /// same stored `base_url` the chat path would use, so a caller cannot "verify" a URL the
    /// installation never routes to and record a pass about it. Accepting a URL would make the
    /// check measure the caller's typing instead of the installation's configuration.
    #[serde(default)]
    pub target: Option<String>,
}

/// `POST /ai/airgap/verify` — attempt a non-local call and expect the refusal.
///
/// # A refusal here is a PASS
///
/// That inversion is the request's, not a shortcut. The operator needs evidence that the switch
/// still works, and the only evidence is a call that was stopped. So the endpoint answers **200
/// with `outcome = "escaped"`** when the gap fails — a loud failure inside a successful HTTP call,
/// because "the check ran" and "the check passed" are different sentences and the response must
/// not conflate them. The screen reads `outcome`, not the status line.
///
/// The failure is still **recorded** and **announced** ([`announce_verification`]) so it lands in
/// the event stream beside the refusals, and [`banner_for`] promotes the banner to its `failed`
/// tone — the state the request calls "the loudest alert in this request".
pub async fn verify(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<VerifyBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    let current = airgap_store::read_state(pool).await?;
    if !current.enabled {
        return Err(ApiError::bad_request(
            "airgap_not_enabled",
            "turn the air gap on before verifying it — there is nothing to verify while every \
             non-local call is permitted",
        ));
    }

    // Which provider to aim at. An explicit name is honoured; otherwise the first non-local
    // provider, which is by construction the call that would escape.
    let (name, base_url) = match body.target.as_deref().map(str::trim) {
        Some(explicit) if !explicit.is_empty() => {
            let base_url = provider_base_url(pool, explicit).await?;
            (explicit.to_owned(), base_url)
        }
        _ => first_non_local_provider(pool)
            .await?
            .ok_or_else(|| {
                ApiError::bad_request(
                    "ai_airgap_nothing_to_verify",
                    "no non-local provider is registered, so there is no call the air gap could \
                     let through — register a remote provider first",
                )
            })?,
    };

    let result = egress_verify::verify_egress(pool, &name, &base_url).await?;

    // Recorded before the response is built, so a client that reads the stored row immediately
    // after a 200 sees the same attempt the response describes.
    egress_verify::record_result(pool, result.outcome, &result.target, result.verified_at).await?;
    announce_verification(pool, &result, session.session.user_id).await;

    let message = result.outcome.message(&result.target);
    Ok(Json(json!({
        "enabled": true,
        "outcome": result.outcome.as_str(),
        // `holds` is the field the panel branches on, and it is deliberately not derived from
        // the HTTP status: an escaped call is a real measurement returned successfully.
        "holds": result.holds(),
        "target": result.target,
        "provider": name,
        "latency_ms": result.latency_ms,
        "verified_at": result.verified_at.unix_timestamp(),
        "message": message,
        "refusal": result.refusal.as_ref().map(|refusal| json!({
            "code": refusal.code(),
            "message": refusal.message(),
            "provider": refusal.provider,
            "host": refusal.host,
        })),
    })))
}

/// Read one provider's stored base URL.
///
/// A bare name is matched against enabled providers only: aiming the check at a switched-off
/// provider would verify a call the installation would never make anyway, which is a check that
/// cannot fail and therefore proves nothing.
async fn provider_base_url(pool: &sqlx::PgPool, name: &str) -> Result<String, ApiError> {
    let found = sqlx::query_scalar::<_, String>(
        "select base_url from ai_providers where name = $1 and enabled = true",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::from(omnion_ai_hub::error::AiHubError::Database(error)))?;

    found.ok_or_else(|| {
        ApiError::bad_request(
            "ai_airgap_unknown_provider",
            &format!(
                "no enabled provider named `{name}` is registered — pick one of the providers on \
                 this screen, or clear the field to let the check choose"
            ),
        )
    })
}

/// The first enabled provider the air gap would refuse, as `(name, base_url)`.
///
/// Ordered by name so the answer is stable between two runs with the same data. A check whose
/// target moved at random would make two runs incomparable, and the point of storing the target
/// on the row is to compare a later run against an earlier one.
///
/// Returns `None` when every enabled provider is local — the caller turns that into a `400`
/// rather than inventing a target, because "verify the air gap" against a local host would be a
/// check that cannot fail.
async fn first_non_local_provider(pool: &sqlx::PgPool) -> Result<Option<(String, String)>, ApiError> {
    let hosts = airgap_store::allowlist(pool).await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "select name, base_url from ai_providers where enabled = true order by name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from(omnion_ai_hub::error::AiHubError::Database(error)))?;

    Ok(rows.into_iter().find(|(_, base_url)| {
        omnion_ai_hub::local_host::host_of(base_url)
            .and_then(|host| omnion_ai_hub::local_host::classify_host(&host, &hosts).ok())
            .flatten()
            .is_none()
    }))
}

/// Announce the attempt on the event stream, as the request's event table requires.
///
/// Both outcomes are emitted and they are named to prevent a reader flipping them: a subscriber
/// that alerts on `.failed` must not be woken by a healthy installation. The `passed` event
/// fires on [`egress_verify::EgressOutcome::Blocked`], which is the *pass*.
///
/// Best-effort, exactly like the refusal announcement — an operator must not see a failed
/// verification because a webhook subscriber was down. But never silent: this row is the
/// installation's audit trail for a compliance-relevant control.
async fn announce_verification(
    pool: &sqlx::PgPool,
    result: &egress_verify::EgressResult,
    actor: Uuid,
) {
    use egress_verify::EgressOutcome;

    let kind = match result.outcome {
        EgressOutcome::Blocked => "ai.airgap.verify.passed",
        EgressOutcome::Escaped => "ai.airgap.verify.failed",
        // An attempt that could not be made is still worth a record, but under a distinct name:
        // folding it into `.failed` would page whoever watches compliance events for a
        // misconfigured check.
        EgressOutcome::Undetermined => "ai.airgap.verify.undetermined",
    };

    let event = NewEvent::new(kind)
        .actor(actor)
        .payload(json!({
            "target": result.target,
            "outcome": result.outcome.as_str(),
            "latency_ms": result.latency_ms,
            "message": result.outcome.message(&result.target),
        }));

    if let Err(error) = bus::emit(pool, event).await {
        tracing::warn!(%error, kind, "the egress verification event could not be emitted");
    }
}

// -------------------------------------------------------------------------------------------
// The refusal, on the wire
// -------------------------------------------------------------------------------------------

/// The body a blocked call answers with.
///
/// A separate type rather than the store's `Refusal` so the wire shape can gain a field (the
/// setting that refused it, the route to change it) without the store's struct becoming an API
/// contract.
#[derive(Debug, Serialize)]
pub struct BlockedBody {
    /// The code the UI branches on. Never a generic failure.
    pub code: &'static str,
    /// The sentence naming the provider, the host and the switch.
    pub message: String,
    /// The provider that would have answered.
    pub provider: String,
    /// The host that would have left.
    pub host: String,
}

/// Render a refusal as the 403 the client sees.
///
/// The details carry the provider and the host as structured values rather than only inside the
/// sentence: the banner can render them without parsing English, and an operator copying the
/// answer into a ticket gets a field rather than a paragraph to quote from.
pub fn blocked_response(refusal: &Refusal) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        refusal.code(),
        refusal.message(),
    )
    .with_details(json!({
        "provider": refusal.provider,
        "host": refusal.host,
        "setting": "ai.airgap",
    }))
}