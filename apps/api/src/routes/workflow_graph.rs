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
use omnion_workflows::plugin_nodes::PluginRegistry;
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
    /// Every finding the last validate produced, so the panel can list all of them instead of
    /// the first. `projection.reason` is the first message on its own.
    #[serde(default)]
    pub findings: Vec<graph::Finding>,
    /// How many of `findings` are errors. Zero with a non-empty list is a warning-only graph,
    /// which is a real state: a rule that runs with an unconnected note.
    #[serde(default)]
    pub error_count: usize,
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
    /// Build the body a reader opens the builder with.
    ///
    /// `all` is the **whole** finding list — errors and warnings — because the problems
    /// panel is where a warning belongs: "Event is not connected to anything" is a note an
    /// author can act on, and filtering it out here is how a rule ends up with a panel that
    /// says "No problems" over a graph the server will refuse to run. The prior code collected
    /// errors only, so the panel could never have rendered the warning branch at all.
    fn build(definition: &graph_store::GraphDefinition) -> Self {
        let all = graph::validate(&definition.graph);
        // A closure, not the method path: `Iterator::filter` hands its predicate `&&Finding`,
        // and naming a `fn(&Finding) -> bool` there is a type error the compiler reports as
        // "trait bounds not satisfied" — three lines of type theory in a filter. `error_count`
        // below is the one definition of the same question.
        let findings: Vec<_> = all.iter().filter(|f| f.is_error()).cloned().collect();
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
            // The same list, carried whole. A client that renders only the first finding makes
            // the author fix one problem per press, which is how a five-defect graph gets
            // abandoned on the fourth.
            //
            // `error_count` counts ERRORS and the panel's own filter counts them again, so
            // the two must not be the same expression: setting it to `findings.len()` after
            // filtering to errors is right by accident, and would be wrong the day anyone
            // passed the unfiltered list in. One definition, used twice.
            error_count: error_count(&all),
            findings: all,
        }
    }
}

/// How many of a finding list are errors. One definition, because the panel, the body and
/// the tests all need to agree and a second copy is where they stop agreeing.
fn error_count(findings: &[graph::Finding]) -> usize {
    findings.iter().filter(|finding| finding.is_error()).count()
}

/// **WHICH GRAPH A VALIDATION IS ABOUT, AND THEREFORE WHETHER IT MAY BE REMEMBERED.**
///
/// `validation_error` is one column with two readers that must not be confused: the rule
/// list's "invalid" chip, and `engine::admit_to_run`, which turns a non-empty column into
/// `workflow_not_runnable` — the guard that stops a rule from running when it cannot. Both are
/// statements about the rule as **stored**.
///
/// The route serves a second caller, and it is the toolbar's own Validate button: it posts
/// the author's **unsaved draft** (`{nodes, edges}`) so the problems panel can answer while
/// they are still typing. That graph has never been saved, so a verdict about it says nothing
/// about the rule on the server — and writing it there anyway has three faces, all from one
/// root. The draft can be a **superset** of what is stored (a card is half-drawn), a
/// **subset** (the broken edge was deleted and not saved), or **unrelated** (a different
/// rule's graph validated through this id). In every one of them the stored row's verdict is
/// overwritten by a graph nobody is running, and the run-time guard then refuses — or, worse,
/// admits — the rule on the strength of an edit that was never saved.
#[derive(Debug, Clone, Copy)]
struct Validated<'a> {
    /// The graph the caller supplied, if any. `None` means "the stored one".
    supplied: Option<&'a GraphInput>,
}

impl<'a> Validated<'a> {
    /// Decide, once, from **the shape of the request** — never from anything the caller
    /// claims about itself.
    ///
    /// The body IS the draft: its presence is the whole distinction between "check this
    /// graph" and "check the rule", and it is the same distinction this handler's own doc
    /// comment already promises the caller ("validates that graph without storing it"). The
    /// decision is written once here and read at the single place that writes, so a second
    /// copy of the rule elsewhere cannot drift from this one.
    #[must_use]
    fn for_request(body: Option<&'a GraphInput>) -> Self {
        Self { supplied: body }
    }

    /// Whether the verdict may be written to the rule row.
    ///
    /// Only a request that named no graph is about the stored rule. A request that carries
    /// one is a question about a draft, and the honest answer to a question about a draft is
    /// the findings — returned in the body, drawn in the panel, and stored nowhere.
    #[must_use]
    fn may_record(self) -> bool {
        self.supplied.is_none()
    }

