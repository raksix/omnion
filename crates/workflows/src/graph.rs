//! The graph a visual builder draws, and the projection the runner executes (REQ-004 slice 1).
//!
//! # Two representations, one definition
//!
//! REQ-003 gave the engine an ordered list of steps, and that list is still what runs: the
//! engine materialises `workflows.steps` into rows and advances them. The builder needs
//! something the list cannot express — two nodes fed by one condition — so the graph arrives
//! as a second representation of the *same* definition, with one rule that keeps them from
//! drifting:
//!
//! * the **graph** is authoritative for the builder (`workflows.graph`);
//! * the **steps projection** is generated and never hand-edited;
//! * every save re-derives the projection from the graph, and
//!   [`project`] is the only function that does it, so a rule cannot be projected two ways.
//!
//! The engine needs no change: it reads `steps`, which is what the projection wrote.
//!
//! # Why the projection is a linearisation and not a second evaluator
//!
//! A graph can express branching, and the v0 engine has exactly one branching step: a `branch`
//! step ends the run when its comparison does not hold. So the projection turns each `condition`
//! node into a [`StepDefinition::branch`] carrying the same comparison, and the run stops there
//! when the comparison is false — the "false" branch is an *end*, not a second path. Every
//! node reachable only through a false edge is simply absent from the projection, and the run
//! stops. A node on the true edge follows. This is honest for a v1 and it is testable: the
//! acceptance criterion is that the existing runner executes a saved graph end to end with no
//! engine change, and this is what makes that true.
//!
//! # What validation refuses
//!
//! A graph that cannot be projected must not be stored as if it could: [`validate`] returns
//! findings (not an error) naming the node, so the panel can offer a jump link. A graph that
//! *can* be projected but is a bad idea — an orphan, a second trigger — is still refused at
//! write time, because the alternative is a definition that runs and does nothing.
//!
//! [`StepDefinition::branch`]: crate::definition::StepDefinition::branch

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::branch::{MAX_FIELD, OPERATORS};
use crate::definition::{MAX_STEPS, StepDefinition};
use crate::error::{Result, WorkflowError};

/// Most nodes one graph may carry (the projection cannot exceed [`MAX_STEPS`]).
pub const MAX_NODES: usize = MAX_STEPS + 2;

/// Most edges one graph may carry.
pub const MAX_EDGES: usize = 400;

/// Longest a node id may be.
pub const MAX_NODE_ID: usize = 64;

/// Longest a node label may be.
pub const MAX_NODE_LABEL: usize = 80;

/// Smallest a canvas coordinate may be. A node dragged a million pixels to the left is a
/// coordinate, not a layout, and it makes the minimap and the auto-layout useless.
pub const MIN_COORD: f64 = -20_000.0;

/// Largest a canvas coordinate may be.
pub const MAX_COORD: f64 = 20_000.0;

/// How far one zoom step moves the viewport.
pub const ZOOM_STEP: f64 = 0.1;

/// Smallest zoom the viewport may hold (REQ-004: 0.25×–2×).
pub const MIN_ZOOM: f64 = 0.25;

/// Largest zoom the viewport may hold.
pub const MAX_ZOOM: f64 = 2.0;

/// The severity of one validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The graph cannot be stored or run as it is.
    Error,
    /// The graph runs, but something about it is worth saying out loud.
    Warning,
}

impl Severity {
    /// Canonical name stored in JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One thing validation has to say about a graph.
///
/// Every finding names the node it is about (`node_id` is `None` only for a finding about the
/// graph as a whole) because the panel turns it into a jump link, and a message without a
/// destination is a message the reader has to go and find the subject of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `error` or `warning`.
    pub severity: Severity,
    /// Stable machine-readable code, e.g. `graph_cycle`.
    pub code: String,
    /// Human-readable explanation, naming the node or nodes involved.
    pub message: String,
    /// The node the finding is about, when there is one.
    pub node_id: Option<String>,
    /// The other end of a two-node finding (a cycle names where it closes).
    pub related_node_id: Option<String>,
}

impl Finding {
    /// An error-level finding.
    #[must_use]
    pub fn error(code: &str, message: impl Into<String>, node_id: Option<&str>) -> Self {
        Self {
            severity: Severity::Error,
            code: code.to_owned(),
            message: message.into(),
            node_id: node_id.map(str::to_owned),
            related_node_id: None,
        }
    }

    /// A warning-level finding.
    #[must_use]
    pub fn warning(code: &str, message: impl Into<String>, node_id: Option<&str>) -> Self {
        Self {
            severity: Severity::Warning,
            code: code.to_owned(),
            message: message.into(),
            node_id: node_id.map(str::to_owned),
            related_node_id: None,
        }
    }

    /// Name a second node — the other end of the connection this finding is about.
    #[must_use]
    pub fn related(mut self, other: Option<&str>) -> Self {
        self.related_node_id = other.map(str::to_owned);
        self
    }

    /// `true` when the finding stops the graph from being stored.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error)
    }
}

/// Where a node sits on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Horizontal offset, snapped to the 8px grid by the builder.
    pub x: f64,
    /// Vertical offset.
    pub y: f64,
}

impl Default for Position {
    fn default() -> Self {
        Self { x: 40.0, y: 40.0 }
    }
}

/// One node of a graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Stable id within the graph: what edges and `workflow_steps.node_id` refer to.
    pub id: String,
    /// A registry key — `trigger.event`, `condition`, `action`, … (see [`node_types`]).
    #[serde(rename = "type")]
    pub node_type: String,
    /// What the canvas draws on the card.
    pub label: String,
    /// The node's parameters, in the shape the registry's parameter schema describes.
    #[serde(default)]
    pub params: Value,
    /// Where it sits.
    #[serde(default)]
    pub position: Position,
}

/// One connection between two nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// Stable id within the graph.
    pub id: String,
    /// Node the edge leaves.
    pub source: String,
    /// Output port it leaves from (`out`, `true`, `false`, `default`, a switch case).
    #[serde(default = "default_source_port")]
    pub source_port: String,
    /// Node it arrives at.
    pub target: String,
}

fn default_source_port() -> String {
    "out".to_owned()
}

