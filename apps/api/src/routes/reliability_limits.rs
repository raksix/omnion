//! `/api/v1/reliability/*` — the limits screen of the reliability centre (REQ-127, slice 1).
//!
//! Three routes answer the operator's three questions, and the order matters because the third
//! one is worthless without the first two:
//!
//! | Path | Power | Question it answers |
//! |---|---|---|
//! | `GET /reliability/rate-limits` | `reliability.read` | which policies exist, in resolution order |
//! | `POST /reliability/rate-limits` · `PATCH` · `DELETE …/{id}` | `reliability.manage` | write one |
//! | `POST /reliability/rate-limits/evaluate` | `reliability.manage` | **which policy would win for this request, and how much is left** |
//! | `GET /reliability/rate-limits/refusals` | `reliability.read` | who has been refused, and how often |
//!
//! ## The dry-run is the reason this screen exists
//!
//! The request asks for "a dry-run form that takes scope, target, route and returns the resolved
//! policy plus the remaining budget — a tool an operator can use under pressure". That is only
//! true if the dry-run's answer IS the middleware's answer, so it calls
//! [`omnion_reliability::limiter_redis::enforce`] — the same function, with the same counter, on
//! the same policy list the request path resolves. **The one thing it does not do is increment:**
//! the budget is read with [`peek`], never spent, because a tester that consumes the budget it
//! measures is how a diagnostic tool becomes an outage.
//!
//! ## A `route`-scoped policy is reported, not silently dropped
//!
//! The middleware runs before the router publishes `MatchedPath`, so it has no route template and
//! a `route`-scoped policy cannot match a request it sees. Rather than hide those rows — an
//! operator who wrote a policy and cannot find it will assume the platform ignored them — the
//! list response carries an explicit `enforced_here` flag per policy and the dry-run says so in
//! its own answer. A feature the panel offers and the platform does not enforce is the
//! "documented but unreachable" shape this request's sibling has produced four times.
//!
//! ## `reliability.*` permissions are added to the catalogue
//!
//! Four keys, the read/manage split the request's API table uses: `reliability.read`,
//! `reliability.manage`, and — for the intake endpoints of slice 4 — `reliability.intake.manage`,
//! which is declared here rather than when it is first used so the catalogue is complete at the
//! moment the routes start referencing it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_reliability::limits::{LimitPolicy, Subject, Verdict};
use omnion_reliability::limiter_redis::FailMode;
use omnion_reliability::vocabulary::events::POLICY_UPDATED;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The limiter this screen edits, named in every response so a `429` and a table row can be
/// joined. It is not the REQ-012 gateway limiter and the panel must never imply it is.
const LIMITER_NAME: &str = "platform_budget";

/// Map a crate error onto the API surface.
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
// Bodies
// ---------------------------------------------------------------------------------------------

/// One policy as the panel reads it, with the resolution answer attached.
#[derive(Debug, Serialize)]
pub struct PolicyBody {
    /// The row's id, or `None` for a policy that could not be stored.
    pub id: Option<Uuid>,
    /// Operator-facing name.
    pub name: String,
    /// One of the four scopes.
    pub scope: String,
    /// The subject this policy is about, or `None` for every subject in the scope.
    pub target_id: Option<String>,
    /// A route TEMPLATE, never a literal path.
    pub route_pattern: Option<String>,
    /// Requests allowed per window.
    pub limit_count: i32,
    /// The window the limit is spent against.
    pub window_seconds: i64,
    /// Headroom inside the same window.
    pub burst: i32,
    /// `limit + burst`, so a table never makes the reader do the addition on one row.
    pub ceiling: i64,
    /// Lower wins within one scope.
    pub priority: i32,
    /// Whether the row ships with the platform.
    pub is_default: bool,
    /// Whether it is enforced.
    pub enabled: bool,
    /// **Whether THIS layer can enforce it.**
    ///
    /// `false` for a `route`-scoped policy, because the platform limiter runs before the router
    /// publishes the matched route and has no template to match on. Carrying this on the row is
    /// what stops an operator from believing a budget that is stored but not spent.
    pub enforced_here: bool,
}

