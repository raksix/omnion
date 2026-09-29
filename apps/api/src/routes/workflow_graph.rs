//! `/api/v1/workflows/{id}/graph` — the editor's read and write surface (REQ-086, slice 1).
//!
//! Three endpoints, and the split between them is the whole design of the slice:
//!
//! * `GET` hands the canvas the document *and* the revision it was read at. The editor holds
//!   that revision for the life of the canvas and sends it back on every save.
//! * `PUT` writes the whole graph, compiles it and writes the compiled steps **in one
//!   statement**. A save that wrote the graph now and the steps later would leave a run
//!   executing yesterday's definition while the canvas shows today's.
//! * `POST …/validate` compiles and reports without writing, because a person typing a node
//!   parameter must not have it stored to find out it is wrong.
//!
//! Two rules the handlers enforce on top of the permission guard (`crate::guards`):
//!
//! * tenancy, through the same `ensure_same_organization` rule every workflow route uses
//!   (`crate::scope`) — a graph is as tenant-scoped as the definition it belongs to;
//! * a conflict is a `409` with the *current* revision, never an overwrite. The editor's
//!   answer to a conflict is compare-and-reload, and it cannot offer that without being told
//!   what it is conflicting with.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_workflows::graph::{self, Graph};
use omnion_workflows::graph_store::{self, SaveOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

/// The 404 a missing workflow answers with, built here rather than imported.
///
/// `routes::workflows` has the same sentence in a private helper, and reaching for it would
/// mean widening that module's visibility for a second caller. The code and the status are the
/// contract a client branches on, and duplicating two lines is cheaper than making an
/// unrelated module's internals public — but the *code* is asserted against the workflow
/// routes' own in the integration test, so the two cannot drift apart silently.
fn graph_workflow_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "workflow_not_found",
        "no such workflow",
    )
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The graph as the canvas reads it.
#[derive(Debug, Serialize)]
pub struct GraphBody {
    /// Workflow the graph belongs to.
    pub workflow_id: Uuid,
    /// The document: nodes, connections and sticky notes.
    pub graph: Graph,
    /// The revision this read saw. Sent back on save.
    pub revision: i32,
    /// How many nodes the document holds.
    pub node_count: usize,
    /// How many connections it holds.
    pub connection_count: usize,
    /// How many sticky notes it holds.
    pub note_count: usize,
    /// When the canvas was last saved, when it ever was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Who saved it last.
    pub updated_by: Option<Uuid>,
}

/// A save request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveGraphBody {
    /// The document to store.
    pub graph: Graph,
    /// The revision the editor loaded. Required, and deliberately *not* defaulted: a save with
    /// no revision would have to mean "force", and a force nobody asked for is how two editors
    /// end up with one silently winning.
    pub revision: i32,
}

/// A successful save.
#[derive(Debug, Serialize)]
pub struct SaveGraphResponse {
    /// The revision the row carries now — what the editor keeps for its next save.
    pub revision: i32,
    /// How many steps the graph compiled to.
    pub step_count: usize,
    /// Which node produced which step, in step order.
    pub node_order: Vec<String>,
}

/// A validation report.
#[derive(Debug, Serialize)]
pub struct ValidateGraphResponse {
    /// `true` when nothing is wrong.
    pub valid: bool,
    /// Every problem, in a stable order.
    pub issues: Vec<omnion_workflows::graph::Issue>,
    /// How many of them there are.
    pub issue_count: usize,
    /// How many steps the graph *would* compile to. Zero when it is not valid, and named
    /// `step_count` rather than `projected_step_count` because there is no second value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_count: Option<usize>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/workflows/{id}/graph` — the document and the revision it was read at.
pub async fn get_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
) -> Result<Json<GraphBody>, ApiError> {
    let stored = graph_store::find_graph(state.db().pool(), workflow_id)
        .await?
        .ok_or_else(graph_workflow_not_found)?;
    crate::scope::ensure_same_organization(&current, Some(stored.organization_id))?;

    // A row whose document cannot be deserialised reads back as an empty graph rather than a
    // 500: the workflow itself still runs from its steps, and hiding it behind a parse error
    // would make a corrupt canvas look like a broken workflow.
    let document = Graph::from_stored(&stored.graph);

    Ok(Json(GraphBody {
        workflow_id: stored.workflow_id,
        node_count: document.nodes.len(),
        connection_count: document.connections.len(),
        note_count: document.notes.len(),
        revision: stored.graph_revision,
        updated_at: stored.graph_updated_at,
        updated_by: stored.graph_updated_by,
        graph: document,
    }))
}