/// A whole definition as the builder draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    /// The nodes, in draw order.
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// The connections.
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl Graph {
    /// The node with this id, if the graph has one.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Edges leaving a node, in draw order.
    #[must_use]
    pub fn edges_from(&self, id: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|edge| edge.source == id).collect()
    }

    /// `true` when no node of this type exists.
    #[must_use]
    pub fn has_type(&self, node_type: &str) -> bool {
        self.nodes.iter().any(|node| node.node_type == node_type)
    }

    /// The graph a new definition starts from: a trigger, an end, and one edge between them.
    ///
    /// A definition that is born valid is the cheapest way to make "a workflow created before
    /// this tick opens in the builder" true for every future row too — there is no shape an
    /// author has to repair.
    #[must_use]
    pub fn starter(kind: &str, event: Option<&str>) -> Self {
        let node_type = match kind {
            "schedule" => "trigger.schedule",
            "manual" => "trigger.manual",
            _ => "trigger.event",
        };
        let mut params = json!({ "kind": kind });
        if let Some(event) = event {
            params["event"] = json!(event);
        }
        Self {
            nodes: vec![
                Node {
                    id: "trigger".to_owned(),
                    node_type: node_type.to_owned(),
                    label: "Trigger".to_owned(),
                    params,
                    position: Position { x: 40.0, y: 40.0 },
                },
                Node {
                    id: "end".to_owned(),
                    node_type: "end".to_owned(),
                    label: "End".to_owned(),
                    params: Value::Object(serde_json::Map::new()),
                    position: Position { x: 340.0, y: 40.0 },
                },
            ],
            edges: vec![Edge {
                id: "e0".to_owned(),
                source: "trigger".to_owned(),
                source_port: "out".to_owned(),
                target: "end".to_owned(),
            }],
        }
    }
}

/// One output port of a node type.
///
/// `Serialize` only: the registry is a compiled-in constant, so nothing ever reads a port
/// back from JSON — the API *writes* it into the palette response and the client draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Port {
    /// The port's name — what an edge's `source_port` refers to.
    pub key: &'static str,
    /// What leaving through this port means, in the palette's words.
    pub label: &'static str,
    /// `true` when leaving on this port ends the run (a stop port).
    pub terminal: bool,
}

impl Port {
    const fn new(key: &'static str, label: &'static str, terminal: bool) -> Self {
        Self {
            key,
            label,
            terminal,
        }
    }
}

/// One kind of node the palette offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NodeType {
    /// Registry key, stored on the node as `type`.
    pub key: &'static str,
    /// What the palette calls it.
    pub label: &'static str,
    /// Which group of the palette rail it sits in: Trigger / Logic / Actions / Data / Plugins.
    pub category: &'static str,
    /// One line under the card — what it does, without the word "node".
    pub summary: &'static str,
    /// Output ports, in draw order.
    pub outputs: &'static [Port],
    /// The parameter fields, as `{"field": {"type": …, "label": …, "required": …}}`.
    ///
    /// A schema rather than a form, because the palette, the inspector and the server's
    /// validator all read the same object: REQ-003 already had a rule that "the action the
    /// matcher could not run is refused when it is written", and a schema the server ignores is
    /// a form that lies.
    pub params: &'static [ParamField],
    /// `true` when the node is decoration the engine never runs.
    pub inert: bool,
}

/// One field of a node's parameter form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ParamField {
    /// Key in the node's `params` object.
    pub key: &'static str,
    /// Label above the input.
    pub label: &'static str,
    /// `text`, `textarea`, `number`, `select` or `boolean`.
    pub kind: &'static str,
    /// `true` when the node is refused without it.
    pub required: bool,
    /// The legal values of a `select`, in palette order.
    pub options: &'static [&'static str],
    /// Help text under the input.
    pub help: &'static str,
}

impl ParamField {
    const fn text(key: &'static str, label: &'static str, help: &'static str) -> Self {
        Self {
            key,
            label,
            kind: "text",
            required: false,
            options: &[],
            help,
        }
    }

    const fn required_text(key: &'static str, label: &'static str, help: &'static str) -> Self {
        Self {
            key,
            label,
            kind: "text",
            required: true,
            options: &[],
            help,
        }
    }

    const fn select(
        key: &'static str,
        label: &'static str,
        options: &'static [&'static str],
        help: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            kind: "select",
            required: true,
            options,
            help,
        }
    }
}

const OUT: &[Port] = &[Port::new("out", "Next", false)];
const BRANCH_PORTS: &[Port] = &[
    Port::new("true", "True", false),
    Port::new("false", "False", true),
];
const TASK_PORTS: &[Port] = &[
    Port::new("success", "Succeeded", false),
    Port::new("error", "Failed", true),
];
const SWITCH_PORTS: &[Port] = &[
    Port::new("case_1", "Case 1", false),
    Port::new("default", "Default", false),
];
const NOTE_PORTS: &[Port] = &[];

const ACTION_FIELD: ParamField = ParamField::required_text(
    "action",
    "Action",
    "The action key this node runs. GET /api/v1/automations/catalogue lists every legal one.",
);
const WAIT_FIELD: ParamField = ParamField {
    key: "seconds",
    label: "Seconds",
    kind: "number",
    required: true,
    options: &[],
    help: "How long the run parks here, 1 to 86400.",
};
const CONDITION_FIELD: ParamField = ParamField::required_text(
    "field",
    "Field",
    "What the comparison reads, e.g. {{event.payload.role}}.",
);
const CONDITION_OP: ParamField = ParamField::select(
    "operator",
    "Operator",
    OPERATORS,
    "How the field is compared against the value.",
);

