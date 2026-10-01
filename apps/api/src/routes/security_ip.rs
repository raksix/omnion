//! `/api/v1/security/ip-*` — the allow/deny lists and the tester (REQ-012, slice 4).
//!
//! Three things live here and they answer three different questions, which is why they are three
//! routes rather than one: **what is listed** (`GET /security/ip-rules`), **what may I add**
//! (`POST /security/ip-rules`) and **what would this address do** (`POST /security/ip-rules/test`).
//!
//! Two decisions are worth stating because they are the ones an access list usually gets wrong:
//!
//! * **Adding a rule that blocks the caller is warned about, not refused.** The operator is
//!   usually typing a rule for somebody else's network; a rule that blocks *this* session is
//!   usually a paste mistake, but it is also a completely legitimate move — locking yourself out
//!   of one route while the panel is served from another is a known technique. So the response
//!   carries `blocks_you: true` and the screen puts the warning next to the submit button, and
//!   the rule is stored either way. Refusing it would make the platform unable to express a real
//!   rule, and the operator who genuinely needs it would work around a refusal by finding the one
//!   input that does not trigger it.
//! * **The tester evaluates the stored rules and never invents one.** It is the same
//!   [`omnion_security::evaluate_ip_rules`] the request path calls, so its verdict cannot drift
//!   from the real one — and it is [`crate::security_ip::enforce`] that calls it, not this file.
//!
//! Every write is audited and emits `security.ip_rule.changed`, which the request lists as one of
//! the events a webhook may subscribe to: who changed an access rule, and which network, is the
//! first question anybody asks after an incident.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_security::{
    IpRule, MAX_IP_RULE_NOTE, RuleKind, add_ip_rule, evaluate_ip_rules, find_ip_rule,
    ip_rule_counts, list_ip_rules, parse_cidr, remove_ip_rule,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a store error onto the API surface — same mapping the other security routes use.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    use omnion_security::SecurityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_security_input", message),
        E::NotFound => ApiError::new(StatusCode::NOT_FOUND, "not_found", "rule not found"),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security store: {inner}"),
        ),
    }
}

/// The clock, so the tests can pin "now" without the route knowing about tests.
fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One rule as the screen reads it.
#[derive(Debug, Serialize)]
pub struct RuleBody {
    /// The row's id — what the delete button sends back.
    pub id: Uuid,
    /// `allow` or `deny`.
    pub kind: String,
    /// The canonical network text.
    pub cidr: String,
    /// Why the rule exists.
    pub note: String,
    /// Who added it, when that user still exists.
    pub created_by: Option<Uuid>,
    /// When it was added.
    pub created_at: String,
    /// When it stops applying; `None` never expires.
    pub expires_at: Option<String>,
    /// Whether the rule is past its expiry **right now**. The screen greys these rather than
    /// hiding them — a rule that silently disappears is a rule nobody knows they have.
    pub expired: bool,
}

impl From<IpRule> for RuleBody {
    fn from(rule: IpRule) -> Self {
        let expired = rule.expires_at.is_some_and(|at| at <= now());
        Self {
            id: rule.id,
            kind: rule.kind.as_str().to_owned(),
            cidr: rule.cidr,
            note: rule.note,
            created_by: rule.created_by,
            created_at: rule.created_at.to_string(),
            expires_at: rule.expires_at.map(|at| at.to_string()),
            expired,
        }
    }
}

/// The list, with the two counts the screen's summary line shows.
#[derive(Debug, Serialize)]
pub struct ListBody {
    /// Every rule, newest first.
    pub rules: Vec<RuleBody>,
    /// How many deny rules are listed.
    pub deny_count: i64,
    /// How many allow rules are listed.
    pub allow_count: i64,
}

/// The create form.
#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    /// `allow` or `deny`.
    pub kind: String,
    /// The network, in CIDR form. A bare address is accepted and completed.
    pub cidr: String,
    /// Why the rule exists. Required and never blank.
    pub note: String,
    /// When the rule stops applying. `None` never expires.
    pub expires_at: Option<String>,
}

