//! `/api/v1/secrets/audit` — the access trail, the anomaly flags and the SIEM export
//! (docs/requests/REQ-125, slice 4).
//!
//! Three reads and one write, and each of them answers a question the operator asks *after*
//! something has already happened:
//!
//! * `GET /secrets/audit` — who read or touched which secret, when, from where, and under which
//!   request id. It reads `audit_log` (not a secrets-specific table — see the crate module for
//!   why), and it accepts only the action names this crate itself writes, so a crafted
//!   `?action=` cannot turn a `secrets.read` surface into a way to read another feature's rows.
//! * `GET /secrets/audit/anomalies` — the flags the detectors raised, unacknowledged first.
//! * `PATCH /secrets/audit/anomalies/{id}/acknowledge` — the one write. It records *who* cleared a
//!   flag and *when*; there is no delete, because a flag nobody cleared is the whole point of
//!   having one.
//! * `GET /secrets/audit/export` — the SIEM feed, newline-delimited JSON, metadata only.
//!
//! Two things this file deliberately does **not** do: it does not refuse a reveal, and it does
//! not return a value. The request offers one optional hard rule ("production reveals require a
//! second approver") and says it ships off; the flag exists in the settings table and is read
//! nowhere. A rule that can lock an incident responder out at the worst moment costs more than
//! an unread advisory row, so the honest implementation of "may block" here is a column that
//! says so and no code that reads it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_secrets::audit::{self, AnomalyRow, AuditFilter, AuditRow, tracked_actions};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::resolve_organization;
use crate::state::AppState;

use super::secrets::map_error;

/// The query string of the audit screen.
///
/// `action` is a list because the screen's filter is a multi-select, and every value is checked
/// against [`omnion_secrets::audit::is_tracked`] before it reaches SQL — an unknown name is dropped
/// rather than forwarded, so a typo narrows the result instead of widening it.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AuditQuery {
    /// Only these actions.
    #[serde(default)]
    pub action: Vec<String>,
    /// Only rows about this secret.
    pub secret_id: Option<Uuid>,
    /// Only rows by this actor.
    pub actor_user_id: Option<Uuid>,
    /// Only rows from this address.
    pub address: Option<String>,
    /// Only rows with this request id.
    pub request_id: Option<Uuid>,
    /// Only rows at or after this instant (RFC 3339).
    pub since: Option<String>,
    /// How many rows; the panel asks for 200 and the store caps it.
    pub limit: Option<i64>,
}

/// One audit row, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct AuditView {
    /// The row's id.
    pub id: i64,
    /// The action name.
    pub action: String,
    /// `secret`, `lease`, `deployment_key`, …
    pub target_type: Option<String>,
    /// The target's id.
    pub target_id: Option<String>,
    /// Who acted.
    pub actor_user_id: Option<Uuid>,
    /// Which kind of actor — the chip that separates a person from a pipeline.
    pub actor_type: String,
    /// Where from.
    pub ip_address: Option<String>,
    /// The request id the caller was handed in the error banner.
    pub request_id: Option<Uuid>,
    /// The lease this touched.
    pub lease_id: Option<Uuid>,
    /// The machine identity that spent it.
    pub deployment_key_id: Option<Uuid>,
    /// The pipeline identity, as presented.
    pub pipeline: Option<String>,
    /// Structured detail. Metadata only — the redaction helper ran before the row was written.
    pub metadata: serde_json::Value,
    /// When it was recorded.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<AuditRow> for AuditView {
    fn from(row: AuditRow) -> Self {
        Self {
            id: row.id,
            action: row.action,
            target_type: row.target_type,
            target_id: row.target_id,
            actor_user_id: row.actor_user_id,
            actor_type: row.actor_type,
            ip_address: row.ip_address,
            request_id: row.request_id,
            lease_id: row.lease_id,
            deployment_key_id: row.deployment_key_id,
            pipeline: row.pipeline,
            metadata: row.metadata,
            created_at: row.created_at,
        }
    }
}