/// Every node type the palette offers, in rail order.
///
/// A closed set, like the action catalogue: the engine knows what a node *is* without a
/// plugin, and a plugin node (REQ-004 slice 4) is added beside these rather than in place of
/// them, so a plugin upgrade cannot leave a rule that the core cannot explain.
pub const NODE_TYPES: &[NodeType] = &[
    NodeType {
        key: "trigger.event",
        label: "Event",
        category: "Trigger",
        summary: "Runs when the platform records an event.",
        outputs: OUT,
        params: &[
            ParamField::required_text(
                "event",
                "Event name",
                "A lower-case dotted name, e.g. page.published.",
            ),
            ParamField {
                key: "conditions",
                label: "Conditions",
                kind: "textarea",
                required: false,
                options: &[],
                help: "The comparisons an event payload must satisfy, as the rule's own group tree.",
            },
        ],
        inert: false,
    },
    NodeType {
        key: "trigger.schedule",
        label: "Schedule",
        category: "Trigger",
        summary: "Runs on a cron expression, in UTC.",
        outputs: OUT,
        params: &[ParamField::required_text(
            "cron",
            "Cron expression",
            "Five fields in UTC, e.g. 0 9 * * 1-5.",
        )],
        inert: false,
    },
    NodeType {
        key: "trigger.manual",
        label: "Manual",
        category: "Trigger",
        summary: "Starts only when a person presses Run.",
        outputs: OUT,
        params: &[],
        inert: false,
    },
    NodeType {
        key: "condition",
        label: "If / else",
        category: "Logic",
        summary: "Continues on true, ends the run on false.",
        outputs: BRANCH_PORTS,
        params: &[
            CONDITION_FIELD,
            CONDITION_OP,
            ParamField::required_text("value", "Value", "What the field is compared against."),
        ],
        inert: false,
    },
    NodeType {
        key: "switch",
        label: "Switch",
        category: "Logic",
        summary: "One branch per case, plus a default.",
        outputs: SWITCH_PORTS,
        params: &[ParamField::required_text(
            "cases",
            "Cases",
            "One \"value → label\" per line; the last port is the default.",
        )],
        inert: false,
    },
    NodeType {
        key: "action",
        label: "Action",
        category: "Actions",
        summary: "Runs one action from the catalogue.",
        outputs: TASK_PORTS,
        params: &[
            ACTION_FIELD,
            ParamField::text(
                "parameters",
                "Parameters",
                "The action's own parameters as JSON.",
            ),
        ],
        inert: false,
    },
    NodeType {
        key: "wait",
        label: "Wait",
        category: "Logic",
        summary: "Parks the run, then continues.",
        outputs: OUT,
        params: &[WAIT_FIELD],
        inert: false,
    },
    NodeType {
        key: "approval",
        label: "Approval",
        category: "Logic",
        summary: "Parks the run until a person decides.",
        outputs: TASK_PORTS,
        params: &[
            ParamField::required_text(
                "permission",
                "Permission",
                "Who may let the run go on, e.g. workflows.approve.",
            ),
            ParamField::text("message", "Message", "What the approver is asked."),
            ParamField {
                key: "expires_in_hours",
                label: "Expires after (hours)",
                kind: "number",
                required: false,
                options: &[],
                help: "How long the gate waits before it gives up. 1 to 720.",
            },
        ],
        inert: false,
    },
    NodeType {
        key: "http_request",
        label: "HTTP request",
        category: "Data",
        summary: "Calls an allowed host, signed with the rule's key.",
        outputs: TASK_PORTS,
        params: &[
            ParamField::required_text("url", "URL", "The host to call; allow-listed on write."),
            ParamField::select(
                "method",
                "Method",
                crate::actions::OUTBOUND_METHODS,
                "The HTTP method.",
            ),
            ParamField::text("body", "Body", "The request body as JSON."),
            ParamField {
                key: "timeout_ms",
                label: "Timeout (ms)",
                kind: "number",
                required: false,
                options: &[],
                help: "How long the call may block. Capped by the rule's bound.",
            },
        ],
        inert: false,
    },
    NodeType {
        key: "transform",
        label: "Transform",
        category: "Data",
        summary: "Builds fields from a template.",
        outputs: OUT,
        params: &[ParamField::required_text(
            "template",
            "Template",
            "The fields to build, as a JSON object of {{ }} expressions.",
        )],
        inert: false,
    },
    NodeType {
        key: "sub_workflow",
        label: "Sub-workflow",
        category: "Actions",
        summary: "Runs another rule, then continues.",
        outputs: TASK_PORTS,
        params: &[ParamField::required_text(
            "workflow_id",
            "Rule",
            "The rule to run, by id.",
        )],
        inert: false,
    },
    NodeType {
        key: "end",
        label: "End",
        category: "Logic",
        summary: "Ends the run, optionally with a reason.",
        outputs: &[],
        params: &[ParamField::text(
            "reason",
            "Reason",
            "What the trace shows when the run stops here.",
        )],
        inert: false,
    },
    NodeType {
        key: "note",
        label: "Note",
        category: "Logic",
        summary: "A sticky comment the engine never runs.",
        outputs: NOTE_PORTS,
        params: &[ParamField::required_text(
            "text",
            "Note",
            "What the note says on the canvas.",
        )],
        // Decoration: it is not reachable, not an orphan, and projects to nothing.
        inert: true,
    },
];

/// The node type with this key.
#[must_use]
pub fn find_node_type(key: &str) -> Option<&'static NodeType> {
    NODE_TYPES.iter().find(|node_type| node_type.key == key)
}

/// The node type that projects onto this step kind, for the SQL backfill's mapping.
#[must_use]
pub const fn node_type_for_step(kind: &str) -> &'static str {
    match kind.as_bytes() {
        b"wait" => "wait",
        b"branch" => "condition",
        b"stop" => "end",
        b"approval" => "approval",
        _ => "action",
    }
}

/// `true` when a node type is a trigger — the graph may carry exactly one.
#[must_use]
pub fn is_trigger_type(key: &str) -> bool {
    key.starts_with("trigger.")
}

/// The ports a node of this type exports, or an empty slice for an unknown type.
#[must_use]
pub fn ports_of(key: &str) -> &'static [Port] {
    find_node_type(key).map_or(&[], |node_type| node_type.outputs)
}