impl PolicyBody {
    fn new(policy: &LimitPolicy) -> Self {
        Self {
            id: policy.id,
            name: policy.name.clone(),
            scope: policy.scope.clone(),
            target_id: policy.target_id.clone(),
            route_pattern: policy.route_pattern.clone(),
            limit_count: policy.limit_count,
            window_seconds: policy.window_seconds,
            burst: policy.burst,
            ceiling: policy.ceiling(),
            priority: policy.priority,
            is_default: policy.is_default,
            enabled: policy.enabled,
            enforced_here: policy.scope != "route",
        }
    }
}

/// The policy list as the panel reads it.
#[derive(Debug, Serialize)]
pub struct PoliciesBody {
    /// The policies, **in resolution order** — the order `pick` walks them, so a table showing
    /// the winning policy first is showing the resolver's own order rather than a sort.
    pub policies: Vec<PolicyBody>,
    /// The scope vocabulary, so the form's dropdown is built from the same list the check
    /// constraint enforces rather than from a second hand-written copy.
    pub vocabulary: Vec<&'static str>,
    /// Which limiter these policies govern, so a `429` and a table row can be joined.
    pub limiter: &'static str,
    /// The mode this deployment fails in when the counter is unreachable, read from the
    /// installed layer rather than from a config file — the request's risk note asks for exactly
    /// that ("the panel must state which mode is active rather than letting the choice hide in a
    /// config file").
    pub fail_mode: &'static str,
}