/// One anomaly flag, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct AnomalyView {
    /// The flag's id — the address an acknowledge names.
    pub id: i64,
    /// Which pattern.
    pub pattern: String,
    /// `advisory` today.
    pub severity: String,
    /// The secret it was seen on.
    pub secret_id: Option<Uuid>,
    /// The secret's name, so the row is readable without a second call.
    pub secret_name: Option<String>,
    /// The actor it was observed on.
    pub actor_user_id: Option<Uuid>,
    /// Where from.
    pub address: Option<String>,
    /// What the detector saw.
    pub detail: serde_json::Value,
    /// The request id of the operation that triggered it.
    pub request_id: Option<Uuid>,
    /// When the flag was raised.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Whether it has been cleared.
    pub acknowledged: bool,
    /// Who cleared it.
    pub acknowledged_by: Option<Uuid>,
    /// When they did.
    #[serde(with = "time::serde::rfc3339::option")]
    pub acknowledged_at: Option<OffsetDateTime>,
}

impl From<AnomalyRow> for AnomalyView {
    fn from(row: AnomalyRow) -> Self {
        Self {
            acknowledged: row.acknowledged_at.is_some(),
            id: row.id,
            pattern: row.pattern,
            severity: row.severity,
            secret_id: row.secret_id,
            secret_name: row.secret_name,
            actor_user_id: row.actor_user_id,
            address: row.address,
            detail: row.detail,
            request_id: row.request_id,
            created_at: row.created_at,
            acknowledged_by: row.acknowledged_by,
            acknowledged_at: row.acknowledged_at,
        }
    }
}

/// The audit screen in one read.
#[derive(Debug, Serialize)]
pub struct AuditResponse {
    /// The rows, newest first.
    pub entries: Vec<AuditView>,
    /// The unacknowledged flag count for the header strip.
    pub open_anomalies: i64,
    /// The action names the filter offers, read from the rows that exist.
    pub filters: Vec<String>,
    /// The detector thresholds, so the panel can explain *why* a flag fired.
    pub detectors: DetectorSettingsView,
    /// The local hour at the installation, which is what "off hours" is measured against.
    pub local_hour: u8,
}

/// The thresholds in the panel's shape, with a sentence the screen can render.
#[derive(Debug, Serialize)]
pub struct DetectorSettingsView {
    /// First hour of business hours.
    pub business_hours_start: i32,
    /// First hour after business hours.
    pub business_hours_end: i32,
    /// How many reveals in an hour is a burst.
    pub reveal_burst_per_hour: i32,
    /// Whether a new address is flagged.
    pub detect_new_network: bool,
    /// Always `false` in this build, and said so rather than hidden: the request's hard rule
    /// exists and ships off.
    pub hard_rule_enforced: bool,
    /// The plain-language explanation, so the screen does not have to invent one.
    pub explanation: String,
}

/// `GET /api/v1/secrets/audit` — the trail plus the open-flag count and the thresholds.
pub async fn read_audit(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditResponse>, ApiError> {
    // A *read* resolves its own organization, and a platform account with none reads across —
    // the same rule the credential list follows. A write still needs a named organization.
    let organization_id = session.user.organization_id;
    let pool = state.db().pool();

    let since = match query.since.as_deref() {
        Some(text) => Some(
            OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).map_err(
                |_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_since",
                        "`since` must be an RFC 3339 instant, e.g. 2026-09-28T09:00:00Z",
                    )
                },
            )?,
        ),
        None => None,
    };

    let filter = AuditFilter {
        actions: query.action,
        secret_id: query.secret_id,
        actor_user_id: query.actor_user_id,
        address: query.address,
        request_id: query.request_id,
        since,
        // Capped in the store as well; the cap here keeps a crafted `?limit=` from asking the
        // database for a million rows before the store gets to refuse it.
        limit: query.limit.map(|l| l.clamp(1, 500)),
    };
    let rows = audit::list_audit(pool, &filter).await.map_err(map_error)?;

    let filters = audit::distinct_actions(pool).await.map_err(map_error)?;
    let anomalies = audit::list_anomalies(pool, organization_id, true, 500)
        .await
        .map_err(map_error)?;
    let settings = audit::load_settings(pool).await;

    Ok(Json(AuditResponse {
        open_anomalies: anomalies.len() as i64,
        entries: rows.into_iter().map(AuditView::from).collect(),
        // From the rows, not from a hand-written list: a filter chip for an action nothing has
        // written yet is a dead control, and a missing chip for one that HAS been written hides
        // evidence from the person reading the screen.
        filters,
        detectors: DetectorSettingsView {
            business_hours_start: settings.business_hours_start,
            business_hours_end: settings.business_hours_end,
            reveal_burst_per_hour: settings.reveal_burst_per_hour,
            detect_new_network: settings.detect_new_network,
            hard_rule_enforced: settings.hard_rule_enforced,
            explanation: format!(
                "A reveal is flagged when it happens outside {}:00–{}:00, when more than {} \
                 happen on one secret within an hour, when it comes from an address this account \
                 has not used for that secret before, or when the account has never revealed that \
                 secret before. Flags are advisory: they are recorded and can be acknowledged, \
                 and nothing is ever blocked by one.",
                settings.business_hours_start,
                settings.business_hours_end,
                settings.reveal_burst_per_hour,
            ),
        },
        local_hour: omnion_secrets::audit::local_hour(
            OffsetDateTime::now_utc(),
            local_offset_minutes(),
        ),
    }))
}