/// `PUT /api/v1/workflows/{id}/graph` — store the document and its compiled steps together.
pub async fn save_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<SaveGraphBody>,
) -> Result<impl IntoResponse, ApiError> {
    let stored = graph_store::find_graph(state.db().pool(), workflow_id)
        .await?
        .ok_or_else(graph_workflow_not_found)?;
    crate::scope::ensure_same_organization(&current, Some(stored.organization_id))?;

    // `If-Match` is the header the REQ names, and it is honoured when present. The body carries
    // the revision too, because a browser `fetch` from a client that stores the editor's state
    // in memory has no reason to synthesise an ETag, and two sources of truth for one number is
    // how a save ends up checking one revision and reporting another. The body wins when both
    // are sent and they disagree, and the disagreement is refused rather than resolved: a
    // caller that does not know its own revision must not be allowed to guess which half was
    // meant.
    if let Some(header_revision) = if_match_revision(&headers) {
        if header_revision != body.revision {
            return Err(ApiError::bad_request(
                "graph_revision_ambiguous",
                format!(
                    "If-Match says revision {header_revision} and the body says {}; \
                     send one or the other",
                    body.revision
                ),
            ));
        }
    }

    if body.revision < 0 {
        return Err(ApiError::bad_request(
            "graph_revision_invalid",
            "a revision is zero or more",
        ));
    }

    let compiled = graph::compile(&body.graph);
    if !compiled.is_clean() {
        // A refused save is a `422`, not a `400`: the request was well-formed and the *content*
        // is what is wrong, and the body carries every issue so the canvas can badge each node
        // rather than showing one sentence for eleven problems.
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "graph_invalid",
            format!(
                "{} issue{} block this save",
                compiled.issues.len(),
                if compiled.issues.len() == 1 { "" } else { "s" }
            ),
        )
        .with_details(json!({ "issues": compiled.issues })));
    }

    let step_count = compiled.steps.len();
    let node_order = compiled.node_order.clone();

    let outcome = graph_store::save_graph(
        state.db().pool(),
        workflow_id,
        &body.graph,
        &compiled.steps,
        body.revision,
        current.user.id,
    )
    .await?;

    match outcome {
        SaveOutcome::Gone => Err(graph_workflow_not_found()),
        SaveOutcome::Conflict { current_revision } => {
            // The current document is *not* included: it can be 200 nodes, and the editor
            // already has one of its own to diff against. The revision is what it needs to ask
            // the question "am I out of date, and by how much".
            Err(ApiError::new(
                StatusCode::CONFLICT,
                "graph_revision_conflict",
                format!(
                    "this canvas was loaded at revision {} and the workflow is now at {current_revision}; \
                     reload to see the newer document",
                    body.revision
                ),
            )
            .with_details(json!({
                "loaded_revision": body.revision,
                "current_revision": current_revision,
            })))
        }
        SaveOutcome::Saved { revision } => {
            omnion_audit::record(
                state.db().pool(),
                NewAuditEntry::by_user(current.user.id, "workflow.graph_saved")
                    .organization(stored.organization_id)
                    .target("workflow", workflow_id.to_string())
                    .metadata(json!({
                        "revision": revision,
                        "previous_revision": body.revision,
                        "node_count": body.graph.nodes.len(),
                        "connection_count": body.graph.connections.len(),
                        "step_count": step_count,
                    }))
                    .ip_address(address.as_text()),
            )
            .await?;

            // The palette listens for this: a saved graph can name a node type that a
            // node package has since added, and the editor's availability badge reads the
            // registry rather than re-deriving it.
            bus::emit(
                state.db().pool(),
                NewEvent::new("workflows.graph.saved")
                    .organization(stored.organization_id)
                    .actor(current.user.id)
                    .payload(json!({
                        "workflow_id": workflow_id,
                        "revision": revision,
                        "node_count": body.graph.nodes.len(),
                        "step_count": step_count,
                    })),
            )
            .await
            .ok();

            Ok((
                StatusCode::OK,
                Json(SaveGraphResponse {
                    revision,
                    step_count,
                    node_order,
                }),
            )
                .into_response())
        }
    }
}

/// `POST /api/v1/workflows/{id}/graph/validate` — compile and report, storing nothing.
pub async fn validate_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
    Json(body): Json<SaveGraphBody>,
) -> Result<Json<ValidateGraphResponse>, ApiError> {
    let stored = graph_store::find_graph(state.db().pool(), workflow_id)
        .await?
        .ok_or_else(graph_workflow_not_found)?;
    crate::scope::ensure_same_organization(&current, Some(stored.organization_id))?;

    let compiled = graph::compile(&body.graph);
    Ok(Json(ValidateGraphResponse {
        valid: compiled.is_clean(),
        issue_count: compiled.issues.len(),
        step_count: compiled.is_clean().then_some(compiled.steps.len()),
        issues: compiled.issues,
    }))
}

/// The revision in an `If-Match` header, when it carries one.
///
/// Accepts the bare number the REQ's table uses and the quoted form HTTP specifies, and refuses
/// a `W/` weak tag or `*` rather than guessing: both mean "I do not know which revision this is",
/// and treating that as "the current one" is how a lost update becomes silent.
fn if_match_revision(headers: &HeaderMap) -> Option<i32> {
    let raw = headers.get(axum::http::header::IF_MATCH)?.to_str().ok()?;
    let trimmed = raw.trim().trim_matches('"');
    trimmed.parse::<i32>().ok()
}

/// A graph with no nodes at all, for a caller that wants the empty document.
#[must_use]
pub fn empty_graph() -> Value {
    Graph::new().to_value()
}
