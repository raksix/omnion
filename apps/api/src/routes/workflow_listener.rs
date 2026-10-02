//! *Listen for a real event* — arming and reading a one-shot listener (REQ-004 slice 3,
//! criterion 5).
//!
//! Two endpoints, and the split between them is the criterion:
//!
//! | Method | Path | What it does |
//! |---|---|---|
//! | POST | `/api/v1/workflows/{id}/listen` | arm a listener for one node |
//! | GET  | `/api/v1/workflows/{id}/listeners` | the rule's listeners and what they captured |
//!
//! The obvious design — one `POST` that arms *and* polls, so the panel calls it every second
//! — is what this file is shaped against, for a reason that only shows up in production:
//! **a poll that arms is a poll that re-arms.** A panel whose "listening" indicator refreshes
//! by POSTing every second replaces its own listener on every tick, and the row the matcher
//! fills is a row the previous tick deleted. The author watches a spinner, sees the payload
//! appear for one frame, and it is gone — a capture that happened, reported as none. The
//! write and the read are therefore separate, and only the write mints a token.
//!
//! **The token is returned once and never again.** It is a handle that names the armed
//! listener so a caller can script against it, not a capability: reading it back is
//! `workflows.read` on the same rule the caller could already read. Only its hash is stored,
//! so a database dump hands over no armed listener at all.
//!
//! **Arming is `workflows.run`, reading is `workflows.read`.** The same split the REQ's API
//! table names. Arming is not a definition write — nothing about the rule changes — but it
//! is the first half of running it for real, so it belongs with the power that can already
//! start a run, and a manager who cannot start runs cannot leave a rule listening either.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::bus;
use omnion_events::model::NewEvent;
use omnion_workflows::test_listener::{self, ListenerStatus, TestListener};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::workflows::workflow_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// What arming asks for.
#[derive(Debug, Deserialize)]
pub struct ListenInput {
    /// The node the listener is armed for. Required, and checked against the stored graph.
    pub node_id: String,
}

/// One listener as the panel reads it.
///
/// `Clone` because the read answers with the newest capture *and* the list, and the panel
/// gets the capture without having to pick the newest row itself — which is a thing it gets
/// wrong when a newer *expired* row is sitting on top.
#[derive(Debug, Clone, Serialize)]
pub struct ListenerBody {
    /// Row id.
    pub id: Uuid,
    /// The node it was armed for.
    pub node_id: String,
    /// The event name it is waiting for, or waited for.
    pub event_name: String,
    /// `armed`, `captured` or `expired` — derived, never stored.
    pub status: &'static str,
    /// When it was armed. RFC 3339, like every other timestamp on this API: the panel
    /// feeds it to `Date.parse`, and the default serde shape for `OffsetDateTime` is a
    /// ten-element tuple array that no browser will read as a date.
    #[serde(with = "time::serde::rfc3339")]
    pub armed_at: OffsetDateTime,
    /// When the window closes.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Seconds until the window closes, for the countdown the panel draws. `0` once spent.
    pub expires_in_seconds: i64,
    /// When the event arrived, for a captured listener.
    #[serde(with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<OffsetDateTime>,
    /// The bus event that filled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,
    /// The captured payload, pretty-printed. `null` until an event fills it.
    pub payload: Option<Value>,
    /// The payload rendered as text, so the inspector can show it without a JSON viewer
    /// component and so a payload with no fields at all still renders as `{}` rather than
    /// as nothing.
    pub payload_text: Option<String>,
}

impl ListenerBody {
    /// Build from a stored row at a point in time.
    ///
    /// The status is derived here rather than read from a column, and the countdown is
    /// clamped at zero: a negative "expires in -94s" is a number only a clock comparison
    /// produces, and the panel draws a bar from it.
    fn build(row: &TestListener, now: OffsetDateTime) -> Self {
        let status = test_listener::status_of(row, now);
        let remaining = (row.expires_at - now).whole_seconds();

        Self {
            id: row.id,
            node_id: row.node_id.clone(),
            event_name: row
                .event_name_captured
                .clone()
                .unwrap_or_else(|| row.event_name.clone()),
            status: status.as_str(),
            armed_at: row.created_at,
            expires_at: row.expires_at,
            expires_in_seconds: remaining.max(0),
            captured_at: row.consumed_at,
            event_id: row.event_id,
            payload: row.payload.clone(),
            payload_text: row
                .payload
                .as_ref()
                .map(|payload| serde_json::to_string_pretty(payload).unwrap_or_default()),
        }
    }
}

