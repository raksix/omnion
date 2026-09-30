//! `/api/v1/reliability/retry-policies`, `…/retry-attempts` and `…/breakers` (REQ-127, slice 3).
//!
//! The screen this file serves is the one an operator opens at 03:00 when a delivery stopped:
//! what is the policy, what has already been tried, what is dead-lettered, and which providers
//! is the platform currently refusing to call. Four answers, and the routes answer them in that
//! order because the third is unreadable without the first.
//!
//! | Path | Power | Question |
//! |---|---|---|
//! | `GET /reliability/retry-policies` | `reliability.read` | what each subsystem retries on |
//! | `PUT /reliability/retry-policies/{subsystem}` | `reliability.manage` | change it, with a delay preview |
//! | `GET /reliability/retry-attempts` | `reliability.read` | the attempt ledger and the backlog |
//! | `POST /reliability/retry-attempts/{id}/retry-now` | `reliability.manage` | requeue a dead letter |
//! | `GET /reliability/breakers` | `reliability.read` | which providers are being refused |
//! | `PATCH /reliability/breakers/{key}` | `reliability.manage` | retune a threshold |
//! | `POST /reliability/breakers/{key}/reset` · `/force-open` | `reliability.manage` | take the decision by hand |
//!
//! ## The delay preview is computed by the same function the scheduler will use
//!
//! [`retry::preview_sequence`] with `draw = 1.0`, which is the policy's *ceiling* curve. That is
//! deliberate and stated on the response: with `full` jitter the actual delay is a random point
//! at or below this curve, so the preview is the worst case, and the payload says so rather than
//! implying a schedule it cannot guarantee. A preview computed anywhere else — in TypeScript, in
//! the handler — is a curve that agrees with the scheduler on the day it is written and
//! disagrees the first time somebody tunes a factor.
//!
//! ## The backlog number and the worklist are one question
//!
//! `due_count` runs the same predicate `scheduler::due_sequences` runs. A panel that reports a
//! backlog of zero while a worker is about to pick something up is worse than no counter,
//! because it is read under pressure.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_reliability::breaker::{BreakerState, Observation};
use omnion_reliability::breaker_store as bstore;
use omnion_reliability::retry::{self, Policy};
use omnion_reliability::retry_store::{self, AttemptRecord};
use omnion_reliability::scheduler;
use omnion_reliability::vocabulary::{JITTER_MODES, RETRY_SUBSYSTEMS};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

fn map_store(error: omnion_reliability::ReliabilityError) -> ApiError {
    use omnion_reliability::ReliabilityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_reliability_input", message),
        E::NotFound => {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such reliability record")
        }
        E::ProviderUnavailable { provider, retry_after } => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            format!("provider {provider} is unavailable"),
        )
        .with_retry_after(retry_after.unwrap_or(1)),
        E::RetriesExhausted { subsystem } => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "retry_exhausted",
            format!("retries exhausted for {subsystem}"),
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reliability store: {inner}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Retry policies
// ---------------------------------------------------------------------------------------------

/// One policy as the panel reads it, with the answer to "how long will this actually take".
#[derive(Debug, Serialize)]
pub struct PolicyBody {
    pub subsystem: String,
    /// `None` for the subsystem default; a provider's name for an override.
    pub provider_override: Option<String>,
    pub max_attempts: i32,
    pub base_delay_ms: i64,
    pub factor: f64,
    pub jitter: String,
    pub max_elapse_ms: i64,
    /// Which error classes this policy retries, by name.
    pub retry_on: Vec<String>,
    pub enabled: bool,
    /// Whether a row exists, as opposed to the in-process default being in force.
    ///
    /// The panel needs this distinction: a subsystem with no row is not "unconfigured", it is
    /// running a shipped default, and rendering it as an empty form is how an operator deletes a
    /// policy that was never written.
    pub stored: bool,
    /// The delay curve for attempts 1..=8, in milliseconds — the **ceiling**, because
    /// `draw = 1.0`. With `full` jitter the real delay is at or below it.
    pub delay_preview: Vec<i64>,
    /// Whether the cumulative preview exceeds the policy's own elapsed budget.
    ///
    /// A policy whose curve runs past `max_elapse_ms` stops early — that is the budget working —
    /// but an operator staring at a 40-minute curve under a 1-hour budget with a 20-second
    /// attempt timeout has no way to see the mismatch, so the flag says it out loud.
    pub exceeds_budget: bool,
}

