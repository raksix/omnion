//! `/api/v1/workflows/{id}/graph` and the node-type registry (REQ-004 slice 1).
//!
//! Three things live here, and each one answers a question the builder asks:
//!
//! * **what node types exist** — `GET /api/v1/workflows/node-types`, the registry the
//!   palette draws. It is code, not a table, so a plugin upgrade cannot leave rows behind
//!   that the core no longer understands (REQ-004: "deliberately not a table").
//! * **what the graph is** — `GET /api/v1/workflows/{id}/graph`, the definition plus the
//!   layout, the version a save must quote, and the step list the projection derived.
//! * **what is wrong with it** — `POST …/validate`, findings rather than a verdict, so the
//!   problems panel can show all of them with a jump link each.
//!
//! and one write:
//!
//! * `PUT …/graph` — replace the graph and the step list together, against the version the
//!   client believes it holds. A stale version is `409 graph_version_conflict` and the
//!   current definition comes back with it, because a client that only learns "conflict"
//!   cannot offer Reload.
//!
//! Every write is audited, and the graph write audits the *version* it produced: the audit
//! trail of a definition is the only record of what a rule looked like on the days a run is
//! asked to explain itself.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::bus;
use omnion_events::model::NewEvent;
use omnion_workflows::graph::{self, Edge, Graph, Node};
use omnion_workflows::graph_store::{self, GraphUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::workflows::workflow_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A graph as the panel reads it.
#[derive(Debug, Serialize)]
pub struct GraphBody {
    /// The rule's id.
    pub id: Uuid,
    /// The nodes and the connections between them.
    pub graph: Graph,
    /// Positions, viewport and collapsed groups. Never sent to the engine.
    pub ui_state: Value,
    /// The version a `PUT` must quote.
    pub graph_version: i32,
    /// When the definition was last validated.
    pub validated_at: Option<time::OffsetDateTime>,
    /// The first validation error, when there was one.
    pub validation_error: Option<String>,
    /// The step list the projection derived — the same array the runner executes.
    pub steps: Value,
    /// How many nodes and connections the canvas draws.
    pub node_count: usize,
    /// How many connections the canvas draws.
    pub edge_count: usize,
    /// What the author would see if they pressed Run now.
    pub projection: ProjectionBody,
}

/// Whether the graph is currently runnable, and what would have to change.
#[derive(Debug, Serialize)]
pub struct ProjectionBody {
    /// `true` when `validate` found no error.
    pub valid: bool,
    /// How many steps the runner would execute.
    pub step_count: usize,
    /// The first reason it would not, when it is not valid.
    pub reason: Option<String>,
}

impl GraphBody {
    fn build(definition: &graph_store::GraphDefinition) -> Self {
        let findings: Vec<_> = graph::validate(&definition.graph)
            .into_iter()
            .filter(graph::Finding::is_error)
            .collect();
        let step_count = definition.steps.as_array().map_or(0, Vec::len);
        Self {
            id: definition.workflow_id,
            graph: definition.graph.clone(),
            ui_state: definition.ui_state.clone(),
            graph_version: definition.graph_version,
            validated_at: definition.validated_at,
            validation_error: definition.validation_error.clone(),
            steps: definition.steps.clone(),
            node_count: definition.graph.nodes.len(),
            edge_count: definition.graph.edges.len(),
            projection: ProjectionBody {
                valid: findings.is_empty(),
                step_count,
                reason: findings.first().map(|finding| finding.message.clone()),
            },
        }
    }
}

/// A graph as the panel writes it.
#[derive(Debug, Deserialize)]
pub struct GraphInput {
    /// The nodes and the connections between them.
    pub graph: Graph,
    /// Layout state; omitted (or `null`) leaves the stored layout alone, which is what a
    /// save that changed nothing about the definition sends.
    #[serde(default)]
    pub ui_state: Option<Value>,
    /// The version the client is editing. A mismatch is a conflict, not a write.
    pub graph_version: i32,
}

/// A layout-only write: what an autosave sends while the author pans the canvas.
#[derive(Debug, Deserialize)]
pub struct UiStateInput {
    /// The layout as the client holds it.
    pub ui_state: Value,
}

/// What a validation answers.
#[derive(Debug, Serialize)]
pub struct ValidationBody {
    /// `true` when nothing is an error. A warning does not make a graph invalid.
    pub valid: bool,
    /// Every finding, errors first, in the order validation produced them.
    pub findings: Vec<graph::Finding>,
    /// How many are errors.
    pub error_count: usize,
    /// How many are warnings.
    pub warning_count: usize,
}

impl ValidationBody {
    fn build(findings: Vec<graph::Finding>) -> Self {
        let error_count = findings.iter().filter(|finding| finding.is_error()).count();
        Self {
            valid: error_count == 0,
            error_count,
            warning_count: findings.len() - error_count,
            findings,
        }
    }
}

/// One node type as the palette reads it.
#[derive(Debug, Serialize)]
pub struct NodeTypeBody {
    /// Registry key, stored on the node as `type`.
    pub key: &'static str,
    /// What the palette calls it.
    pub label: &'static str,
    /// Which rail group it sits in.
    pub category: &'static str,
    /// One line under the card.
    pub summary: &'static str,
    /// Output ports, in draw order.
    pub outputs: &'static [graph::Port],
    /// The parameter fields the inspector draws.
    pub params: &'static [graph::ParamField],
    /// `true` when the engine never runs it.
    pub inert: bool,
    /// A starter node for a freshly dropped card, so the inspector has something to show.
    pub defaults: Value,
}

/// The palette's whole registry.
#[derive(Debug, Serialize)]
pub struct NodeTypesBody {
    /// Every node type, in rail order.
    pub node_types: Vec<NodeTypeBody>,
    /// The palette rail's groups, in the order they are drawn.
    pub categories: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/workflows/node-types` — what the palette may offer.
///
/// Deliberately not a table (REQ-004 data model): a plugin upgrade that renames a node type
/// would leave a row behind describing a node the core can no longer explain, and a rule
/// using it would fail at run time instead of at validation.
pub async fn list_node_types(
    State(state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<NodeTypesBody>, ApiError> {
    // The registry is a compile-time constant, so the answer is the same for every
    // organization. The route layer still guards it: a caller who cannot read the rules has
    // no business being told what shapes a rule may take.
    let _ = &state;
    let mut categories: Vec<String> = Vec::new();
    let node_types: Vec<NodeTypeBody> = graph::NODE_TYPES
        .iter()
        .map(|node_type| {
            if !categories.iter().any(|name| name == node_type.category) {
                categories.push(node_type.category.to_owned());
            }
            NodeTypeBody {
                key: node_type.key,
                label: node_type.label,
                category: node_type.category,
                summary: node_type.summary,
                outputs: node_type.outputs,
                params: node_type.params,
                inert: node_type.inert,
                defaults: default_params(node_type),
            }
        })
        .collect();

    Ok(Json(NodeTypesBody {
        node_types,
        categories,
    }))
}

/// `GET /api/v1/workflows/{id}/graph` — the definition the builder opens on.
pub async fn get_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
) -> Result<Json<GraphBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;
    let definition = graph_store::find_graph(state.db().pool(), workflow.id)
        .await?
        .ok_or_else(|| workflow_missing())?;

    Ok(Json(GraphBody::build(&definition)))
}

/// `PUT /api/v1/workflows/{id}/graph` — replace the graph and the step list it projects onto.
///
/// The two are written by one statement, and a stale `graph_version` writes nothing at all.
/// A rule therefore cannot be left with a canvas that says one thing and a run that does
/// another, and a second editor's save cannot be lost to a first editor's autosave.
pub async fn replace_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<GraphInput>,
) -> Result<Json<GraphBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    if input.graph_version < 1 {
        return Err(ApiError::bad_request(
            "graph_version_required",
            "a graph save must quote the version it is editing, starting at 1",
        ));
    }

    let findings = graph::validate(&input.graph);
    if let Some(blocking) = findings.iter().find(|finding| finding.is_error()) {
        // The whole list travels with the refusal: a client that renders only the first
        // finding makes the author press Save once per problem.
        return Err(
            ApiError::bad_request("graph_invalid", blocking.message.clone()).with_details(json!({
                "findings": findings,
                "error_count": findings.iter().filter(|f| f.is_error()).count(),
            })),
        );
    }

    let saved = graph_store::replace_graph(
        state.db().pool(),
        workflow.id,
        GraphUpdate {
            graph: input.graph,
            ui_state: input.ui_state,
            graph_version: input.graph_version,
        },
    )
    .await?;

    let Some(saved) = saved else {
        return Err(workflow_missing());
    };

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "workflow.graph.updated")
            .organization(workflow.organization_id)
            .target("workflow", workflow.id.to_string())
            .metadata(json!({
                "graph_version": saved.graph_version,
                "nodes": saved.graph.nodes.len(),
                "edges": saved.graph.edges.len(),
                "steps": saved.steps.as_array().map_or(0, Vec::len),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(GraphBody::build(&saved)))
}

/// `PUT /api/v1/workflows/{id}/graph/ui-state` — the layout, and nothing else.
///
/// Autosave calls this while an author pans, zooms or drags a card, and it bumps neither
/// `graph_version` nor the step list. A definition is not changed by where its nodes are, and
/// a save that said otherwise would make an afternoon of arranging invalidate a colleague's
/// edit every time it landed.
pub async fn replace_ui_state(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
    Json(input): Json<UiStateInput>,
) -> Result<StatusCode, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    if input.ui_state.is_object() {
        // A layout that is not an object would be refused by the column's own constraint,
        // after the author had already watched their positions vanish.
        graph_store::replace_ui_state(state.db().pool(), workflow.id, input.ui_state).await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/workflows/{id}/validate` — what is wrong with this graph.
///
/// A body with a `graph` validates that graph without storing it; an empty body validates
/// the stored one. The second is what the toolbar's Validate button sends, and the first is
/// what an editor wants while the author is still typing.
pub async fn validate_graph(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(workflow_id): Path<Uuid>,
    body: Option<Json<GraphInput>>,
) -> Result<Json<ValidationBody>, ApiError> {
    let workflow = workflow_in_scope(&state, &current, workflow_id).await?;

    let graph = match body {
        Some(Json(input)) => input.graph,
        None => graph_store::find_graph(state.db().pool(), workflow.id)
            .await?
            .map(|definition| definition.graph)
            .ok_or_else(|| workflow_missing())?,
    };

    let findings = graph::validate(&graph);
    // The verdict is stored so the rule list can draw "invalid" without re-validating every
    // row it renders. Only an error is remembered: the chip is about "this cannot run".
    let first_error = findings
        .iter()
        .find(|finding| finding.is_error())
        .map(|finding| finding.message.clone());
    graph_store::record_validation(state.db().pool(), workflow.id, first_error.as_deref()).await?;

    if first_error.is_some() {
        bus::emit(
            state.db().pool(),
            NewEvent::new("workflow.validation_failed")
                .organization(workflow.organization_id)
                .site(workflow.site_id)
                .actor(current.user.id)
                .payload(json!({
                    "workflow_id": workflow.id,
                    "findings": findings.len(),
                    "first": first_error,
                })),
        )
        .await?;
    }

    Ok(Json(ValidationBody::build(findings)))
}

/// The parameters a freshly dropped card starts with, derived from the schema.
///
/// A card whose required fields are empty is refused the moment it is dropped, which reads
/// as "the builder is broken" rather than as "fill this in" — so the defaults carry the
/// first legal value of every required select, and an empty string for a required text.
fn default_params(node_type: &graph::NodeType) -> Value {
    let mut params = serde_json::Map::new();
    for field in node_type.params {
        if !field.required {
            continue;
        }
        let value = match field.kind {
            "select" => field
                .options
                .first()
                .map_or(Value::Null, |option| json!(option)),
            "number" => json!(1),
            _ => Value::String(String::new()),
        };
        params.insert(field.key.to_owned(), value);
    }
    Value::Object(params)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The 404 every graph route answers with when the rule is gone.
///
/// Shared so a client that branches on `workflow_not_found` sees one code from all of them,
/// rather than a 404 from the graph route and a differently-worded one from the run route.
fn workflow_missing() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::NOT_FOUND,
        "workflow_not_found",
        "no such workflow",
    )
}

/// Write an audit row; a definition change is not reported as saved without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// A node the API would have to accept, used by the tests below and by the walkthrough's
/// fixtures: a trigger and an end, joined, is the smallest graph the runner can execute.
///
/// Kept next to the handler rather than in the client because the walkthrough asserts
/// against it — a body the server and the test disagree about is a body the test proves
/// nothing.
#[must_use]
pub fn starter_graph(event: Option<&str>) -> Graph {
    Graph::starter("event", event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_required_select_starts_on_a_legal_value() {
        // The palette drops a card that is immediately valid: a select whose default is not
        // one of its own options would be refused on the author's first save.
        for node_type in graph::NODE_TYPES {
            for field in node_type.params {
                if !field.required || field.kind != "select" {
                    continue;
                }
                let defaults = default_params(node_type);
                let chosen = defaults
                    .get(field.key)
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert!(
                    field.options.contains(&chosen),
                    "{}: a new {} defaults to {chosen:?}, which is not one of its options",
                    node_type.key,
                    field.key
                );
            }
        }
    }

    #[test]
    fn a_required_text_starts_empty_rather_than_absent() {
        // Absent and empty are the same thing to validation, but only empty is visible: an
        // inspector that shows nothing under a required label reads as a broken form.
        for node_type in graph::NODE_TYPES {
            for field in node_type.params {
                if !field.required || field.kind == "select" {
                    continue;
                }
                let defaults = default_params(node_type);
                assert!(
                    defaults.get(field.key).is_some(),
                    "{} needs {} and a new node must show the field",
                    node_type.key,
                    field.key
                );
            }
        }
    }

    #[test]
    fn a_starter_graph_validates_clean() {
        let starter = starter_graph(Some("page.published"));
        let findings: Vec<_> = graph::validate(&starter)
            .into_iter()
            .filter(graph::Finding::is_error)
            .collect();
        assert!(
            findings.is_empty(),
            "a new rule must open valid: {findings:?}"
        );
    }

    #[test]
    fn every_node_type_the_backfill_can_write_is_in_the_palette() {
        // Migration 0050 writes these five node types into existing rules' graphs. If the
        // registry loses one, every backfilled rule opens already invalid and the finding
        // blames the author.
        for kind in ["task", "wait", "branch", "stop", "approval"] {
            let key = graph::node_type_for_step(kind);
            assert!(
                graph::find_node_type(key).is_some(),
                "the backfill writes {kind:?} as {key:?} and the palette must offer it"
            );
        }
    }

    #[test]
    fn a_validation_body_counts_errors_and_warnings_apart() {
        // The problems panel has to be able to say "1 error, 2 warnings" without the client
        // re-counting, and `valid` must not be true just because warnings exist.
        let mut broken = starter_graph(Some("page.published"));
        broken.nodes.push(Node {
            id: "stray".to_owned(),
            node_type: "note".to_owned(),
            label: "Stray".to_owned(),
            params: json!({ "text": "hello" }),
            position: graph::Position::default(),
        });
        let body = ValidationBody::build(graph::validate(&broken));
        // A stray note is decoration, not an error: the panel may show it and the graph is
        // still runnable, so `valid` stays true. This is the half of the rule the assertion
        // below used to get backwards — a warning must never red the whole canvas.
        assert!(body.valid, "a note is not an error: {:?}", body.findings);
        assert_eq!(body.error_count, 0, "{:?}", body.findings);

        let mut invalid = starter_graph(Some("page.published"));
        invalid.nodes.push(Node {
            id: "a1".to_owned(),
            node_type: "action".to_owned(),
            label: "Runs something".to_owned(),
            params: json!({ "action": "echo" }),
            position: graph::Position::default(),
        });
        let body = ValidationBody::build(graph::validate(&invalid));
        assert!(!body.valid);
        assert!(body.error_count > 0, "an orphan action is an error");
    }

    #[test]
    fn an_edge_helper_keeps_the_default_port_explicit() {
        // The client and the tests build edges by hand; a port left out must mean `out`, and
        // both sides must mean it the same way.
        let edge: Edge = serde_json::from_value(json!({
            "id": "e0", "source": "trigger", "target": "a1"
        }))
        .expect("an edge without a port deserialises");
        assert_eq!(edge.source_port, "out");
    }
}