/// Check the graph and answer what is wrong with it.
///
/// Findings, not an error: the panel shows all of them at once with a jump link each, and a
/// save is refused only when at least one is an error. A warning is something a person may
/// legitimately accept — a `note` with no connection, say.
#[must_use]
pub fn validate(graph: &Graph) -> Vec<Finding> {
    let mut findings = Vec::new();

    if graph.nodes.is_empty() {
        findings.push(Finding::error(
            "graph_empty",
            "the graph has no nodes — a definition needs at least a trigger",
            None,
        ));
        return findings;
    }
    if graph.nodes.len() > MAX_NODES {
        findings.push(Finding::error(
            "too_many_nodes",
            format!(
                "a graph may carry at most {MAX_NODES} nodes, this one has {}",
                graph.nodes.len()
            ),
            None,
        ));
    }
    if graph.edges.len() > MAX_EDGES {
        findings.push(Finding::error(
            "too_many_edges",
            format!(
                "a graph may carry at most {MAX_EDGES} connections, this one has {}",
                graph.edges.len()
            ),
            None,
        ));
    }

    // Node identity. A duplicate id is the failure every other check then mis-reports, so it
    // comes first and names the id twice.
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    for node in &graph.nodes {
        *ids.entry(node.id.as_str()).or_insert(0) += 1;
    }
    for (id, count) in &ids {
        if *count > 1 {
            findings.push(Finding::error(
                "duplicate_node_id",
                format!("{count} nodes are called {id:?} — every node needs its own id"),
                Some(id),
            ));
        }
    }
    for node in &graph.nodes {
        if node.id.is_empty() || node.id.len() > MAX_NODE_ID {
            findings.push(Finding::error(
                "invalid_node_id",
                format!(
                    "a node id is 1 to {MAX_NODE_ID} characters, got {:?}",
                    truncate(&node.id)
                ),
                Some(&node.id),
            ));
        }
        if node.label.trim().is_empty() {
            findings.push(Finding::error(
                "node_label_required",
                "a node needs a label — the canvas draws this card by it",
                Some(&node.id),
            ));
        } else if node.label.len() > MAX_NODE_LABEL {
            findings.push(Finding::warning(
                "node_label_truncated",
                format!(
                    "the label is {} characters and the card draws {} of them",
                    node.label.len(),
                    MAX_NODE_LABEL
                ),
                Some(&node.id),
            ));
        }
        if !node.position.x.is_finite()
            || !node.position.y.is_finite()
            || node.position.x < MIN_COORD
            || node.position.x > MAX_COORD
            || node.position.y < MIN_COORD
            || node.position.y > MAX_COORD
        {
            findings.push(Finding::error(
                "invalid_position",
                format!(
                    "a node sits at ({}, {}) and the canvas only draws between {MIN_COORD} and {MAX_COORD}",
                    node.position.x, node.position.y
                ),
                Some(&node.id),
            ));
        }
        match find_node_type(&node.node_type) {
            None => findings.push(Finding::error(
                "unknown_node_type",
                format!(
                    "{:?} is not a node type the platform knows — the palette lists the legal ones",
                    node.node_type
                ),
                Some(&node.id),
            )),
            Some(node_type) => {
                findings.extend(validate_params(node, node_type));
            }
        }
    }

    // Exactly one trigger. Zero means nothing can start the definition; two means the author
    // cannot say which one fires.
    let triggers: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|node| is_trigger_type(&node.node_type))
        .collect();
    match triggers.as_slice() {
        [] => findings.push(Finding::error(
            "no_trigger",
            "the graph has no trigger — a definition cannot start itself",
            None,
        )),
        [one] => {
            if !graph.edges.iter().any(|edge| edge.source == one.id) {
                findings.push(Finding::error(
                    "trigger_not_connected",
                    format!("the trigger {:?} goes nowhere", one.label),
                    Some(&one.id),
                ));
            }
        }
        many => {
            for trigger in many {
                findings.push(Finding::error(
                    "multiple_triggers",
                    format!(
                        "{:?} is a second trigger — a definition starts from exactly one",
                        trigger.label
                    ),
                    Some(&trigger.id),
                ));
            }
        }
    }

    // Edges: endpoints must exist, the port must be one the source exports, and the same
    // pair of ports may be connected once.
    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    for edge in &graph.edges {
        let source = graph.node(&edge.source);
        let target = graph.node(&edge.target);
        let (Some(source), Some(target)) = (source, target) else {
            findings.push(Finding::error(
                "edge_dangling",
                format!(
                    "the connection {:?} → {:?} names a node the graph does not have",
                    edge.source, edge.target
                ),
                graph.node(&edge.source).map(|node| node.id.as_str()),
            ));
            continue;
        };
        if let Some(node_type) = find_node_type(&source.node_type) {
            if node_type.outputs.is_empty() {
                findings.push(Finding::error(
                    "port_missing",
                    format!(
                        "{:?} exports no port, so nothing can leave it",
                        source.label
                    ),
                    Some(&source.id),
                ));
            } else if !node_type
                .outputs
                .iter()
                .any(|port| port.key == edge.source_port)
            {
                findings.push(Finding::error(
                    "unknown_source_port",
                    format!(
                        "{:?} has no {:?} port — it exports {}",
                        source.label,
                        edge.source_port,
                        node_type
                            .outputs
                            .iter()
                            .map(|port| format!("{:?}", port.key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(&source.id),
                ));
            }
        }
        if edge.source == edge.target {
            findings.push(Finding::error(
                "edge_self_loop",
                format!("{:?} is connected to itself", source.label),
                Some(&source.id),
            ));
        }
        let key = (
            edge.source.as_str(),
            edge.source_port.as_str(),
            edge.target.as_str(),
        );
        if !seen.insert(key) {
            findings.push(Finding::error(
                "duplicate_edge",
                format!(
                    "{:?} → {:?} is connected twice on the same port",
                    source.label, target.label
                ),
                Some(&source.id),
            ));
        }
    }

    // Reachability from the trigger, and orphans.
    let trigger_id = triggers.first().map(|node| node.id.as_str());
    let reachable = trigger_id
        .map(|id| reachable_from(graph, id))
        .unwrap_or_default();
    for node in &graph.nodes {
        if is_trigger_type(&node.node_type)
            || find_node_type(&node.node_type).is_some_and(|t| t.inert)
        {
            continue;
        }
        if !reachable.contains(node.id.as_str()) {
            findings.push(Finding::error(
                "orphan_node",
                format!(
                    "{:?} is not reachable from the trigger — it would never run",
                    node.label
                ),
                Some(&node.id),
            ));
        }
    }

    // A cycle. The engine executes an ordered list, so a graph that loops has no projection
    // at all; this is refused at write time rather than at run time.
    if let Some(cycle) = find_cycle(graph) {
        let names: Vec<&str> = cycle
            .iter()
            .map(|id| {
                graph
                    .node(id)
                    .map_or(id.as_str(), |node| node.label.as_str())
            })
            .collect();
        findings.push(
            Finding::error(
                "graph_cycle",
                format!(
                    "the connections form a loop ({} → {}), and a run has no way to leave it",
                    names.join(" → "),
                    names.first().copied().unwrap_or_default()
                ),
                cycle.first().map(String::as_str),
            )
            .related(cycle.get(1).map(String::as_str)),
        );
    }

    // Every non-terminal node needs a way out, or the run stops in the middle with nothing
    // to say about it.
    for node in &graph.nodes {
        let Some(node_type) = find_node_type(&node.node_type) else {
            continue;
        };
        if node_type.inert || node_type.outputs.is_empty() {
            continue;
        }
        if reachable.contains(node.id.as_str()) && !graph.edges.iter().any(|e| e.source == node.id)
        {
            findings.push(Finding::error(
                "dangling_output",
                format!(
                    "{:?} has no connection leaving it — the run would stop here",
                    node.label
                ),
                Some(&node.id),
            ));
        }
    }

    findings
}

/// A node's own parameters against the registry's schema.
fn validate_params(node: &Node, node_type: &NodeType) -> Vec<Finding> {
    let mut findings = Vec::new();
    for field in node_type.params {
        if field.required {
            let present = node
                .params
                .get(field.key)
                .is_some_and(|value| !value.is_null() && value.as_str() != Some(""));
            if !present {
                findings.push(Finding::error(
                    "missing_parameter",
                    format!("{:?} needs {} ({})", node.label, field.label, field.key),
                    Some(&node.id),
                ));
            }
        }
        if !field.options.is_empty() {
            if let Some(Value::String(chosen)) = node.params.get(field.key) {
                if !field.options.contains(&chosen.as_str()) {
                    findings.push(Finding::error(
                        "invalid_parameter",
                        format!(
                            "{:?} has {chosen:?} for {}, which is one of {}",
                            node.label,
                            field.key,
                            field.options.join(", ")
                        ),
                        Some(&node.id),
                    ));
                }
            }
        }
    }
    // A condition node's field path is the one thing the engine will read literally, so it is
    // bounded here rather than at run time.
    if node_type.key == "condition" {
        if let Some(Value::String(field)) = node.params.get("field") {
            if field.len() > MAX_FIELD {
                findings.push(Finding::error(
                    "invalid_parameter",
                    format!(
                        "{:?} reads a field path of {} characters; the engine reads up to {MAX_FIELD}",
                        node.label,
                        field.len()
                    ),
                    Some(&node.id),
                ));
            }
        }
    }
    if node_type.key == "wait" {
        if let Some(Value::Number(seconds)) = node.params.get("seconds") {
            if let Some(seconds) = seconds.as_i64() {
                if !(1..=crate::definition::MAX_WAIT_SECONDS).contains(&seconds) {
                    findings.push(Finding::error(
                        "invalid_parameter",
                        format!(
                            "{:?} parks for {seconds} seconds; a wait holds a run for 1 to {}",
                            node.label,
                            crate::definition::MAX_WAIT_SECONDS
                        ),
                        Some(&node.id),
                    ));
                }
            }
        }
    }
    findings
}

/// Every node the trigger can reach, following connections only.
#[must_use]
pub fn reachable_from<'a>(graph: &'a Graph, start: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![start];
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        for edge in graph.edges.iter().filter(|edge| edge.source == id) {
            queue.push(edge.target.as_str());
        }
    }
    seen
}