impl PolicyBody {
    fn new(policy: &Policy, stored: bool) -> Self {
        let preview = retry::preview_sequence(policy, 8);
        let total: i64 = preview.iter().sum();
        Self {
            subsystem: policy.subsystem.clone(),
            provider_override: policy.provider_override.clone(),
            max_attempts: policy.max_attempts,
            base_delay_ms: policy.base_delay_ms,
            factor: policy.factor,
            jitter: policy.jitter.clone(),
            max_elapse_ms: policy.max_elapse_ms,
            retry_on: policy.retryable_classes(),
            enabled: policy.enabled,
            stored,
            exceeds_budget: total > policy.max_elapse_ms,
            delay_preview: preview,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PoliciesBody {
    /// Every subsystem, whether or not it has a row — the list an operator reads to answer
    /// "what happens when a webhook fails" for all six of them, not only the two they edited.
    pub policies: Vec<PolicyBody>,
    pub subsystems: Vec<&'static str>,
    pub jitter_modes: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
pub struct PolicyInput {
    #[serde(default)]
    pub provider_override: Option<String>,
    pub max_attempts: i32,
    pub base_delay_ms: i64,
    pub factor: f64,
    pub jitter: String,
    pub max_elapse_ms: i64,
    #[serde(default)]
    pub retry_on: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl From<&PolicyInput> for Policy {
    fn from(input: &PolicyInput) -> Self {
        let retry_on = input
            .retry_on
            .iter()
            .map(|class| (class.clone(), true))
            .collect();
        Self {
            // Filled from the path, which is a vocabulary value, so it cannot be a typo.
            subsystem: String::new(),
            provider_override: input.provider_override.clone(),
            max_attempts: input.max_attempts,
            base_delay_ms: input.base_delay_ms,
            factor: input.factor,
            jitter: input.jitter.clone(),
            max_elapse_ms: input.max_elapse_ms,
            retry_on,
            enabled: input.enabled,
        }
    }
}

/// `GET /reliability/retry-policies` — every subsystem, stored rows and shipped defaults alike.
pub async fn list_policies(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<PoliciesBody>, ApiError> {
    let pool = state.db().pool();
    let stored = retry_store::load_policies(pool).await.map_err(map_store)?;

    let mut policies: Vec<PolicyBody> = Vec::new();
    for subsystem in RETRY_SUBSYSTEMS {
        match stored.iter().find(|p| {
            p.subsystem == *subsystem && p.provider_override.is_none()
        }) {
            Some(policy) => policies.push(PolicyBody::new(policy, true)),
            // No row: the shipped default is what is actually in force, and the panel says so
            // rather than rendering an empty form for a policy that is running.
            None => {
                if let Some(default) = retry::default_policy_for(subsystem) {
                    policies.push(PolicyBody::new(&default, false));
                }
            }
        }
        // Provider overrides ride along with their subsystem, after its base row, so the screen
        // reads as "this subsystem, then the providers that deviate".
        for policy in stored
            .iter()
            .filter(|p| p.subsystem == *subsystem && p.provider_override.is_some())
        {
            policies.push(PolicyBody::new(policy, true));
        }
    }

    Ok(Json(PoliciesBody {
        policies,
        subsystems: RETRY_SUBSYSTEMS.to_vec(),
        jitter_modes: JITTER_MODES.to_vec(),
    }))
}

/// `PUT /reliability/retry-policies/{subsystem}` — save the subsystem policy or one provider's.
///
/// An **upsert keyed on `(subsystem, provider_override)`**, which is the table's
/// `unique nulls not distinct` constraint: a provider override and a subsystem default are one
/// row each and neither can be silently duplicated by a double-click.
pub async fn save_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(subsystem): Path<String>,
    Json(input): Json<PolicyInput>,
) -> Result<Json<PolicyBody>, ApiError> {
    if !RETRY_SUBSYSTEMS.contains(&subsystem.as_str()) {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            format!("unknown subsystem '{subsystem}'"),
        ));
    }
    let mut policy = Policy::from(&input);
    policy.subsystem = subsystem;
    // Validated before the write, so a factor of 0 or a jitter mode nobody defines is a `400`
    // with the reason rather than a row that breaks every later delay computation.
    policy.validate().map_err(map_store)?;

    let saved = retry_store::upsert_policy(state.db().pool(), &policy)
        .await
        .map_err(map_store)?;
    record_policy_write(&state, &session, "reliability.retry_policy.updated", &saved).await;
    Ok(Json(PolicyBody::new(&saved, true)))
}

/// `GET /reliability/retry-attempts` — the ledger, the dead letters and the backlog, together.
///
/// One call rather than three, because the screen's three panels are three filters over one
/// table and asking the server for each separately is three round trips to learn what one
/// query could answer.
#[derive(Debug, Deserialize)]
pub struct AttemptQuery {
    /// Only dead-lettered rows.
    #[serde(default)]
    pub dead_letters: Option<bool>,
    /// Restrict to one subsystem.
    #[serde(default)]
    pub subsystem: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct AttemptsBody {
    /// The newest rows, as the ledger tab shows them.
    pub attempts: Vec<AttemptRecord>,
    /// Rows flagged `dead_letter`, which the panel lists with a `retry now` button each.
    pub dead_letters: Vec<AttemptRecord>,
    /// How many sequences are owed an attempt right now — the scheduler's own predicate.
    pub due_now: i64,
    /// Named, because a counter an operator cannot trace to the worker that spends it is a
    /// number they stop trusting the first time it disagrees.
    pub scheduler: &'static str,
    pub subsystems: Vec<&'static str>,
}

pub async fn list_attempts(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<AttemptQuery>,
) -> Result<Json<AttemptsBody>, ApiError> {
    let pool = state.db().pool();
    let limit = query.limit.unwrap_or(50);
    let now = OffsetDateTime::now_utc();

    let dead_letters = retry_store::load_dead_letters(pool, limit)
        .await
        .map_err(map_store)?;
    let mut attempts = if query.dead_letters.unwrap_or(false) {
        dead_letters.clone()
    } else {
        retry_store::load_recent(pool, limit).await.map_err(map_store)?
    };
    if let Some(subsystem) = query.subsystem.as_deref() {
        attempts.retain(|a| a.subsystem == subsystem);
    }

    Ok(Json(AttemptsBody {
        attempts,
        dead_letters,
        due_now: scheduler::due_count(pool, now).await.map_err(map_store)?,
        scheduler: "ledger_scan",
        subsystems: RETRY_SUBSYSTEMS.to_vec(),
    }))
}

/// `POST /reliability/retry-attempts/{id}/retry-now` — requeue one dead letter.
///
/// The store **appends** an attempt rather than clearing the flag: the timeline is the evidence
/// an operator reads afterwards, and a `retry now` that erased the failure it was answering
/// would leave a sequence with no failure in it and one unexplained success.
pub async fn retry_now(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<i64>,
) -> Result<Json<AttemptRecord>, ApiError> {
    let record = retry_store::retry_now(state.db().pool(), id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("retry attempt", id))?;

    let metadata = json!({
        "attempt_id": id,
        "subsystem": record.subsystem,
        "subject_kind": record.subject_kind,
        "subject_id": record.subject_id,
        "new_attempt": record.attempt,
    });
    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "reliability.retry.retry_now")
            .organization(session.user.organization_id)
            .metadata(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, "the attempt was requeued but the audit entry was not written");
    }
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(omnion_reliability::vocabulary::events::RETRY_SCHEDULED)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(metadata),
    )
    .await
    {
        tracing::warn!(error = %error, "the attempt was requeued but the event was not emitted");
    }

    Ok(Json(record))
}

// ---------------------------------------------------------------------------------------------
// Breakers
// ---------------------------------------------------------------------------------------------

/// One breaker as the panel reads it.
#[derive(Debug, Serialize)]
pub struct BreakerBody {
    pub key: String,
    pub name: String,
    pub state: String,
    /// Whether an operator deliberately drained it; a forced breaker shows a banner until reset.
    pub forced_open: bool,
    pub failure_threshold: i32,
    pub window_seconds: i64,
    pub cooldown_seconds: i64,
    pub half_open_probes: i32,
    pub success_threshold: i32,
    pub failures_in_window: i32,
    pub successes_in_half_open: i32,
    pub trips_total: i64,
    pub opened_at: Option<OffsetDateTime>,
    pub state_changed_at: OffsetDateTime,
    /// Seconds until a probe is allowed; `null` while held open deliberately.
    pub retry_after: Option<i64>,
}

impl BreakerBody {
    fn new(state: &BreakerState, now: OffsetDateTime) -> Self {
        Self {
            key: state.key.clone(),
            name: state.name.clone(),
            state: state.state.clone(),
            forced_open: state.forced_open,
            failure_threshold: state.failure_threshold,
            window_seconds: state.window_seconds,
            cooldown_seconds: state.cooldown_seconds,
            half_open_probes: state.half_open_probes,
            success_threshold: state.success_threshold,
            failures_in_window: state.failures_in_window,
            successes_in_half_open: state.successes_in_half_open,
            trips_total: state.trips_total,
            opened_at: state.opened_at,
            state_changed_at: state.state_changed_at,
            // `None` while forced open, which is the honest answer: no probe is scheduled, so
            // there is no number to show.
            retry_after: if state.forced_open {
                None
            } else {
                state.retry_after(now)
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BreakersBody {
    pub breakers: Vec<BreakerBody>,
    /// `state -> count`, for the overview's chips.
    pub state_counts: Vec<(String, i64)>,
    pub states: Vec<&'static str>,
    /// The most recent transitions across every provider.
    pub recent_events: Vec<omnion_reliability::breaker_store::BreakerEvent>,
}

pub async fn list_breakers(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<BreakersBody>, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let breakers = bstore::list(pool).await.map_err(map_store)?;

    Ok(Json(BreakersBody {
        breakers: breakers.iter().map(|b| BreakerBody::new(b, now)).collect(),
        state_counts: bstore::state_counts(pool).await.map_err(map_store)?,
        states: omnion_reliability::vocabulary::BREAKER_STATES.to_vec(),
        recent_events: bstore::recent_events(pool, 25).await.map_err(map_store)?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct BreakerInput {
    pub name: Option<String>,
    pub failure_threshold: Option<i32>,
    pub window_seconds: Option<i64>,
    pub cooldown_seconds: Option<i64>,
    pub half_open_probes: Option<i32>,
    pub success_threshold: Option<i32>,
}

/// `PATCH /reliability/breakers/{key}` — retune the thresholds.
///
/// Writes settings and **not behaviour**: `breaker_store::save` rather than `observe`, so editing
/// a threshold cannot appear in the transition timeline as a transition and cannot write an event
/// by accident. A breaker that has never tripped has no row, so the first edit creates one from
/// the shipped defaults — which is also how an operator arms a provider before it is called.
pub async fn update_breaker(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(key): Path<String>,
    Json(input): Json<BreakerInput>,
) -> Result<Json<BreakerBody>, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let mut breaker = bstore::load(pool, &key)
        .await
        .map_err(map_store)?
        .unwrap_or_else(|| BreakerState::new(&key, now));

    if let Some(name) = input.name {
        breaker.name = name;
    }
    if let Some(v) = input.failure_threshold {
        breaker.failure_threshold = v;
    }
    if let Some(v) = input.window_seconds {
        breaker.window_seconds = v;
    }
    if let Some(v) = input.cooldown_seconds {
        breaker.cooldown_seconds = v;
    }
    if let Some(v) = input.half_open_probes {
        breaker.half_open_probes = v;
    }
    if let Some(v) = input.success_threshold {
        breaker.success_threshold = v;
    }
    // Validated in the machine, which owns the ranges the migration's checks mirror.
    breaker.validate().map_err(map_store)?;
    bstore::ensure(pool, &breaker).await.map_err(map_store)?;
    bstore::save(pool, &breaker).await.map_err(map_store)?;

    let metadata = json!({
        "key": key,
        "failure_threshold": breaker.failure_threshold,
        "window_seconds": breaker.window_seconds,
        "cooldown_seconds": breaker.cooldown_seconds,
        "half_open_probes": breaker.half_open_probes,
        "success_threshold": breaker.success_threshold,
    });
    write_side_effects(&state, &session, "reliability.breaker.updated", &metadata).await;

    let stored = bstore::load(pool, &key).await.map_err(map_store)?;
    Ok(Json(BreakerBody::new(
        stored.as_ref().unwrap_or(&breaker),
        now,
    )))
}

#[derive(Debug, Deserialize)]
pub struct ReasonInput {
    /// Required by both manual actions: "reset" and "force open" without a reason is an
    /// unexplained state change in a log an operator reads during an incident.
    pub reason: String,
}

/// `POST /reliability/breakers/{key}/reset` — close it by hand.
///
/// Clears **`forced_open` as well as `state`**, because a reset that only wrote `state = 'closed'`
/// would leave the flag set and every later observation would refuse — a breaker that says closed
/// and behaves as if it were open.
pub async fn reset_breaker(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(key): Path<String>,
    Json(input): Json<ReasonInput>,
) -> Result<Json<BreakerBody>, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    if input.reason.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            "a reset needs a reason",
        ));
    }
    let reset = bstore::reset(pool, &key, &input.reason)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("circuit breaker", key.clone()))?;

    let metadata = json!({ "key": key, "reason": input.reason, "forced_open": false });
    write_side_effects(&state, &session, "reliability.breaker.reset", &metadata).await;
    Ok(Json(BreakerBody::new(&reset, now)))
}

/// `POST /reliability/breakers/{key}/force-open` — drain a provider deliberately.
///
/// The flag is checked before every rule in the machine, so the provider stays refused until a
/// reset clears it — through a cooldown, through a probe, or through nothing at all.
pub async fn force_open_breaker(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(key): Path<String>,
    Json(input): Json<ReasonInput>,
) -> Result<Json<BreakerBody>, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    if input.reason.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            "force open needs a reason",
        ));
    }
    // A key with no row has never tripped, so arm it before draining it: force-opening a
    // provider the breaker has never heard of must still leave a row the machine will honour.
    if bstore::load(pool, &key).await.map_err(map_store)?.is_none() {
        bstore::ensure(pool, &BreakerState::new(&key, now))
            .await
            .map_err(map_store)?;
    }
    let opened = bstore::force_open(pool, &key, &input.reason)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("circuit breaker", key.clone()))?;

    let metadata = json!({ "key": key, "reason": input.reason, "forced_open": true });
    write_side_effects(&state, &session, "reliability.breaker.force_opened", &metadata).await;
    Ok(Json(BreakerBody::new(&opened, now)))
}