/// What arming answers. The token is here and nowhere else.
#[derive(Debug, Serialize)]
pub struct ArmedListenerBody {
    /// The row that was written.
    pub listener: ListenerBody,
    /// The cleartext token, returned exactly once.
    pub token: String,
    /// The window the listener is armed for, in seconds.
    pub expires_in_seconds: i64,
}

/// What the read answers.
#[derive(Debug, Serialize)]
pub struct ListenerListBody {
    /// The rule's listeners, newest first, whatever state they are in.
    pub listeners: Vec<ListenerBody>,
    /// How many are waiting right now — what the toolbar's indicator reads.
    pub armed: usize,
    /// The most recent capture, when there is one. A convenience so the panel does not have
    /// to pick the newest row itself and get it wrong when a newer *expired* row is on top.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured: Option<ListenerBody>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/workflows/{id}/listen` — arm a one-shot listener for a node.
pub async fn listen_workflow(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<ListenInput>,
) -> Result<(StatusCode, Json<ArmedListenerBody>), ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    let now = OffsetDateTime::now_utc();

    // The refusals live in the store, because "is this a node on the graph" is a fact about
    // the graph and not about the HTTP layer — and a check the API could skip would be a
    // check every other caller has to remember to write.
    let (row, token) = test_listener::arm_for_known_node(
        state.db().pool(),
        workflow.organization_id,
        workflow.id,
        &input.node_id,
        current.user.id,
        now,
    )
    .await?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "workflow.listener_armed")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "node_id": row.node_id,
                "event": row.event_name,
                "listener_id": row.id,
                "expires_at": row.expires_at,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    // The event REQ-004 names, so an armed listener explains itself in the audit trail and
    // the bus — an idle listener is otherwise invisible to anyone who is not looking at
    // the panel.
    bus::emit(
        state.db().pool(),
        NewEvent::new("workflow.test_listener.armed")
            .organization(workflow.organization_id)
            .site(workflow.site_id)
            .actor(current.user.id)
            .payload(json!({
                "workflow_id": workflow.id,
                "listener_id": row.id,
                "node_id": row.node_id,
                "event": row.event_name,
                "expires_at": row.expires_at,
            })),
    )
    .await?;

    let remaining = (row.expires_at - now).whole_seconds();
    Ok((
        StatusCode::CREATED,
        Json(ArmedListenerBody {
            listener: ListenerBody::build(&row, now),
            token,
            expires_in_seconds: remaining.max(0),
        }),
    ))
}

/// `GET /api/v1/workflows/{id}/listeners` — the rule's listeners and what they captured.
///
/// A read that **never filters by state**, and the reason is the failure it prevents: an
/// expired listener that quietly disappeared from the panel is indistinguishable from one
/// that was never armed, and only the second is something the author can act on. The row
/// stays and says `expired`.
pub async fn list_workflow_listeners(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
) -> Result<Json<ListenerListBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    let now = OffsetDateTime::now_utc();

    let rows = test_listener::list_listeners(state.db().pool(), workflow.id, 20).await?;
    let bodies: Vec<ListenerBody> = rows.iter().map(|row| ListenerBody::build(row, now)).collect();

    Ok(Json(ListenerListBody {
        armed: bodies
            .iter()
            .filter(|body| body.status == ListenerStatus::Armed.as_str())
            .count(),
        captured: bodies
            .iter()
            .find(|body| body.status == ListenerStatus::Captured.as_str())
            .cloned(),
        listeners: bodies,
    }))
}

/// `GET /api/v1/workflows/{id}/listeners/{token}` — read one listener back by its token.
///
/// The token is a handle, so this is `workflows.read` on the same rule the caller could
/// already read, and the read is **scoped to the caller's organization**. A `None` covers
/// both "no such token" and "not yours", deliberately: a caller who can tell the two apart
/// can enumerate other organizations' armed listeners one token at a time.
pub async fn get_workflow_listener(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((workflow_id, token)): Path<(Uuid, String)>,
) -> Result<Json<ListenerBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    let now = OffsetDateTime::now_utc();

    let hash = test_listener::hash_token(&token);
    let row = test_listener::find_by_token(state.db().pool(), &hash, workflow.organization_id)
        .await?
        .filter(|row| row.workflow_id == workflow_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "listener_not_found",
                "no listener is armed under that token for this rule",
            )
        })?;

    Ok(Json(ListenerBody::build(&row, now)))
}