/// The anomaly list on its own, for the strip's popover.
#[derive(Debug, Serialize)]
pub struct AnomalyListResponse {
    /// The flags, newest first.
    pub anomalies: Vec<AnomalyView>,
}

/// `GET /api/v1/secrets/audit/anomalies` — the flags, unacknowledged first.
pub async fn read_anomalies(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<AnomalyListResponse>, ApiError> {
    let rows = audit::list_anomalies(state.db().pool(), session.user.organization_id, false, 200)
        .await
        .map_err(map_error)?;
    Ok(Json(AnomalyListResponse {
        anomalies: rows.into_iter().map(AnomalyView::from).collect(),
    }))
}

/// The acknowledge body. It takes no reason: the request names an acknowledge *action*, and a
/// free-text field here would be a second thing to validate for no gain.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AcknowledgeInput {}

/// `PATCH /api/v1/secrets/audit/anomalies/{id}/acknowledge` — clear one flag.
///
/// The answer distinguishes "cleared" from "already cleared", because a second click on a row
/// that is already acknowledged is a normal thing for a panel to do and it should say so rather
/// than report a change that did not happen.
#[derive(Debug, Serialize)]
pub struct AcknowledgeResponse {
    /// `acknowledged` or `already_acknowledged`.
    pub state: &'static str,
    /// The flag's id, echoed so the screen can patch the right row.
    pub id: i64,
}

pub async fn acknowledge_anomaly(
    State(state): State<AppState>,
    session: CurrentSession,
    address: ClientAddress,
    Path(id): Path<i64>,
    Json(_input): Json<AcknowledgeInput>,
) -> Result<Json<AcknowledgeResponse>, ApiError> {
    let organization_id = resolve_organization(&session, None).ok();
    let changed = audit::acknowledge_anomaly(
        state.db().pool(),
        id,
        session.user.id,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(map_error)?;

    audit_entry(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.audit.acknowledged")
            .organization(organization_id)
            .target("anomaly", id.to_string())
            .metadata(json!({ "anomaly_id": id, "changed": changed }))
            .ip_address(address.as_text()),
    )
    .await;

    // Acknowledging is not a secret operation, so it carries no value and no lease. It is still
    // an event: "who waved this through" is exactly the question a compliance review asks.
    emit(
        &state,
        NewEvent::new("secrets.audit_anomaly_acknowledged")
            .actor(session.user.id)
            .payload(json!({ "anomaly_id": id, "changed": changed })),
    )
    .await;

    Ok(Json(AcknowledgeResponse {
        state: if changed {
            "acknowledged"
        } else {
            "already_acknowledged"
        },
        id,
    }))
}

/// `GET /api/v1/secrets/audit/export` — the SIEM feed.
///
/// Newline-delimited JSON with a `Content-Disposition: attachment` name, because the request
/// lists "webhook, JSON lines or syslog-shaped" and JSON lines is the one every collector in that
/// list can read without a parser. The response carries an explicit allowlist of fields — see
/// [`omnion_secrets::audit::SiemRecord`] for why an allowlist rather than a redaction pass.
pub async fn export_audit(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<AuditQuery>,
    address: ClientAddress,
) -> Result<Response, ApiError> {
    let filter = AuditFilter {
        actions: query.action,
        secret_id: query.secret_id,
        actor_user_id: query.actor_user_id,
        address: query.address,
        request_id: query.request_id,
        since: None,
        limit: query.limit.map(|l| l.clamp(1, 5_000)),
    };
    let rows = audit::list_audit(state.db().pool(), &filter)
        .await
        .map_err(map_error)?;
    let body = audit::siem_export(&rows);

    // The export is a read, and a read is itself audited: "who pulled the whole secrets trail
    // into an external system" is precisely the question an operator cannot answer afterwards.
    audit_entry(
        &state,
        NewAuditEntry::by_user(session.user.id, "secret.audit.exported")
            .organization(session.user.organization_id)
            .target("audit", "secrets")
            .metadata(json!({ "rows": rows.len(), "namespaces": omnion_secrets::audit::TRACKED_NAMESPACES }))
            .ip_address(address.as_text()),
    )
    .await;

    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-ndjson; charset=utf-8",
        )
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            "attachment; filename=\"omnion-secrets-audit.ndjson\"",
        )
        .body(axum::body::Body::from(body))
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "export_failed",
                format!("the export could not be assembled: {error}"),
            )
        })
}