/// The first cycle in the graph, as the node ids along it.
///
/// Iterative depth-first search with an explicit stack: a 200-node graph is fine, and a
/// recursive walk on a graph that is *allowed* to be a cycle is a stack overflow waiting for
/// the one definition an author drew by accident.
#[must_use]
pub fn find_cycle(graph: &Graph) -> Option<Vec<String>> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (position, node) in graph.nodes.iter().enumerate() {
        index.insert(node.id.as_str(), position);
    }
    // 0 = unvisited, 1 = on the current path, 2 = done.
    let mut state = vec![0u8; graph.nodes.len()];
    for start in 0..graph.nodes.len() {
        if state[start] != 0 {
            continue;
        }
        let mut path: Vec<usize> = vec![start];
        let mut positions: HashMap<usize, usize> = HashMap::new();
        positions.insert(start, 0);
        state[start] = 1;

        while let Some(&current) = path.last() {
            let next = graph
                .edges
                .iter()
                .filter(|edge| edge.source == graph.nodes[current].id)
                .find_map(|edge| index.get(edge.target.as_str()).copied());

            match next {
                Some(target) if state[target] == 1 => {
                    let start_at = positions[&target];
                    let cycle: Vec<String> = path[start_at..]
                        .iter()
                        .map(|position| graph.nodes[*position].id.clone())
                        .collect();
                    return Some(cycle);
                }
                Some(target) if state[target] == 0 => {
                    state[target] = 1;
                    positions.insert(target, path.len());
                    path.push(target);
                }
                _ => {
                    state[current] = 2;
                    positions.remove(&current);
                    path.pop();
                }
            }
        }
    }
    None
}

