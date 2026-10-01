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
/// The tone is `failed` when the last egress verification passed *nothing* — the request calls
/// that "the loudest alert in this request", and it must outrank a working gap. A green banner
/// over a failed verification is the exact false reassurance this control exists to prevent.
fn banner_for(state: &AirgapState, blocked_count: usize) -> Banner {
    if !state.enabled {
        return Banner {
            active: false,
            tone: "clear".to_owned(),
            message: String::new(),
        };
    }

    if state.egress_verify_result.as_deref() == Some("failed") {
        return Banner {
            active: true,
            tone: "failed".to_owned(),
            message:
                "The air gap is on, but the last egress verification let a call through. Until that \
                 is understood, treat this installation as reachable from the internet."
                    .to_owned(),
        };
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
// Egress verification (the endpoint; slice 4 writes the checker behind it)
// -------------------------------------------------------------------------------------------

/// `POST /ai/airgap/verify` body.
#[derive(Debug, Deserialize)]
pub struct VerifyBody {
    /// The host to aim the check at. Optional: with none given the check picks the first
    /// non-local provider, which is the call that would escape if the gap were broken.
    #[serde(default)]
    pub target: Option<String>,
}

/// `POST /ai/airgap/verify`.
///
/// The check itself is slice 4's; the route exists now so the settings screen can render the
/// panel and its last result without a second endpoint appearing later. It answers the last
/// recorded result rather than pretending to run one — a "verified" badge nobody ran is worse
/// than no badge.
pub async fn verify(
    State(state): State<AppState>,
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
    let _ = body.target;

    Ok(Json(json!({
        "enabled": true,
        "verified_at": current.egress_verified_at.map(|at| at.unix_timestamp()),
        "target": current.egress_verify_target,
        "result": current.egress_verify_result,
        "implemented": false,
        "message": "the live egress check lands with the doctor slice; the last recorded result is \
                    shown above",
    })))
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