/// The installation's UTC offset in minutes, east of UTC.
///
/// Off-hours is a statement about the *operator's* clock, not about UTC: a reveal at 02:00 in
/// İğdır is 23:00 UTC the previous day, and a detector that used UTC would flag a quiet evening
/// while calling a 3 a.m. automated rotation "business hours". `OMNION_UTC_OFFSET_MINUTES` is the
/// same name the other modules read, and a default of 0 keeps a fresh install behaving exactly
/// as UTC rather than guessing a region.
fn local_offset_minutes() -> i32 {
    offset_from(std::env::var("OMNION_UTC_OFFSET_MINUTES").ok())
}

/// The offset reader's decision, extracted so it can be tested without touching the process
/// environment: this crate is `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe` in
/// edition 2024, so a test that mutated the environment would have to `allow` the lint the
/// workspace forbids — a worse trade than testing the decision itself.
fn offset_from(raw: Option<String>) -> i32 {
    raw.and_then(|value| value.trim().parse().ok()).unwrap_or(0)
}

/// Record an event, and never let a bus problem fail the caller's request.
async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the event could not be recorded");
    }
}

/// Write an audit row without ever failing the caller's request.
async fn audit_entry(state: &AppState, entry: NewAuditEntry) {
    if let Err(error) = omnion_audit::entries::record(state.db().pool(), entry).await {
        tracing::warn!(error = %error, "the audit row could not be written");
    }
}

/// The headline action names, re-exported so a future route file does not have to reach into the
/// crate's internals to name one.
#[must_use]
pub fn action_names() -> [&'static str; 8] {
    tracked_actions()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_secrets::audit::actions;

    #[test]
    fn a_wired_offset_is_read_and_a_broken_one_falls_back_to_utc() {
        assert_eq!(offset_from(Some("180".to_owned())), 180);
        assert_eq!(offset_from(Some("  -300 ".to_owned())), -300);
        assert_eq!(offset_from(Some("Europe/Istanbul".to_owned())), 0);
        assert_eq!(offset_from(None), 0);
    }

    #[test]
    fn every_headline_action_belongs_to_the_surface() {
        assert_eq!(action_names(), tracked_actions());
        assert!(action_names().contains(&actions::DENIED));
        assert!(action_names().contains(&actions::DEPLOY_KEY_USE));
        for action in action_names() {
            assert!(
                omnion_secrets::audit::is_tracked(action),
                "{action} is named as tracked but matches no namespace, so the screen would drop it"
            );
        }
    }

    #[test]
    fn a_name_outside_the_namespaces_is_not_this_surface() {
        // The narrowing property: a crafted `?action=` must not widen the read to another
        // feature's rows through a secrets permission.
        for foreign in ["iam.role.created", "billing.invoice.paid", "secret", ""] {
            assert!(
                !omnion_secrets::audit::is_tracked(foreign),
                "{foreign} must not be readable through the secrets audit surface"
            );
        }
        // And every name the handlers actually write is inside the namespaces -- the property
        // whose absence made lease rows invisible on the screen.
        for owned in [
            "secret.lease.issued",
            "secret.lease.revoked",
            "secret.lease.redeemed",
            "secret.credential.typed",
            "secret.credential.validated",
            "secret.root_key.rewrap_paused",
            "secret.root_key.rewrap_resumed",
            "secret.slot_changed",
            "secret.access.denied",
            "secret.audit.exported",
            "deployment_key.created",
            "deployment_key.revoked",
            "deployment_key.deleted",
        ] {
            assert!(
                omnion_secrets::audit::is_tracked(owned),
                "{owned} is written by a handler and must be visible on the audit screen"
            );
        }
    }
}