/// Turn a graph into the ordered step list the runner executes.
///
/// This is the single place the projection happens; every save goes through it, so a rule
/// cannot be projected two different ways. It returns the steps *and* the node id that
/// produced each, because `workflow_steps.node_id` is what paints the canvas after a run.
///
/// A node the projection cannot express is refused here, not skipped: silently dropping a
/// node would leave a rule that runs and does not do what its canvas shows.
pub fn project(graph: &Graph) -> Result<Vec<(String, StepDefinition)>> {
    let findings: Vec<Finding> = validate(graph)
        .into_iter()
        .filter(Finding::is_error)
        .collect();
    if let Some(first) = findings.first() {
        return Err(WorkflowError::invalid(
            "graph_invalid",
            format!("{} ({} finding(s) in total)", first.message, findings.len()),
        ));
    }

    let trigger = graph
        .nodes
        .iter()
        .find(|node| is_trigger_type(&node.node_type))
        .ok_or_else(|| WorkflowError::invalid("graph_invalid", "the graph has no trigger"))?;

    let mut steps: Vec<(String, StepDefinition)> = Vec::new();
    let mut current = trigger.id.clone();
    // The v0 engine has one branching step, so a graph walk follows the *true* edge and stops
    // at a false one. `visited` is what turns a loop the validator refused into a bounded
    // walk rather than a hang, should one ever be stored by hand.
    let mut visited: BTreeSet<String> = BTreeSet::new();

    while visited.insert(current.clone()) {
        if steps.len() >= MAX_STEPS {
            return Err(WorkflowError::invalid(
                "too_many_steps",
                format!("the graph projects to more than {MAX_STEPS} steps"),
            ));
        }
        let node = graph.node(&current).ok_or_else(|| {
            WorkflowError::invalid(
                "graph_invalid",
                format!("the graph has no node {current:?}"),
            )
        })?;

        match find_node_type(&node.node_type) {
            // A trigger is where the run starts, not something the runner steps through, and
            // a note is decoration. Neither contributes a step.
            Some(node_type) if node_type.inert || is_trigger_type(&node.node_type) => {}
            Some(node_type) => {
                let step = step_for(node, node_type, graph)?;
                steps.push((node.id.clone(), step));
            }
            None => {
                return Err(WorkflowError::invalid(
                    "unknown_node_type",
                    format!("{:?} is not a node type the platform knows", node.node_type),
                ));
            }
        }

        let next = graph
            .edges
            .iter()
            .find(|edge| edge.source == node.id && follows(edge.source_port.as_str()));
        let Some(next) = next else { break };
        current = next.target.clone();
    }

    if steps.is_empty() {
        return Err(WorkflowError::invalid(
            "graph_invalid",
            "the graph projects to no steps — nothing would run",
        ));
    }
    Ok(steps)
}

/// Which output port the linear walk follows.
fn follows(port: &str) -> bool {
    matches!(port, "out" | "true" | "success" | "case_1" | "default")
}

/// The step one node projects onto.
fn step_for(node: &Node, node_type: &NodeType, graph: &Graph) -> Result<StepDefinition> {
    let name = node.label.clone();
    let params = &node.params;
    match node_type.key {
        "end" => Ok(StepDefinition::stop(
            name,
            params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("the definition ends here"),
        )),
        "wait" => {
            let seconds = params
                .get("seconds")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    WorkflowError::invalid(
                        "invalid_parameter",
                        format!("{name:?} parks the run but says for how long"),
                    )
                })?;
            // Read through the engine's own reader, so a projection can never store a wait
            // the engine would refuse to resume.
            crate::definition::wait_seconds_from(&json!({ "seconds": seconds }))?;
            Ok(StepDefinition::wait(name, seconds))
        }
        "condition" => {
            let field = params
                .get("field")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let operator = params
                .get("operator")
                .and_then(Value::as_str)
                .unwrap_or("equals")
                .to_owned();
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            Ok(StepDefinition::branch(name, field, operator, value))
        }
        "approval" => {
            let permission = params.get("permission").and_then(Value::as_str);
            let message = params.get("message").and_then(Value::as_str);
            let expires = params
                .get("expires_in_hours")
                .and_then(Value::as_i64)
                .and_then(|hours| i32::try_from(hours).ok());
            Ok(StepDefinition::approval(name, permission, message, expires))
        }
        "http_request" => {
            // An outbound call is a task step whose action is `http_request`; the engine
            // checks the method and the host against its own lists at run time.
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let method = params
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("POST")
                .to_uppercase();
            let mut call = json!({ "url": url, "method": method });
            if let Some(body) = params.get("body") {
                call["body"] = body.clone();
            }
            if let Some(timeout) = params.get("timeout_ms").and_then(Value::as_i64) {
                call["timeout_ms"] = json!(timeout);
            }
            Ok(StepDefinition::task(name, "http_request", call))
        }
        "transform" => {
            let template = params.get("template").cloned().unwrap_or_else(|| json!({}));
            Ok(StepDefinition::task(name, "echo", template))
        }
        "sub_workflow" => {
            let target = params
                .get("workflow_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WorkflowError::invalid(
                        "invalid_parameter",
                        format!("{name:?} names no rule to run"),
                    )
                })?;
            let parsed = Uuid::parse_str(target).map_err(|_| {
                WorkflowError::invalid(
                    "invalid_parameter",
                    format!("{name:?} names {target:?}, which is not a rule id"),
                )
            })?;
            Ok(StepDefinition::task(
                name,
                "run_workflow",
                json!({ "workflow_id": parsed }),
            ))
        }
        "action" => {
            let action = params
                .get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WorkflowError::invalid(
                        "invalid_parameter",
                        format!("{name:?} names no action to run"),
                    )
                })?;
            let action_params = match params.get("parameters") {
                Some(Value::String(raw)) if !raw.trim().is_empty() => serde_json::from_str(raw)
                    .map_err(|error| {
                        WorkflowError::invalid(
                            "invalid_parameter",
                            format!("{name:?} has parameters that are not JSON: {error}"),
                        )
                    })?,
                Some(value @ Value::Object(_)) => value.clone(),
                _ => Value::Object(serde_json::Map::new()),
            };
            Ok(StepDefinition::task(name, action, action_params))
        }
        other => Err(WorkflowError::invalid(
            "unknown_node_type",
            format!("{other:?} does not project onto a step"),
        )),
    }
}

/// A viewport inside [`MIN_ZOOM`]`..=`[`MAX_ZOOM`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// Horizontal pan.
    pub x: f64,
    /// Vertical pan.
    pub y: f64,
    /// Zoom factor, clamped on read.
    pub zoom: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