    /// Record the verdict, if this request is entitled to.
    ///
    /// **The write is a method, and that is the whole fix.** The first version left the
    /// decision as `if may_record()` at the call site and tested `may_record` directly — and
    /// the test was green with the call-site guard removed, because a test of a pure function
    /// cannot see whether its one caller used it. An assertion nobody has watched fail is a
    /// comment. Routing the write through here means the only path to the column already
    /// carries the decision, so "the route writes a draft's verdict" stops being expressible
    /// rather than being merely discouraged, and the same test now covers the route.
    ///
    /// `None` is what a request about a draft records — and *clears* a stale reason, because
    /// a draft's verdict is not evidence about the stored graph, so the column must keep
    /// saying whatever the last real validation said.
    async fn record_verdict(
        self,
        pool: &sqlx::PgPool,
        workflow_id: Uuid,
        first_error: Option<&str>,
    ) -> omnion_workflows::Result<()> {
        if !self.may_record() {
            return Ok(());
        }
        graph_store::record_validation(pool, workflow_id, first_error).await
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
///
/// `key`/`label`/… are `String` rather than `&'static str` for one reason: the same body
/// describes a **core** type (a `const` in the binary) and a **plugin** type (a manifest a
/// third party uploaded yesterday). The two cannot share a body that borrows from either, and
/// the alternative — a second response type the palette has to union with — is a client that
/// has to know which kind it is looking at before it can draw it.
#[derive(Debug, Serialize)]
pub struct NodeTypeBody {
    /// Registry key, stored on the node as `type`.
    pub key: String,
    /// What the palette calls it.
    pub label: String,
    /// Which rail group it sits in.
    pub category: String,
    /// One line under the card.
    pub summary: String,
    /// Output ports, in draw order.
    pub outputs: Vec<PortBody>,
    /// The parameter fields the inspector draws.
    pub params: Vec<ParamFieldBody>,
    /// `true` when the engine never runs it. Always `false` for a plugin node.
    pub inert: bool,
    /// A starter node for a freshly dropped card, so the inspector has something to show.
    pub defaults: Value,
    /// `Some("Plugin: <name>")` for a plugin node, `None` for a core one.
    ///
    /// Optional rather than an empty string so the client can tell "not a plugin" from "a
    /// plugin that failed to name itself" — and the second is refused at registration, so the
    /// client may treat `None` as an ordinary node without losing a case.
    pub badge: Option<String>,
    /// The provider's key, when this is a plugin node. The tooltip the criterion asks for.
    pub provider: Option<String>,
}

/// One port as the canvas draws it.
#[derive(Debug, Serialize)]
pub struct PortBody {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub terminal: bool,
}

/// One inspector field.
#[derive(Debug, Serialize)]
pub struct ParamFieldBody {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub options: Vec<String>,
    pub help: String,
}

/// The palette's whole registry.
#[derive(Debug, Serialize)]
pub struct NodeTypesBody {
    /// Every node type, in rail order. Core first, then plugin types under `Plugins`.
    pub node_types: Vec<NodeTypeBody>,
    /// The palette rail's groups, in the order they are drawn. `Plugins` is present **only**
    /// when a plugin contributed a node type, so an organization with no plugins never sees a
    /// group heading it cannot fill.
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
///
/// The core half is a compile-time constant and therefore the same for every organization.
/// The plugin half is **per organization** — that is the criterion's "appears when enabled and
/// disappears when disabled" — and it comes from `plugins_enabled_for`, which is the seam
/// REQ-121 fills in. Until then it is an empty registry, so the response is byte-identical to
/// what the core-only route returned and no existing client changes behaviour.
pub async fn list_node_types(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<NodeTypesBody>, ApiError> {
    let mut categories: Vec<String> = Vec::new();
    let mut node_types: Vec<NodeTypeBody> = graph::NODE_TYPES
        .iter()
        .map(|node_type| {
            if !categories.iter().any(|name| name == node_type.category) {
                categories.push(node_type.category.to_owned());
            }
            NodeTypeBody {
                key: node_type.key.to_owned(),
                label: node_type.label.to_owned(),
                category: node_type.category.to_owned(),
                summary: node_type.summary.to_owned(),
                outputs: node_type
                    .outputs
                    .iter()
                    .map(|port| PortBody {
                        key: port.key.to_owned(),
                        label: port.label.to_owned(),
                        // The core registry has no per-port help; a plugin manifest may. The
                        // field is omitted rather than sent as null so the canvas's
                        // `port.help && …` reads the same shape for both kinds.
                        help: None,
                        terminal: port.terminal,
                    })
                    .collect(),
                params: node_type
                    .params
                    .iter()
                    .map(|field| ParamFieldBody {
                        key: field.key.to_owned(),
                        label: field.label.to_owned(),
                        kind: field.kind.to_owned(),
                        required: field.required,
                        options: field.options.iter().map(|o| (*o).to_owned()).collect(),
                        help: field.help.to_owned(),
                    })
                    .collect(),
                inert: node_type.inert,
                defaults: default_params(node_type),
                badge: None,
                provider: None,
            }
        })
        .collect();

    // The plugin half, appended after the core so the rail's group order is stable and the
    // `Plugins` heading is always last — a group that moves position as plugins come and go
    // makes the palette feel unstable for a reason nobody can name.
    let plugins = plugins_enabled_for(&state, &current).await?;
    for node in plugins.nodes() {
        if !categories.iter().any(|name| name == &node.category) {
            categories.push(node.category.clone());
        }
        node_types.push(NodeTypeBody {
            key: node.key.clone(),
            label: node.label.clone(),
            category: node.category.clone(),
            summary: node.summary.clone(),
            outputs: node
                .outputs
                .iter()
                .map(|port| PortBody {
                    key: port.key.clone(),
                    label: port.label.clone(),
                    help: port.help.clone(),
                    terminal: port.terminal,
                })
                .collect(),
            params: node
                .params
                .iter()
                .map(|field| ParamFieldBody {
                    key: field.key.clone(),
                    label: field.label.clone(),
                    kind: field.kind.clone(),
                    required: field.required,
                    options: field.options.clone(),
                    help: field.help.clone(),
                })
                .collect(),
            // A plugin node is never decoration: it declares ports, so the author wired it
            // expecting it to do something, and drawing it greyed as inert would be a lie.
            inert: false,
            defaults: node.defaults.clone(),
            badge: Some(node.badge.clone()),
            provider: Some(node.provider.plugin.clone()),
        });
    }

    Ok(Json(NodeTypesBody {
        node_types,
        categories,
    }))
}

/// The plugin node types this organization has enabled.
///
/// **The seam, and it is deliberately empty.** REQ-121 owns plugin enablement, its store and
/// its `plugins.read` gate; this function is where that lands, and it is the *only* place —
/// `crates/workflows` deliberately does not know that plugins exist as rows, so the seam is a
/// function rather than a trait so a later reader can find it in one grep.
///
/// It is not a stub in the sense of returning nothing forever: it reads the real session, so
/// the organization id is already in hand when the store arrives, and the `Result` means the
/// error path is wired rather than added later. Until then the answer is an empty registry,
/// which is exactly the state of an organization with no plugins — so the route is correct
/// today rather than approximately correct.
async fn plugins_enabled_for(
    _state: &AppState,
    _current: &CurrentSession,
) -> Result<PluginRegistry, ApiError> {
    Ok(PluginRegistry::empty())
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

    // **VALIDATE TO TELL THE AUTHOR, NOT TO REFUSE THE WORK.** The graph is validated here for
    // two reasons that both point the same way: the same registry the palette was drawn from
    // (a save checked against a *different* set is how a rule gets refused for a node the
    // palette still offers), and so the author is handed every finding at once rather than
    // pressing Save once per problem. It is deliberately NOT a gate. Refusing a save whose cards
    // are not wired together yet refuses the first keystroke of the feature: a rule is built by
    // being incomplete, and the runner refuses a graph it cannot project — so the guard that
    // belongs to this feature lives at run time, where it is the true statement.
    let plugins = plugins_enabled_for(&state, &current).await?;
    let findings = graph::validate_with_plugins(&input.graph, &plugins);
    let errors = error_count(&findings);

    let saved = graph_store::replace_graph(
        state.db().pool(),
        workflow.id,
        GraphUpdate {
            graph: input.graph,
            ui_state: input.ui_state,
            graph_version: input.graph_version,
        },
        // The same registry the findings above came from. Validating against one set and
        // projecting against another is how a graph gets accepted by one function and
        // refused by the next with a sentence about a different problem.
        Some(&plugins),
    )
    .await?;

    let Some(saved) = saved else {
        return Err(workflow_missing());
    };

    // A save that could not project is a successful save of a rule that cannot run yet, and the
    // author has to be able to tell those apart. `GraphBody::build` recomputes the findings the
    // panel's problems panel draws from and reports them in `projection`; the count is carried
    // here because a client that renders only the first finding makes the author press Save
    // once per problem.
    // `build` re-validates with the CORE registry; this save was validated against the
    // registry the palette was drawn from, and a plugin node is known there and unknown
    // here. The save's own list is the one the author must see, or enabling a plugin makes
    // the panel invent a finding the server never produced.
    let mut body = GraphBody::build(&saved);
    body.error_count = errors;
    body.findings = findings;

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

    Ok(Json(body))
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

    // **DECIDE ONCE, FROM THE REQUEST'S SHAPE, WHETHER THIS VERDICT MAY BE REMEMBERED.**
    // See `Validated`: a body is the builder's unsaved draft, and only a request that named no
    // graph is a statement about the rule on the server.
    let requested = Validated::for_request(body.as_ref().map(|Json(input)| input));
    let graph = match requested.supplied {
        Some(input) => input.graph.clone(),
        None => graph_store::find_graph(state.db().pool(), workflow.id)
            .await?
            .map(|definition| definition.graph)
            .ok_or_else(|| workflow_missing())?,
    };

    // Validated against the node types this organization actually has. Using the core-only
    // `validate` here would report a rule using an enabled plugin as `unknown_node_type` —
    // and that verdict is *stored* on the row, so the rule list would show a permanently
    // "invalid" chip on a rule that is fine, with no way for the author to clear it.
    let plugins = plugins_enabled_for(&state, &current).await?;
    let findings = graph::validate_with_plugins(&graph, &plugins);
    // The verdict is stored so the rule list can draw "invalid" without re-validating every
    // row it renders. Only an error is remembered: the chip is about "this cannot run".
    let first_error = findings
        .iter()
        .find(|finding| finding.is_error())
        .map(|finding| finding.message.clone());

    // **A DRAFT'S VERDICT IS ANSWERED, NOT STORED.** Everything in this block — the column
    // write, the list chip and the `workflow.validation_failed` event — is a statement about the
    // rule **as stored**, because `admit_to_run` reads that column to decide whether a run may
    // start. Writing a draft's findings there moved that guard on an edit that was never
    // saved: an author who removed the broken edge saw the panel go green and their *saved*
    // rule become unrunnable, and an author with a card half-drawn took a healthy rule down
    // with them. The findings still go back in the body and still render in the panel — which
    // is the whole question this endpoint was asked.
    requested
        .record_verdict(state.db().pool(), workflow.id, first_error.as_deref())
        .await?;

    if requested.may_record() && first_error.is_some() {
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

    /// **THE PANEL MUST BE ABLE TO RENDER A WARNING, OR ITS WARNING BRANCH IS DEAD CODE.**
    ///
    /// `GraphBody::build` collected `filter(is_error)` and shipped that as `findings`, so the
    /// list on the wire could only ever contain errors — while the panel's own rendering has
    /// a `severity !== "error"` branch and a "N warnings" heading. The branch was unreachable:
    /// a graph with a real warning and no error reached the author as "No problems". The
    /// `error_count` that came with it was `findings.len()` after the filter, which was right
    /// only by accident and would have counted warnings as errors the moment the filter was
    /// removed — the two were the same expression, so they could not disagree until they did.
    #[test]
    fn the_body_carries_warnings_and_counts_only_errors() {
        // **START FROM A GRAPH PROVEN CLEAN, THEN ADD THE ONE WARNING.** The first draft built
        // its fixture from `starter_graph(None)` and expected a clean result — and an event
        // trigger with no event name is a `missing_parameter` **error**, so the fixture carried
        // a second finding and the assertion that mattered (`errors == 0`) failed. A fixture
        // whose cleanliness is assumed is a fixture whose defects are the test's.
        let mut warned = starter_graph(Some("page.published"));
        assert!(
            error_count(&graph::validate(&warned)) == 0,
            "the starting point must be clean, or every finding below is ambiguous"
        );

        // **THE FIXTURE IS A LABEL, NOT A NOTE CARD — and the difference is the whole test.**
        // The second draft of this test reached for a `note` node, which reads like the obvious
        // "harmless card that is not wired to anything" — and produces *no finding at all*,
        // because a note is inert by design (it contributes no step and no error). The test
        // would then have passed its own fixture guard with `all.is_empty()` and proved
        // nothing: an empty list counts errors correctly.
        //
        // A label longer than the card draws is the one warning the validator raises on an
        // otherwise clean graph, so it is the only fixture that reaches the state under test:
        // warnings present, errors absent.
        let long = "W".repeat(graph::MAX_NODE_LABEL + 1);
        let Some(trigger) = warned.nodes.first_mut() else {
            panic!("a starter graph has a trigger");
        };
        trigger.label = long;

        let all = graph::validate(&warned);
        let warnings = all.iter().filter(|f| !f.is_error()).count();
        let errors = error_count(&all);

        // **The guard, and it is an equality this time.** The original form allowed `all` to be
        // empty, which is exactly the outcome that would make the test pass for the wrong
        // reason. This fixture MUST produce one warning and no error, or it is not the shape.
        assert_eq!(
            warnings, 1,
            "this fixture must produce exactly one warning, or it is not testing the shape: \
             {all:?}"
        );
        assert_eq!(
            errors, 0,
            "a warning must not be counted as an error, or the panel refuses a runnable \
             rule: {all:?}"
        );

        // And the assertion the old code could not have passed: the error count and the list
        // length disagree, which is the only way to notice that both were the same
        // expression. Before the fix `findings` was the error-only list and `error_count` was
        // `findings.len()` — one expression, so the two could never disagree until they did.
        assert_ne!(
            errors,
            all.len(),
            "a list with a warning in it must not report every entry as an error"
        );
    }

    /// **A VALIDATION OF A DRAFT MUST NOT BE WRITTEN ONTO THE STORED RULE.**
    ///
    /// `POST /validate` serves two callers from one handler: the rule list's chip, which is
    /// about the **stored** graph, and the builder toolbar's Validate button, which posts the
    /// author's **unsaved draft**. The handler derived its verdict from whichever graph it was
    /// given and then wrote it to the rule row *unconditionally* — so the draft's verdict
    /// overwrote the stored rule's.
    ///
    /// The consequence is not cosmetic, and `engine::admit_to_run` is why: it turns a
    /// non-empty `validation_error` into `workflow_not_runnable`. So an author who deletes the
    /// broken edge in the canvas and presses Validate sees the panel go green — and has
    /// **marked their saved rule unrunnable**, because the green draft was written over the
    /// stored graph that still has the defect. The rule stops firing on its schedule, and the
    /// stored defect is still there. The mirror case is just as bad: press Validate with a
    /// half-drawn card and a stored rule that is perfectly fine becomes unrunnable.
    ///
    /// Neither of these needs a race or two tabs. One tab, one button, and the guard whose
    /// whole job is "do not run this" is moved by an edit that was never saved.
    ///
    /// **Why the fix is a decision about the request's shape and not a version check.** The
    /// tempting alternative is to compare the draft against `graph_version` and store only on
    /// a match — but the builder sends `{nodes, edges}` with no version at all, and a draft
    /// that happens to equal the stored graph is still a draft. Presence of the body is the
    /// only signal that distinguishes the two callers, and this handler's own doc comment
    /// already promises it.
    #[test]
    fn a_validation_of_an_unsaved_draft_is_answered_but_not_remembered() {
        let draft = GraphInput {
            graph: starter_graph(Some("page.published")),
            ui_state: None,
            graph_version: 0,
        };

        // The draft case: findings are returned, nothing is recorded.
        let answered = Validated::for_request(Some(&draft));
        assert!(
            !answered.may_record(),
            "a request carrying a graph is asking about THAT graph — it is the builder's live \
             draft and has never been saved, so its verdict must not touch the rule row"
        );

        // **The control, and it is the half that says the fix is not "never record".** A
        // request with no body validates the stored graph, and that verdict is exactly what
        // the rule list's chip and `admit_to_run` need. A fix that silenced this branch would
        // leave every rule permanently looking valid and every unrunnable rule admitted —
        // strictly worse than the defect, and green in the half a test that only checks the
        // draft case would pass.
        let stored = Validated::for_request(None);
        assert!(
            stored.may_record(),
            "a request with no body IS about the stored rule; refusing to record it would \
             strip the 'invalid' chip and hand `admit_to_run` a column nothing ever writes"
        );

        // And the two are different requests, not two views of one.
        assert_ne!(
            (answered.supplied.is_some(), stored.may_record()),
            (false, true),
            "one request supplied a graph and the other did not; the decision reads that, not \
             anything else about them"
        );
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