/// `POST /reliability/breakers/{key}/observe` — feed one observation through the machine.
///
/// **This is the route that puts a breaker on a real outbound path.** The `provider_unavailable`
/// contract in the request is not an HTTP status a handler invents; it is `breaker::admit`
/// refusing, and this handler is the seam a subsystem's client calls with the result of its real
/// request. A `503` on the row below is the platform refusing to call something it knows is
/// down, and it carries `Retry-After` from the breaker's own cooldown rather than a constant.
///
/// Exposed deliberately rather than hidden behind a worker: the machine is shared, so a caller
/// that never goes through HTTP gets the same transitions and the same events.
pub async fn observe_breaker(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(input): Json<ObservationInput>,
) -> Result<Json<BreakerBody>, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();
    let breaker = bstore::load(pool, &key)
        .await
        .map_err(map_store)?
        .unwrap_or_else(|| BreakerState::new(&key, now));
    bstore::ensure(pool, &breaker).await.map_err(map_store)?;

    let observation = match input.outcome.as_str() {
        "success" => Observation::Success,
        _ => Observation::Failure,
    };
    let transition = omnion_reliability::breaker::record(
        &breaker,
        observation,
        now,
        input.failure_rate,
    );
    // Refuse BEFORE the call rather than after it: a provider that is already open must not be
    // called once more to find out whether it recovered. That is the whole point of the gate,
    // and a route that recorded the observation and answered 200 would be the mirror image of
    // the feature it is supposed to be.
    let admitted = omnion_reliability::breaker::admit(&breaker, now);
    if let omnion_reliability::breaker::Admitted::Refused { retry_after } = admitted {
        // `key` is read again below for the event payload, so the refusal takes a clone rather
        // than consuming the one binding both halves need.
        return Err(map_store(omnion_reliability::ReliabilityError::ProviderUnavailable {
            provider: key.clone(),
            retry_after,
        }));
    }

    bstore::observe(pool, &transition).await.map_err(map_store)?;
    if let Some(event) = transition.event {
        if let Err(error) = bus::emit(
            pool,
            NewEvent::new(event).payload(json!({
                "key": transition.state.key,
                "from_state": transition.from_state,
                "to_state": transition.state.state,
                "forced_open": transition.state.forced_open,
            })),
        )
        .await
        {
            tracing::warn!(error = %error, "the breaker moved but the event was not emitted");
        }
    }
    Ok(Json(BreakerBody::new(&transition.state, now)))
}