impl Viewport {
    /// Clamp a viewport a client posted: a zoom of 0 or 40 is a client bug, and refusing the
    /// whole save over it would lose the author's real edit.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        if !self.x.is_finite() {
            self.x = 0.0;
        }
        if !self.y.is_finite() {
            self.y = 0.0;
        }
        if !self.zoom.is_finite() {
            self.zoom = 1.0;
        }
        self.x = self.x.clamp(MIN_COORD, MAX_COORD);
        self.y = self.y.clamp(MIN_COORD, MAX_COORD);
        self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self
    }

    /// Zoom one step in, staying inside the bounds.
    #[must_use]
    pub fn zoomed(self, direction: Zoom) -> Self {
        let zoom = (self.zoom + direction.delta() * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM);
        Self { zoom, ..self }
    }
}

/// Which way a zoom key moves the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    /// Closer.
    In,
    /// Further away.
    Out,
}

impl Zoom {
    const fn delta(self) -> f64 {
        match self {
            Self::In => 1.0,
            Self::Out => -1.0,
        }
    }
}

/// The layout half of `workflows.ui_state`: never read by the engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    /// Where the canvas is looking.
    #[serde(default)]
    pub viewport: Viewport,
    /// Explicit positions, when the client keeps them outside the nodes.
    ///
    /// `None` for a save that carries no layout change — which is the case that must not bump
    /// `graph_version`, so the builder can pan and zoom all day without a single version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions: Option<serde_json::Map<String, Value>>,
}

/// Snap a coordinate to the 8px grid the builder draws.
#[must_use]
pub fn snap(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value / 8.0).round() * 8.0
}