/// The create response: the rule, plus the one warning the caller must see.
#[derive(Debug, Serialize)]
pub struct CreateBody {
    /// The stored rule, canonicalised.
    pub rule: RuleBody,
    /// Whether this rule covers **the caller's own address**. The screen shows this as a warning
    /// on the form; it never blocks the write, because a self-lockout is a legitimate move and
    /// refusing it would only teach the operator which input avoids the warning.
    pub blocks_you: bool,
    /// The same sentence, ready to render.
    pub warning: Option<String>,
}

/// The tester's request.
#[derive(Debug, Deserialize)]
pub struct TestRequest {
    /// The address to test. Required — a tester that cannot test "this session's own address" is
    /// the one case an operator needs during an incident.
    pub address: String,
}

/// The tester's answer.
#[derive(Debug, Serialize)]
pub struct TestBody {
    /// Whether the address would be refused.
    pub blocked: bool,
    /// `allow` or `deny` when a live rule decided it; `None` when nothing applied.
    pub decision: Option<String>,
    /// The rule that decided, as the screen renders it.
    pub matched_rule: Option<RuleBody>,
    /// A rule that *would* have matched but has expired — the explanation for a deny that
    /// stopped applying.
    pub expired_rule: Option<RuleBody>,
    /// The sentence, in words an operator can act on.
    pub reason: String,
    /// The canonical form of what was typed, so the form can show what it will store.
    pub normalised: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /security/ip-rules` — both lists.
pub async fn get(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<ListBody>, ApiError> {
    let pool = state.db().pool();
    let rules = list_ip_rules(pool).await.map_err(map_store)?;
    let (deny_count, allow_count) = ip_rule_counts(pool).await.map_err(map_store)?;

    Ok(Json(ListBody {
        rules: rules.into_iter().map(RuleBody::from).collect(),
        deny_count,
        allow_count,
    }))
}

/// `POST /security/ip-rules` — add one rule.
pub async fn post(
    State(state): State<AppState>,
    session: CurrentSession,
    ClientAddress(address): ClientAddress,
    Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CreateBody>), ApiError> {
    let kind = RuleKind::parse(body.kind.trim()).map_err(map_store)?;
    let cidr = parse_cidr(&body.cidr).map_err(map_store)?;

    let note = body.note.trim();
    if note.is_empty() {
        // Field-level, and for a reason beyond tidiness: an unexplained access rule is one an
        // operator removes without reading during a panic, or leaves in place without
        // understanding. The SQL check constraint holds the same line.
        return Err(ApiError::bad_request(
            "invalid_security_input",
            "say why this rule exists — an access rule nobody can explain is one nobody dares \
             remove or nobody dares leave",
        ));
    }
    if note.chars().count() > MAX_IP_RULE_NOTE {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            format!(
                "the note is {} characters; keep it under {MAX_IP_RULE_NOTE}",
                note.chars().count()
            ),
        ));
    }

    let expires_at = match body.expires_at.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).map_err(
                |_| {
                    ApiError::bad_request(
                        "invalid_security_input",
                        format!(
                            "\"{raw}\" is not a timestamp — use an RFC 3339 value like \
                             2026-01-01T00:00:00Z, or leave it empty for a rule that never expires"
                        ),
                    )
                },
            )?,
        ),
    };

    let rule = add_ip_rule(
        state.db().pool(),
        kind,
        &cidr,
        note,
        expires_at,
        session.user.id,
    )
    .await
    .map_err(map_store)?;

    // The self-lockout warning. Evaluated against the rules as they will be **after** this one,
    // because the question is "will I be able to keep using the panel", and answering it with the
    // pre-write rule set would say "you're fine" for the exact rule that makes it false.
    let verdict = evaluate_ip_rules(
        &list_ip_rules(state.db().pool()).await.map_err(map_store)?,
        address,
        now(),
    );
    let blocks_you = verdict.blocked;
    let warning = blocks_you.then(|| {
        format!(
            "{cidr} covers this session's own address ({}), so the next request from here will be \
             refused. The rule is saved — remove it if that was not the intent.",
            address
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        )
    });

    record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.ip_rule.added")
            .organization(session.user.organization_id)
            .target("security_ip_rule", rule.id.to_string())
            .metadata(json!({
                "kind": rule.kind.as_str(),
                "cidr": rule.cidr,
                "note": rule.note,
                "expires_at": rule.expires_at,
                "blocks_you": blocks_you,
            })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the rule was added but could not be audited: {error}"),
        )
    })?;

    // Swap the rule set the request path judges by, so this rule refuses the *next* request
    // rather than the one after the next restart. Best-effort: the row is already stored, and a
    // reload failure is logged rather than turned into a failed write.
    crate::security_ip::reload_from_store(&state).await;

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("security.ip_rule.changed")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "action": "added",
                "rule_id": rule.id,
                "kind": rule.kind.as_str(),
                "cidr": rule.cidr,
            })),
    )
    .await
    {
        // Same rule as every other emitter in this centre: the rule is stored, so a failed
        // emission is a log line and never a 500 that would tell the operator it failed.
        tracing::warn!(error = %error, "the IP rule was added but the event was not emitted");
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateBody {
            rule: RuleBody::from(rule),
            blocks_you,
            warning,
        }),
    ))
}

/// `DELETE /security/ip-rules/{id}` — remove one rule.
pub async fn delete(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let existing = find_ip_rule(pool, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such IP rule"))?;

    // A row that is already gone is a 404 rather than a success: the screen's delete button
    // disables itself on the response, and "deleted" for a rule that was never there teaches the
    // operator the button works when it did nothing.
    if !remove_ip_rule(pool, id).await.map_err(map_store)? {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such IP rule",
        ));
    }

    record_audit(
        pool,
        NewAuditEntry::by_user(session.user.id, "security.ip_rule.removed")
            .organization(session.user.organization_id)
            .target("security_ip_rule", id.to_string())
            .metadata(json!({
                "kind": existing.kind.as_str(),
                "cidr": existing.cidr,
            })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the rule was removed but could not be audited: {error}"),
        )
    })?;

    // Same reason as the add: a removed deny has to stop applying now, not at the next boot.
    crate::security_ip::reload_from_store(&state).await;

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("security.ip_rule.changed")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "action": "removed",
                "rule_id": id,
                "kind": existing.kind.as_str(),
                "cidr": existing.cidr,
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "the IP rule was removed but the event was not emitted");
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /security/ip-rules/test` — what would this address do?
///
/// The verdict comes from [`evaluate_ip_rules`], the same function the request path calls, so the
/// answer is the platform's answer rather than a second implementation's. The *normalised* field
/// is here because an operator testing `203.0.113.0/8` should learn that it is not the network
/// they typed — and the parser refuses the host-bit forms outright, so what reaches this route is
/// already canonical.
pub async fn test(
    State(state): State<AppState>,
    _session: CurrentSession,
    Json(body): Json<TestRequest>,
) -> Result<Json<TestBody>, ApiError> {
    let address = body.address.trim().parse::<IpAddr>().map_err(|_| {
        ApiError::bad_request(
            "invalid_security_input",
            format!(
                "\"{}\" is not an IP address — the tester takes a single address, not a network \
                 (to list a network, use the form above)",
                body.address.trim()
            ),
        )
    })?;

    let rules = list_ip_rules(state.db().pool()).await.map_err(map_store)?;
    let verdict = evaluate_ip_rules(&rules, Some(address), now());

    Ok(Json(TestBody {
        blocked: verdict.blocked,
        decision: verdict.decision.map(|kind| kind.as_str().to_owned()),
        matched_rule: verdict.matched_rule.map(|rule| RuleBody::from(*rule)),
        expired_rule: verdict.expired.map(|rule| RuleBody::from(*rule)),
        reason: verdict.reason,
        normalised: address.to_string(),
    }))
}