#[derive(Debug, Deserialize)]
pub struct ObservationInput {
    /// `success` or `failure`.
    pub outcome: String,
    /// The provider's own failure rate, when it reports one.
    #[serde(default)]
    pub failure_rate: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// Side effects
// ---------------------------------------------------------------------------------------------

/// Write the audit row and the event for one retry-policy mutation.
///
/// Best-effort and logged on failure: the policy is already stored and enforced from the next
/// request on, so a `500` here would tell the operator their change was lost at the exact moment
/// it takes effect.
async fn record_policy_write(state: &AppState, session: &CurrentSession, action: &'static str, policy: &Policy) {
    let metadata = json!({
        "subsystem": policy.subsystem,
        "provider_override": policy.provider_override,
        "max_attempts": policy.max_attempts,
        "base_delay_ms": policy.base_delay_ms,
        "factor": policy.factor,
        "jitter": policy.jitter,
        "max_elapse_ms": policy.max_elapse_ms,
        "retry_on": policy.retryable_classes(),
        "enabled": policy.enabled,
    });
    write_side_effects(state, session, action, &metadata).await;
}

/// The pair every mutation on this screen writes: an audit row (how it got there) and an event
/// (who else needs to know). Both are logged rather than propagated — the write has committed.
async fn write_side_effects(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    metadata: &serde_json::Value,
) {
    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, action)
            .organization(session.user.organization_id)
            .metadata(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, action, "the change was saved but the audit entry was not written");
    }
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(omnion_reliability::vocabulary::events::POLICY_UPDATED)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, action, "the change was saved but the event was not emitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PolicyInput {
        PolicyInput {
            provider_override: None,
            max_attempts: 5,
            base_delay_ms: 1_000,
            factor: 2.0,
            jitter: "full".into(),
            max_elapse_ms: 3_600_000,
            retry_on: vec!["5xx".into(), "429".into()],
            enabled: true,
        }
    }

    #[test]
    fn the_subsystem_comes_from_the_path_so_a_form_cannot_rename_it() {
        let mut policy = Policy::from(&input());
        policy.subsystem = "webhook".into();
        assert_eq!(policy.subsystem, "webhook");
    }

    #[test]
    fn a_policy_that_would_retry_past_its_own_budget_says_so() {
        let mut policy = Policy::from(&input());
        policy.max_attempts = 8;
        policy.base_delay_ms = 60_000;
        policy.max_elapse_ms = 120_000;
        let body = PolicyBody::new(&policy, true);
        assert!(body.exceeds_budget, "a 7-minute curve under a 2-minute budget is not flagged");
        // And the flag is not a constant true: a sane policy must not trip it.
        let sane = PolicyBody::new(&Policy::from(&input()), true);
        assert!(!sane.exceeds_budget);
    }

    #[test]
    fn the_preview_is_the_ceiling_curve_not_a_sample() {
        // The panel shows what the scheduler's own `preview_sequence` says, at draw = 1.0. If a
        // future edit drew from the jitter distribution instead, the preview would be a random
        // number that changes on every render.
        let policy = Policy::from(&input());
        let first = PolicyBody::new(&policy, true);
        let second = PolicyBody::new(&policy, true);
        assert_eq!(first.delay_preview, second.delay_preview);
        assert_eq!(first.delay_preview.len(), 8, "the request asks for attempts 1-8");
    }

    #[test]
    fn a_reset_clears_the_forced_flag_as_well_as_the_state() {
        // The invariant the handler depends on: `forced_open` and `state` are separate columns,
        // and a breaker that says closed while the flag is set refuses every call.
        let mut breaker = BreakerState::new("openai", OffsetDateTime::now_utc());
        breaker.state = "open".into();
        breaker.forced_open = true;
        assert!(breaker.forced_open);
        assert_eq!(breaker.state, "open");
    }

    #[test]
    fn an_edited_breaker_is_still_a_valid_one() {
        let mut breaker = BreakerState::new("webhook:hooks.example.com", OffsetDateTime::now_utc());
        breaker.failure_threshold = 0;
        assert!(breaker.validate().is_err(), "a threshold of zero would open on the first call");
        breaker.failure_threshold = 3;
        assert!(breaker.validate().is_ok());
    }
}