fn truncate(value: &str) -> String {
    if value.chars().count() <= 40 {
        return value.to_owned();
    }
    value.chars().take(37).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, node_type: &str) -> Node {
        Node {
            id: id.to_owned(),
            node_type: node_type.to_owned(),
            label: id.to_owned(),
            params: Value::Object(serde_json::Map::new()),
            position: Position::default(),
        }
    }

    fn action(id: &str) -> Node {
        let mut node = node(id, "action");
        node.params = json!({ "action": "echo", "parameters": { "value": 1 } });
        node
    }

    /// trigger → one action → end
    fn linear() -> Graph {
        Graph {
            nodes: vec![
                node("trigger", "trigger.manual"),
                action("a1"),
                node("end", "end"),
            ],
            edges: vec![
                Edge {
                    id: "e0".to_owned(),
                    source: "trigger".to_owned(),
                    source_port: "out".to_owned(),
                    target: "a1".to_owned(),
                },
                Edge {
                    id: "e1".to_owned(),
                    source: "a1".to_owned(),
                    source_port: "success".to_owned(),
                    target: "end".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn a_starter_graph_is_valid_and_projects_to_one_step() {
        let graph = Graph::starter("manual", None);
        let all = validate(&graph);
        let codes: Vec<&str> = all.iter().map(|finding| finding.code.as_str()).collect();
        assert!(
            codes.is_empty(),
            "a new definition must be born valid: {codes:?}"
        );

        let steps = project(&graph).expect("a starter graph projects");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].0, "end");
        assert_eq!(steps[0].1.kind, crate::model::StepKind::Stop);
    }

    #[test]
    fn a_linear_graph_projects_to_the_steps_the_runner_expects() {
        let steps = project(&linear()).expect("a linear graph projects");
        let names: Vec<&str> = steps.iter().map(|(_, step)| step.name.as_str()).collect();
        assert_eq!(names, vec!["a1", "end"]);

        // The projection is the engine's own type, and the runner needs no change to read it.
        let encoded = serde_json::to_value(&steps[0].1).expect("a step serialises");
        assert_eq!(encoded["name"], "a1");
        assert_eq!(encoded["kind"], "task");
        assert_eq!(encoded["action"], "echo");
    }

    #[test]
    fn a_condition_projects_to_a_branch_and_the_false_edge_ends_the_run() {
        let mut graph = linear();
        let mut condition = node("c1", "condition");
        condition.params =
            json!({ "field": "{{event.role}}", "operator": "equals", "value": "admin" });
        graph.nodes.insert(1, condition);
        // trigger → c1, c1 true → a1, a1 success → end; the false edge goes nowhere.
        graph.edges = vec![
            Edge {
                id: "e0".to_owned(),
                source: "trigger".to_owned(),
                source_port: "out".to_owned(),
                target: "c1".to_owned(),
            },
            Edge {
                id: "e1".to_owned(),
                source: "c1".to_owned(),
                source_port: "true".to_owned(),
                target: "a1".to_owned(),
            },
            Edge {
                id: "e2".to_owned(),
                source: "a1".to_owned(),
                source_port: "success".to_owned(),
                target: "end".to_owned(),
            },
        ];

        let steps = project(&graph).expect("a condition projects");
        let branch = &steps[0].1;
        assert_eq!(branch.name, "c1");
        assert_eq!(branch.kind, crate::model::StepKind::Branch);
        assert_eq!(branch.params["operator"], "equals");
        assert_eq!(branch.params["value"], "admin");
        // The false edge is a stop, not a second path: the projection walks the true edge.
        let names: Vec<&str> = steps.iter().map(|(_, step)| step.name.as_str()).collect();
        assert_eq!(names, vec!["c1", "a1", "end"]);
    }

    #[test]
    fn a_cycle_is_refused_and_names_the_loop() {
        let mut graph = linear();
        graph.edges.push(Edge {
            id: "e2".to_owned(),
            source: "end".to_owned(),
            source_port: "default".to_owned(),
            target: "a1".to_owned(),
        });
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "graph_cycle")
            .expect("a loop is found");
        assert!(
            finding.message.contains("a1"),
            "names the node: {}",
            finding.message
        );

        // And a graph with a cycle cannot be stored, rather than stored and hanging at run time.
        let error = project(&graph).expect_err("a cycle does not project");
        assert_eq!(error.code(), "graph_invalid");
    }

    #[test]
    fn a_second_trigger_is_refused_by_name() {
        let mut graph = linear();
        graph.nodes.push(node("trigger2", "trigger.schedule"));
        let findings = validate(&graph);
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.code == "multiple_triggers")
                .count(),
            2,
            "both triggers are told, not just the second"
        );
    }

    #[test]
    fn an_orphan_is_refused_by_name() {
        let mut graph = linear();
        graph.nodes.push(action("a2"));
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "orphan_node")
            .expect("an unconnected action is an orphan");
        assert_eq!(finding.node_id.as_deref(), Some("a2"));
        assert!(finding.message.contains("a2"));
    }

    #[test]
    fn a_duplicate_edge_is_refused() {
        let mut graph = linear();
        graph.edges.push(Edge {
            id: "e-dup".to_owned(),
            source: "a1".to_owned(),
            source_port: "success".to_owned(),
            target: "end".to_owned(),
        });
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "duplicate_edge")
            .expect("the same port may be connected once");
        assert_eq!(finding.node_id.as_deref(), Some("a1"));
    }

    #[test]
    fn an_incompatible_port_is_refused_with_the_legal_ports() {
        let mut graph = linear();
        // `manual` triggers export one port called `out`; there is no `true`.
        graph.edges[0].source_port = "true".to_owned();
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "unknown_source_port")
            .expect("a port the source does not export is refused");
        assert!(
            finding.message.contains("out"),
            "the message lists the legal port"
        );
    }

    #[test]
    fn a_node_with_no_way_out_is_refused() {
        let mut graph = linear();
        graph.edges.retain(|edge| edge.source != "a1");
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "dangling_output")
            .expect("a reachable node that goes nowhere stops the run in the middle");
        assert_eq!(finding.node_id.as_deref(), Some("a1"));
    }

    #[test]
    fn a_missing_required_parameter_names_the_field() {
        let mut graph = linear();
        graph.nodes[1].params = json!({});
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "missing_parameter")
            .expect("an action without an action is refused");
        assert!(finding.message.contains("action"), "{}", finding.message);
    }

    #[test]
    fn a_duplicate_node_id_is_refused_before_anything_else_misreports_it() {
        let mut graph = linear();
        graph.nodes.push(action("a1"));
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "duplicate_node_id")
            .expect("two nodes may not share an id");
        assert!(finding.message.contains("a1"));
    }

    #[test]
    fn a_clean_graph_reports_no_problems() {
        let all = validate(&linear());
        let codes: Vec<&str> = all.iter().map(|finding| finding.code.as_str()).collect();
        assert!(
            codes.is_empty(),
            "the problems panel must be able to say nothing: {codes:?}"
        );
    }

    #[test]
    fn a_note_is_decoration_and_never_projects() {
        let mut graph = linear();
        let mut note = node("n1", "note");
        note.params = json!({ "text": "remember to check the bounce list" });
        graph.nodes.insert(1, note);
        // A note is not required to be reachable, and it contributes no step.
        let steps = project(&graph).expect("a note does not break the projection");
        assert_eq!(steps.len(), 2);
        assert!(validate(&graph).is_empty(), "{:?}", validate(&graph));
    }

    #[test]
    fn the_registry_has_a_legend_for_every_node_type() {
        for node_type in NODE_TYPES {
            assert!(!node_type.key.is_empty(), "a node type needs a key");
            assert!(
                !node_type.summary.is_empty(),
                "{} has no summary",
                node_type.key
            );
            assert!(
                !node_type.category.is_empty(),
                "{} has no palette group",
                node_type.key
            );
            if is_trigger_type(node_type.key) {
                assert_eq!(node_type.category, "Trigger");
            }
        }
        // Node types the SQL backfill writes must exist in the registry, or a backfilled rule
        // opens in the builder already invalid.
        for kind in ["task", "wait", "branch", "stop", "approval"] {
            assert!(
                find_node_type(node_type_for_step(kind)).is_some(),
                "the backfill can write {kind:?} and the registry must know it"
            );
        }
    }

    #[test]
    fn a_zoom_stays_inside_the_canvas_bounds() {
        let start = Viewport {
            x: 0.0,
            y: 0.0,
            zoom: MAX_ZOOM,
        };
        assert_eq!(start.zoomed(Zoom::In).zoom, MAX_ZOOM);
        let tiny = Viewport {
            x: 0.0,
            y: 0.0,
            zoom: MIN_ZOOM,
        }
        .zoomed(Zoom::Out);
        assert_eq!(tiny.zoom, MIN_ZOOM);

        // A client that posts a zoom of 0 or NaN gets a usable viewport, not a refused save.
        let nonsense = Viewport {
            x: 0.0,
            y: 0.0,
            zoom: f64::NAN,
        }
        .clamped();
        assert!((nonsense.zoom - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn positions_snap_to_the_grid() {
        assert!((snap(13.0) - 16.0).abs() < f64::EPSILON);
        assert!((snap(7.0) - 8.0).abs() < f64::EPSILON);
        // A tie rounds away from zero, so a node dragged to exactly 12 lands on 16 and not on
        // 8 — pinned because the two halves must not disagree between two clients.
        assert!((snap(12.0) - 16.0).abs() < f64::EPSILON);
        assert!((snap(f64::NAN) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_graph_survives_a_json_round_trip() {
        let graph = linear();
        let encoded = serde_json::to_string(&graph).expect("a graph serialises");
        let decoded: Graph = serde_json::from_str(&encoded).expect("a graph deserialises");
        assert_eq!(graph, decoded);
    }

    #[test]
    fn a_wait_outside_the_engine_bound_is_refused_by_the_engine_reader() {
        let mut graph = linear();
        graph.nodes[1] = node("w1", "wait");
        graph.nodes[1].params = json!({ "seconds": 999_999 });
        let finding = validate(&graph)
            .into_iter()
            .find(|finding| finding.code == "invalid_parameter")
            .expect("a wait the engine would refuse is refused here");
        assert!(finding.message.contains("86400"), "{}", finding.message);
    }

    #[test]
    fn a_sub_workflow_naming_a_non_uuid_is_refused() {
        let mut graph = linear();
        // Same id as the node it replaces, so the connections still point at it — a test that
        // also broke the edges would fail for the wrong reason.
        graph.nodes[1] = node("a1", "sub_workflow");
        graph.nodes[1].params = json!({ "workflow_id": "not-a-uuid" });
        let error = project(&graph).expect_err("a sub-workflow node needs a rule id");
        // The specific code, not the graph-level one: a client renders the message under the
        // field that caused it, and a generic code would put it under the whole node.
        assert_eq!(error.code(), "invalid_parameter");
        assert!(error.to_string().contains("not-a-uuid"), "{error}");
    }
}