/// A write.
#[derive(Debug, Deserialize)]
pub struct PolicyInput {
    /// Operator-facing name.
    pub name: String,
    /// One of the four scopes.
    pub scope: String,
    /// The subject, or `None` for every subject in the scope.
    #[serde(default)]
    pub target_id: Option<String>,
    /// A route template.
    #[serde(default)]
    pub route_pattern: Option<String>,
    /// Requests allowed per window.
    pub limit_count: i32,
    /// The window in seconds.
    pub window_seconds: i64,
    /// Headroom inside the window.
    #[serde(default)]
    pub burst: i32,
    /// Lower wins within one scope.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// Whether it is enforced.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// `serde` cannot call a const for a default, so the two are functions with the same names the
/// SQL column carries. `100` is the migration's default and the value the migration seeds use, so
/// a form that omits the field produces the same row as the seed.
fn default_priority() -> i32 {
    100
}
fn default_true() -> bool {
    true
}

impl From<&PolicyInput> for LimitPolicy {
    fn from(input: &PolicyInput) -> Self {
        Self {
            // A write never carries an id: the route decides whether this is a create or an
            // update, and a body that could name its own row is a body that could rewrite another
            // policy's scope by answering a different URL.
            id: None,
            name: input.name.clone(),
            scope: input.scope.clone(),
            target_id: input.target_id.clone(),
            route_pattern: input.route_pattern.clone(),
            limit_count: input.limit_count,
            window_seconds: input.window_seconds,
            burst: input.burst,
            priority: input.priority,
            is_default: false,
            enabled: input.enabled,
        }
    }
}

/// A dry-run request: the same four facts the middleware reads off a request.
#[derive(Debug, Deserialize)]
pub struct EvaluateRequest {
    /// Which scope the operator is asking about, or `None` to let the resolver choose.
    #[serde(default)]
    pub scope: Option<String>,
    /// The user's id, when asking about the user scope.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// The address, when asking about the IP scope.
    #[serde(default)]
    pub ip: Option<String>,
    /// The route template, when asking about the route scope.
    #[serde(default)]
    pub route: Option<String>,
    /// The counter to assume, when explaining a refusal a caller is already seeing.
    #[serde(default)]
    pub count: Option<i64>,
}

/// The dry-run's answer.
#[derive(Debug, Serialize)]
pub struct EvaluateBody {
    /// The winning policy, or `None` when no policy applies.
    pub policy: Option<PolicyBody>,
    /// The verdict, from the same function the middleware calls.
    pub verdict: Verdict,
    /// The counter's reading, and whether it was readable at all.
    pub counted: CountedBody,
    /// The window the budget is spent against, so a tester can say "in this window".
    pub window_start: Option<String>,
    /// The Redis key the counter lives under, for somebody reading a dump.
    pub counter_key: Option<String>,
    /// The failure mode this deployment is in, restated on the dry-run because a `429` an
    /// operator cannot reproduce is a `429` they will widen the wrong document over.
    pub fail_mode: &'static str,
}

/// The counter's reading, without lying about it.
#[derive(Debug, Serialize)]
pub struct CountedBody {
    /// What the counter stood at before the request being described.
    pub count: i64,
    /// Whether the counter was readable. `false` means the numbers below are not a measurement.
    pub authoritative: bool,
}

/// The refusals, as the panel reads them.
#[derive(Debug, Serialize)]
pub struct RefusalsBody {
    /// The rollups, newest window first.
    pub refusals: Vec<RefusalBody>,
    /// How many refusals landed in the window the tile reports, summed across scopes.
    pub last_24_hours: i64,
}

/// One rollup.
#[derive(Debug, Serialize)]
pub struct RefusalBody {
    /// Which scope was refused.
    pub scope: String,
    /// The subject, or `None` when the scope has none.
    pub target_id: Option<String>,
    /// The route the policy was scoped to, or `(any route)`.
    pub route: String,
    /// The window these refusals belong to.
    pub window_start: String,
    /// How many were refused in it.
    pub refusals: i32,
    /// When the last one landed.
    pub last_refusal_at: String,
}

/// The read of the refusals list.
#[derive(Debug, Deserialize)]
pub struct RefusalsQuery {
    /// How many rollups to return; clamped to the shared page ceiling.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Format a timestamp the way the rest of the platform does.
fn format_offset(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /reliability/rate-limits` — the policies, in resolution order.
pub async fn list_policies(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<PoliciesBody>, ApiError> {
    let policies = omnion_reliability::store::load_policies(state.db().pool())
        .await
        .map_err(map_store)?;
    // Every policy, not only the enabled ones: a disabled row is the answer to "why is nothing
    // limiting this subject", and a list that hides it sends the operator hunting for a policy
    // that exists and is switched off. The ORDER is the resolver's, not a sort.
    let all = omnion_reliability::store::load_all_policies(state.db().pool())
        .await
        .map_err(map_store)?;
    let _ = policies;

    Ok(Json(PoliciesBody {
        policies: all
            .iter()
            .map(PolicyBody::new)
            .collect::<Vec<_>>(),
        vocabulary: omnion_reliability::vocabulary::RATE_SCOPES.to_vec(),
        limiter: LIMITER_NAME,
        fail_mode: crate::reliability_middleware::installed()
            .map_or(FailMode::default().as_str(), |l| l.fail_mode().as_str()),
    }))
}

/// `POST /reliability/rate-limits` — create or replace one policy.
///
/// **Upsert rather than insert**, because the table carries
/// `unique nulls not distinct (scope, target_id, route_pattern)`: two rows for "every user, every
/// route" are the same policy written twice, and a `POST` that refused the second one would make
/// a typo look like a validation failure. The write is followed by a reload so the **next**
/// request is decided by the numbers that were just saved — without it the middleware would keep
/// enforcing whatever it read at boot and the panel's own dry-run would answer differently from
/// the layer that refuses.
pub async fn create_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<PolicyInput>,
) -> Result<Json<PolicyBody>, ApiError> {
    let policy: LimitPolicy = (&input).into();
    // Validated in the store, on the way into the table — a window of zero would divide by zero
    // on the first request that matched it.
    let saved = omnion_reliability::store::insert_policy(state.db().pool(), &policy)
        .await
        .map_err(map_store)?;

    record_policy_write(&state, &session, "reliability.rate_limits.created", &saved).await;
    crate::reliability_middleware::reload_from_store(&state).await;
    Ok(Json(PolicyBody::new(&saved)))
}

/// `PATCH /reliability/rate-limits/{id}` — edit one policy.
pub async fn update_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<PolicyInput>,
) -> Result<Json<PolicyBody>, ApiError> {
    let policy: LimitPolicy = (&input).into();
    let saved = omnion_reliability::store::update_policy(state.db().pool(), id, &policy)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("rate-limit policy", id))?;

    record_policy_write(&state, &session, "reliability.rate_limits.updated", &saved).await;
    crate::reliability_middleware::reload_from_store(&state).await;
    Ok(Json(PolicyBody::new(&saved)))
}

/// `DELETE /reliability/rate-limits/{id}` — remove a custom policy, disable a shipped one.
///
/// The distinction is in the store rather than in the handler, and it is the request's own rule:
/// "a delete on a default is a disable, not a removal". The response says which one happened, so
/// the panel does not render "deleted" for a row that is still there and merely switched off.
pub async fn delete_policy(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let removed = omnion_reliability::store::delete_policy(state.db().pool(), id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::not_found("rate-limit policy", id))?;

    let was_default = removed.is_default;
    record_policy_write(
        &state,
        &session,
        "reliability.rate_limits.removed",
        &removed,
    )
    .await;
    crate::reliability_middleware::reload_from_store(&state).await;

    Ok(Json(json!({
        "id": id,
        "was_default": was_default,
        "disabled": was_default,
        "deleted": !was_default,
        "name": removed.name,
        "scope": removed.scope,
        "note": if was_default {
            "a shipped policy is disabled rather than removed, so a later boot still has a row for this scope"
        } else {
            "the custom policy was removed"
        },
    })))
}

/// `POST /reliability/rate-limits/evaluate` — dry-run one request through the real decision.
///
/// **This does not spend the caller's budget.** The counter is read with `peek`, and `peek` never
/// increments. A diagnostic tool that consumes what it measures is a tool an operator cannot use
/// twice.
pub async fn evaluate(
    State(state): State<AppState>,
    _session: CurrentSession,
    Json(input): Json<EvaluateRequest>,
) -> Result<Json<EvaluateBody>, ApiError> {
    if let Some(scope) = input.scope.as_deref() {
        if !omnion_reliability::vocabulary::RATE_SCOPES.contains(&scope) {
            return Err(ApiError::bad_request(
                "invalid_reliability_input",
                format!(
                    "scope must be one of {}, got '{scope}'",
                    omnion_reliability::vocabulary::RATE_SCOPES.join(", ")
                ),
            ));
        }
    }
    if let Some(raw) = input.ip.as_deref()
        && raw.parse::<std::net::IpAddr>().is_err()
    {
        return Err(ApiError::bad_request(
            "invalid_reliability_input",
            format!("\"{raw}\" is not an IP address"),
        ));
    }

    let ip = input.ip.as_deref().and_then(|raw| raw.parse().ok());
    let subject = Subject {
        user_id: input.user_id,
        organization_id: None,
        ip,
        route: input.route.clone(),
    };

    // The FULL list, enabled and disabled, so the dry-run can name a disabled policy as the one
    // that would win — "nothing applies because the policy you wrote is switched off" is a
    // different answer from "nothing applies", and only the first is actionable.
    let all = omnion_reliability::store::load_all_policies(state.db().pool())
        .await
        .map_err(map_store)?;

    let now = OffsetDateTime::now_utc();
    let (verdict, counted, policy) = if let Some(scope) = input.scope.as_deref() {
        // A named scope is the operator asking "what would THIS budget say", which is not the same
        // question as "which policy wins" — and it is the question a form with a scope dropdown
        // actually asks.
        let matching = omnion_reliability::limits::pick(
            &all
                .iter()
                .filter(|p| p.scope == scope)
                .cloned()
                .collect::<Vec<_>>(),
            &subject,
        )
        .cloned();
        evaluate_specific(&state, matching, &subject, now).await
    } else {
        let matching = omnion_reliability::limits::pick(&all, &subject).cloned();
        evaluate_specific(&state, matching, &subject, now).await
    };

    let (window_start, counter_key) = match policy.as_ref() {
        Some(policy) => {
            let key = subject
                .key_for(&policy.scope)
                .map(|key| omnion_reliability::limiter_redis::counter_key(policy, &key, now));
            (
                Some(format_offset(omnion_reliability::limiter_redis::window_start(
                    policy, now,
                ))),
                key,
            )
        }
        None => (None, None),
    };

    // The `count` the operator supplied is what the verdict is computed against; without it the
    // dry-run answers "what does the policy ALLOW", which is the question an operator has while
    // tuning, and with it the question they have during an incident.
    let verdict = if let (Some(policy), Some(count)) = (policy.as_ref(), input.count) {
        omnion_reliability::limits::decide(
            policy,
            count,
            now,
            omnion_reliability::limiter_redis::window_start(policy, now),
        )
    } else {
        verdict
    };

    Ok(Json(EvaluateBody {
        policy: policy.as_ref().map(PolicyBody::new),
        verdict,
        counted: CountedBody {
            count: counted.count,
            authoritative: counted.authoritative,
        },
        window_start,
        counter_key,
        fail_mode: crate::reliability_middleware::installed()
            .map_or(FailMode::default().as_str(), |l| l.fail_mode().as_str()),
    }))
}

/// Read the winning policy's budget WITHOUT spending it, and decide against what was read.
async fn evaluate_specific(
    state: &AppState,
    policy: Option<LimitPolicy>,
    subject: &Subject,
    now: OffsetDateTime,
) -> (Verdict, omnion_reliability::limiter_redis::Counted, Option<LimitPolicy>) {
    let Some(policy) = policy else {
        return (
            Verdict::Unlimited,
            omnion_reliability::limiter_redis::Counted {
                count: 0,
                authoritative: true,
            },
            None,
        );
    };
    let Some(key) = subject.key_for(&policy.scope) else {
        return (
            Verdict::Unlimited,
            omnion_reliability::limiter_redis::Counted {
                count: 0,
                authoritative: true,
            },
            Some(policy),
        );
    };
    // `peek`, not `enforce`: the dry-run must not consume the budget it is measuring.
    let counted = omnion_reliability::limiter_redis::peek(&state.redis(), &policy, &key, now).await;
    if !counted.authoritative {
        return (
            Verdict::Uncounted {
                scope: policy.scope.clone(),
            },
            counted,
            Some(policy),
        );
    }
    let start = omnion_reliability::limiter_redis::window_start(&policy, now);
    (
        omnion_reliability::limits::decide(&policy, counted.count, now, start),
        counted,
        Some(policy),
    )
}

/// `GET /reliability/rate-limits/refusals` — the rollups the panel and the events read.
pub async fn list_refusals(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<RefusalsQuery>,
) -> Result<Json<RefusalsBody>, ApiError> {
    let rows = omnion_reliability::store::load_refusals(
        state.db().pool(),
        query.limit.unwrap_or(omnion_reliability::vocabulary::MAX_PAGE),
    )
    .await
    .map_err(map_store)?;
    let last_24_hours = omnion_reliability::store::refusals_in_window(state.db().pool(), 24)
        .await
        .map_err(map_store)?;

    Ok(Json(RefusalsBody {
        refusals: rows
            .into_iter()
            .map(|row| RefusalBody {
                scope: row.scope,
                target_id: row.target_id,
                route: row.route,
                window_start: format_offset(row.window_start),
                refusals: row.refusals,
                last_refusal_at: format_offset(row.last_refusal_at),
            })
            .collect(),
        last_24_hours,
    }))
}

// ---------------------------------------------------------------------------------------------
// The write's paper trail
// ---------------------------------------------------------------------------------------------

/// Write the audit row and the event for one policy mutation.
///
/// Both are best-effort and both are logged on failure: the write has already committed, and a
/// `500` here would tell the operator their policy was lost when it is being enforced by the very
/// next request. The pair is what makes a policy change reviewable after the fact — the panel
/// shows current state, the audit shows how it got there.
async fn record_policy_write(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    policy: &LimitPolicy,
) {
    let metadata = json!({
        "limiter": LIMITER_NAME,
        "policy_id": policy.id,
        "name": policy.name,
        "scope": policy.scope,
        "target_id": policy.target_id,
        "route_pattern": policy.route_pattern,
        "limit_count": policy.limit_count,
        "window_seconds": policy.window_seconds,
        "burst": policy.burst,
        "priority": policy.priority,
        "enabled": policy.enabled,
    });

    if let Err(error) = record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, action)
            .organization(session.user.organization_id)
            .metadata(metadata.clone()),
    )
    .await
    {
        tracing::warn!(error = %error, "the policy was saved but the audit entry was not written");
    }

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new(POLICY_UPDATED)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(metadata),
    )
    .await
    {
        tracing::warn!(error = %error, "the policy was saved but the event was not emitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PolicyInput {
        PolicyInput {
            name: "sign-in".into(),
            scope: "ip".into(),
            target_id: None,
            route_pattern: None,
            limit_count: 10,
            window_seconds: 60,
            burst: 2,
            priority: default_priority(),
            enabled: default_true(),
        }
    }

    #[test]
    fn the_response_carries_the_ceiling_so_the_table_never_adds() {
        // limit + burst shown as its own number: an operator reading "10 + 2" is doing the
        // arithmetic themselves, and a table that does it for them cannot get one row wrong.
        let mut policy: LimitPolicy = (&input()).into();
        policy.id = Some(Uuid::from_u128(1));
        let body = PolicyBody::new(&policy);
        assert_eq!(body.ceiling, 12);
        assert_eq!(body.limit_count, 10);
        assert_eq!(body.burst, 2);
    }

    #[test]
    fn a_route_policy_is_reported_as_not_enforced_here() {
        // The platform limiter runs before the router publishes `MatchedPath`, so it has no
        // route template. A row that is stored but not spent is the "documented but unreachable"
        // shape, and the flag is what keeps the panel from implying otherwise.
        let mut policy: LimitPolicy = (&input()).into();
        policy.scope = "route".into();
        policy.route_pattern = Some("/api/v1/posts/{id}".into());
        let body = PolicyBody::new(&policy);
        assert!(!body.enforced_here, "a route policy cannot be spent by this layer");

        let mut other = policy.clone();
        other.scope = "user".into();
        other.route_pattern = Some("/api/v1/posts/{id}".into());
        assert!(
            PolicyBody::new(&other).enforced_here,
            "a user policy with a route pattern is still enforced — on every subject that reaches it"
        );
    }

    #[test]
    fn a_write_never_carries_its_own_id() {
        // The route decides create-or-update; a body that could name its own row is a body that
        // could rewrite another policy's scope by answering a different URL.
        let mut body = input();
        body.name = "x".into();
        let policy: LimitPolicy = (&body).into();
        assert_eq!(policy.id, None, "the route owns the identity, not the body");
        assert!(!policy.is_default, "a body cannot claim to be a shipped default");
    }

    #[test]
    fn the_form_defaults_match_the_migration_defaults() {
        // `serde` cannot call a const, so these two functions stand in for the column defaults.
        // They have to agree or a form that omits a field produces a different row than the seed.
        assert_eq!(default_priority(), 100);
        assert!(default_true());
    }

    #[test]
    fn an_omitted_burst_is_zero_and_not_a_refusal() {
        let body: PolicyInput = serde_json::from_value(json!({
            "name": "x",
            "scope": "user",
            "limit_count": 5,
            "window_seconds": 60,
        }))
        .expect("the minimal body parses");
        assert_eq!(body.burst, 0, "burst is headroom, and headroom nobody asked for is zero");
        assert_eq!(body.priority, 100);
        assert!(body.enabled);
    }

    #[test]
    fn the_limiter_name_is_the_one_the_middleware_publishes() {
        // A `429` body says `limiter: platform_budget`; a policy row must name the same string or
        // an operator cannot join the two, and the join is the whole point of the field.
        assert_eq!(LIMITER_NAME, crate::reliability_middleware::LIMITER_NAME);
    }

    #[test]
    fn a_timestamp_formats_as_rfc3339_rather_than_a_number() {
        let stamp = format_offset(
            OffsetDateTime::from_unix_timestamp(1_000_000_020).expect("a valid instant"),
        );
        assert!(stamp.contains('T'), "an ISO stamp, not a unix number: {stamp}");
    }
}
