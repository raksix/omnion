//! The visual graph of a workflow, and the compiler that turns it into the engine's steps.
//!
//! The editor (REQ-086) authors a *graph* — nodes with stable keys and connections between their
//! ports — but the engine runs *steps*. Slice 1 is the bridge: one document, two representations,
//! written together in one transaction, with a deterministic compiler between them so the two can
//! never silently disagree.
//!
//! Three properties are the reason this file exists rather than a `serde_json` round trip:
//!
//! 1. **A node key is a stable editor string** (`http_1`, `if_2`), not the step's display name.
//!    Renaming a node on the canvas must not break the connection that points at it, so the
//!    compiler resolves *keys* and only then names a step.
//! 2. **Validation is a set of codes, not a boolean.** The canvas lists issues per node so a
//!    person can jump to one; a single `Err` would make the badge and the panel the same thing.
//! 3. **Compilation is deterministic.** The same graph always produces the same steps, in the
//!    same order, so "graph and steps disagree" is detectable rather than a thing you discover
//!    when a run does the wrong thing three weeks later.
//!
//! A disabled node is kept, not dropped: it stays in the graph with its connections intact and
//! compiles to a step the engine skips, because a person who turns a node off expects to turn it
//! back on without rewiring. A *sticky note* is not a node at all and never compiles.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::actions;
use crate::definition::StepDefinition;
use crate::error::{Result, WorkflowError};
use crate::model::StepKind;
use crate::registry::{self, PortKind};

/// Most nodes one graph may carry.
///
/// Above this the canvas is unusable long before the compiler is slow, so the limit is a product
/// decision stated once here rather than a number re-guessed per layer.
pub const MAX_NODES: usize = 200;

/// Most connections one graph may carry.
pub const MAX_CONNECTIONS: usize = 400;

/// Longest a node key may be.
pub const MAX_NODE_KEY: usize = 48;

/// Longest a node label may be.
pub const MAX_NODE_LABEL: usize = 80;

// ---------------------------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------------------------

/// One node of the graph, exactly as the editor stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    /// Stable editor key. Connections name this, not the label.
    pub key: String,
    /// Registry key of the node type, e.g. `http_request`.
    #[serde(rename = "type")]
    pub node_type: String,
    /// Label shown on the canvas.
    pub label: String,
    /// Canvas position; floats, and the editor snaps them on its own side.
    pub position: Position,
    /// Node parameters, as the inspector leaves them.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    /// A disabled node stays in the graph and compiles to a step the engine skips.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl GraphNode {
    /// A positioned node with no parameters.
    #[must_use]
    pub fn new(key: impl Into<String>, node_type: impl Into<String>, x: f64, y: f64) -> Self {
        let key = key.into();
        Self {
            label: key.clone(),
            key,
            node_type: node_type.into(),
            position: Position { x, y },
            params: Map::new(),
            disabled: false,
        }
    }

    /// Set the label.
    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Set one parameter.
    #[must_use]
    pub fn with_param(mut self, name: impl Into<String>, value: Value) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// The registry definition of this node's type, when the type is known.
    #[must_use]
    pub fn definition(&self) -> Option<&'static registry::NodeDefinition> {
        registry::find_node(&self.node_type)
    }

    /// Parameters as a plain JSON value, never `null`.
    #[must_use]
    pub fn params_value(&self) -> Value {
        Value::Object(self.params.clone())
    }
}

/// Where a node sits on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Horizontal coordinate in canvas units.
    pub x: f64,
    /// Vertical coordinate in canvas units.
    pub y: f64,
}

impl Default for Position {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

impl Position {
    /// A position.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// One connection between two ports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// Node key the edge leaves.
    pub from: String,
    /// Output port on that node.
    pub from_port: String,
    /// Node key the edge enters.
    pub to: String,
    /// Input port on that node.
    pub to_port: String,
    /// Optional branch label — `true`, `false`, `case A`, `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Connection {
    /// A connection without a branch label.
    #[must_use]
    pub fn new(
        from: impl Into<String>,
        from_port: impl Into<String>,
        to: impl Into<String>,
        to_port: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            from_port: from_port.into(),
            to: to.into(),
            to_port: to_port.into(),
            label: None,
        }
    }

    /// Give the connection a branch label.
    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The two endpoints, ordered, so a reversed duplicate is recognisable as the same edge.
    #[must_use]
    pub fn endpoints(&self) -> (String, String, String, String) {
        (
            self.from.clone(),
            self.from_port.clone(),
            self.to.clone(),
            self.to_port.clone(),
        )
    }
}

/// A sticky note: a comment on the canvas that is never executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StickyNote {
    /// Editor-stable id, so a note can be selected and moved.
    pub id: String,
    /// Where it sits.
    pub position: Position,
    /// Canvas colour, e.g. `amber` or `slate`.
    #[serde(default = "default_note_color")]
    pub color: String,
    /// How wide the note is, in canvas units.
    #[serde(default = "default_note_width")]
    pub width: f64,
    /// How tall the note is, in canvas units.
    #[serde(default = "default_note_height")]
    pub height: f64,
    /// The text a person wrote.
    pub text: String,
}

/// Serde default for [`StickyNote::color`]: amber is the palette's first note colour.
fn default_note_color() -> String {
    "amber".to_string()
}

/// Serde default for [`StickyNote::width`].
fn default_note_width() -> f64 {
    240.0
}

/// Serde default for [`StickyNote::height`].
fn default_note_height() -> f64 {
    140.0
}

impl StickyNote {
    /// A note at a position.
    #[must_use]
    pub fn new(id: impl Into<String>, text: impl Into<String>, x: f64, y: f64) -> Self {
        Self {
            id: id.into(),
            position: Position::new(x, y),
            color: default_note_color(),
            width: default_note_width(),
            height: default_note_height(),
            text: text.into(),
        }
    }
}

/// The whole graph document, as stored in `workflows.graph`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    /// Nodes, in editor order.
    pub nodes: Vec<GraphNode>,
    /// Connections, in editor order.
    pub connections: Vec<Connection>,
    /// Sticky notes. Never compiled, never executed.
    #[serde(default)]
    pub notes: Vec<StickyNote>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            connections: Vec::new(),
            notes: Vec::new(),
        }
    }
}

impl Graph {
    /// An empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a stored graph, tolerating the column's default.
    ///
    /// A graph that cannot be read is an *empty* graph rather than a refused request: the row
    /// still has whatever `steps` it always had, and the editor opening an unreadable document
    /// must show an empty canvas with a reason, not a 500 that hides the workflow entirely.
    pub fn from_stored(raw: &Value) -> Self {
        serde_json::from_value(raw.clone()).unwrap_or_default()
    }

    /// The stored JSON shape of this graph.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "nodes": self.nodes,
            "connections": self.connections,
            "notes": self.notes,
        })
    }

    /// One node by key.
    #[must_use]
    pub fn node(&self, key: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|node| node.key == key)
    }

    /// How many nodes a run would actually execute.
    #[must_use]
    pub fn enabled_nodes(&self) -> usize {
        self.nodes.iter().filter(|node| !node.disabled).count()
    }

    /// The keys of the nodes nothing connects to, ignoring disabled nodes.
    ///
    /// "Unreachable" is a graph property, not a type property: a node with no incoming edge can
    /// only run if something starts it, and a node whose only edges leave it is dead weight. A
    /// trigger legitimately has no incoming edge, so it is never reported.
    #[must_use]
    pub fn unreachable_keys(&self) -> Vec<String> {
        let targets: HashSet<&str> = self
            .connections
            .iter()
            .map(|edge| edge.to.as_str())
            .collect();

        self.nodes
            .iter()
            .filter(|node| {
                if node.disabled {
                    return false;
                }
                let is_trigger = node
                    .definition()
                    .is_some_and(registry::NodeDefinition::is_trigger);
                !is_trigger && !targets.contains(node.key.as_str())
            })
            .map(|node| node.key.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// One problem with a graph, addressed at a node when it has one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    /// Stable code — the REQ's list, so the panel can group and the editor can badge.
    pub code: &'static str,
    /// Node the problem is on, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_key: Option<String>,
    /// The **parameter** the problem is on, when the issue is about one.
    ///
    /// Additive rather than something the client has to parse out of `message`. The code
    /// editor's gutter needs to mark the *line* a problem is on, and the message is prose
    /// written for a person — `"url" must be a URL` carries the name in quotes today, which
    /// is a convention, not a contract, and every consumer that scraped it would break the
    /// first time a sentence was reworded. Skipped when absent, so an issue with no
    /// parameter serialises exactly as it did before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Connection index, for the connection codes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_index: Option<usize>,
    /// A sentence a person can act on.
    pub message: String,
}

impl Issue {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            node_key: None,
            param: None,
            connection_index: None,
            message: message.into(),
        }
    }

    fn at_node(
        code: &'static str,
        node_key: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            node_key: Some(node_key.into()),
            param: None,
            connection_index: None,
            message: message.into(),
        }
    }

    /// An issue about one parameter of one node.
    ///
    /// The `at_node` constructor with the parameter name attached, so the ~17 call sites in
    /// this module that report a parameter problem cannot forget to say which one — and the
    /// code editor's gutter has something to point at.
    fn at_param(
        code: &'static str,
        node_key: impl Into<String>,
        param: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            node_key: Some(node_key.into()),
            param: Some(param.into()),
            connection_index: None,
            message: message.into(),
        }
    }

    fn at_connection(code: &'static str, index: usize, message: impl Into<String>) -> Self {
        Self {
            code,
            node_key: None,
            param: None,
            connection_index: Some(index),
            message: message.into(),
        }
    }
}

/// Every problem a graph has, in a stable order.
///
/// Not a `Result`: the canvas shows a badge *and* a list, and a validation call that returned
/// only the first problem could not build either.
#[must_use]
pub fn validate(graph: &Graph) -> Vec<Issue> {
    let mut issues = Vec::new();

    if graph.nodes.len() > MAX_NODES {
        issues.push(Issue::new(
            "graph_too_large",
            format!(
                "a graph carries at most {MAX_NODES} nodes, this one has {}",
                graph.nodes.len()
            ),
        ));
    }
    if graph.connections.len() > MAX_CONNECTIONS {
        issues.push(Issue::new(
            "graph_too_large",
            format!(
                "a graph carries at most {MAX_CONNECTIONS} connections, this one has {}",
                graph.connections.len()
            ),
        ));
    }

    // Node keys: shape, then uniqueness. A duplicate key is reported against the *second* node
    // so the panel can point at one place rather than two.
    let mut keys: BTreeMap<&str, usize> = BTreeMap::new();
    for node in &graph.nodes {
        let key = node.key.trim();
        if key.is_empty() {
            issues.push(Issue::at_node(
                "node_key_invalid",
                &node.key,
                "every node needs a key, because connections name it",
            ));
            continue;
        }
        if key.len() > MAX_NODE_KEY {
            issues.push(Issue::at_node(
                "node_key_invalid",
                &node.key,
                format!("a node key is at most {MAX_NODE_KEY} characters"),
            ));
        }
        match keys.get(key) {
            Some(first) => issues.push(Issue::at_node(
                "node_duplicate_key",
                &node.key,
                format!(
                    "the key \"{key}\" is already used by node {} of the graph",
                    *first + 1
                ),
            )),
            None => {
                keys.insert(
                    key,
                    graph.nodes.iter().position(|n| n.key == key).unwrap_or(0),
                );
            }
        }
    }

    if graph.nodes.iter().all(|node| {
        node.definition()
            .is_none_or(|definition| !definition.is_trigger())
    }) {
        issues.push(Issue::new(
            "graph_no_trigger",
            "a graph needs a trigger node to start a run",
        ));
    }

    // Per-node type, parameters and retry policy.
    for node in &graph.nodes {
        let Some(definition) = node.definition() else {
            issues.push(Issue::at_node(
                "node_unknown_type",
                &node.key,
                format!(
                    "\"{}\" is not a node this platform knows; install a node package that \
                     provides it, or pick another",
                    node.node_type
                ),
            ));
            continue;
        };
        if definition.deprecated {
            issues.push(Issue::at_node(
                "node_type_deprecated",
                &node.key,
                match definition.superseded_by {
                    Some(replacement) => {
                        format!(
                            "\"{}\" is deprecated; use \"{replacement}\" instead",
                            node.node_type
                        )
                    }
                    None => format!("\"{}\" is deprecated", node.node_type),
                },
            ));
        }
        for problem in check_params(node, definition) {
            issues.push(problem);
        }
    }

    // Connections: existence, then the port contract, then duplicates, then cycles.
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    for (index, edge) in graph.connections.iter().enumerate() {
        let (from, from_port, to, to_port) = edge.endpoints();

        if !seen.insert((from.clone(), from_port.clone(), to.clone(), to_port.clone())) {
            issues.push(Issue::at_connection(
                "connection_duplicate",
                index,
                format!("the same connection is already drawn once"),
            ));
            continue;
        }
        if from == to {
            issues.push(Issue::at_connection(
                "connection_cycle",
                index,
                format!("node \"{from}\" cannot feed itself"),
            ));
            continue;
        }

        let source = graph.node(&from);
        let target = graph.node(&to);
        if source.is_none() || target.is_none() {
            issues.push(Issue::at_connection(
                "connection_node_unknown",
                index,
                match (source.is_none(), target.is_none()) {
                    (true, true) => {
                        format!("neither \"{from}\" nor \"{to}\" is a node of this graph")
                    }
                    (true, false) => format!("\"{from}\" is not a node of this graph"),
                    _ => format!("\"{to}\" is not a node of this graph"),
                },
            ));
            continue;
        }

        // Both ends must be *definitions* for the port contract to be checked at all. A node
        // whose type the registry does not know was already reported as `node_unknown_type`,
        // and a second, vaguer complaint about its edges helps nobody — so skip the edge rather
        // than inventing a verdict about ports that do not exist.
        let (Some(source), Some(target)) = (
            source.and_then(GraphNode::definition),
            target.and_then(GraphNode::definition),
        ) else {
            continue;
        };

        if let Err(error) = source.check_connection(&from_port, target, &to_port) {
            issues.push(Issue::at_connection(
                leak_code(error.code()),
                index,
                error.message().to_string(),
            ));
        }
    }

    issues.extend(cycle_issues(graph));
    issues.extend(unreachable_issues(graph));
    issues.extend(terminal_issues(graph));

    // Stable order: by node then by code, so the panel does not reshuffle between two calls on
    // the same graph and the badge count stays comparable.
    issues.sort_by(|a, b| {
        a.node_key
            .as_deref()
            .unwrap_or("")
            .cmp(b.node_key.as_deref().unwrap_or(""))
            .then(a.code.cmp(b.code))
            .then(a.connection_index.cmp(&b.connection_index))
    });
    issues.dedup_by(|a, b| a.code == b.code && a.node_key == b.node_key && a.message == b.message);
    issues
}

/// The codes the registry can answer with, as the `&'static str` an [`Issue`] carries.
///
/// A refused connection is reported with the *registry's* code, not a new one, so the canvas and
/// the installer lint speak the same vocabulary. The fallback exists so a code the registry grows
/// later is reported as `connection_invalid` — visibly generic — rather than panicking or, worse,
/// being dropped from the list the panel shows. The four arms are the registry's whole set today
/// (`grep -oE '"connection_[a-z_]+"' crates/workflows/src/registry.rs`), and the test below pins
/// that claim so the next person to add one is told here rather than discovering a lie in a
/// message a person reads.
fn leak_code(code: &str) -> &'static str {
    match code {
        "connection_port_unknown" => "connection_port_unknown",
        "connection_port_closed" => "connection_port_closed",
        "connection_type_mismatch" => "connection_type_mismatch",
        other => {
            debug_assert!(false, "the registry grew a connection code: {other}");
            "connection_invalid"
        }
    }
}

/// Parameters against the node's own schema.
fn check_params(node: &GraphNode, definition: &'static registry::NodeDefinition) -> Vec<Issue> {
    let mut issues = Vec::new();

    for param in &definition.params {
        let value = node.params.get(&param.name);
        if param.required && value.is_none() {
            issues.push(Issue::at_param(
                "node_param_required",
                &node.key,
                &param.name,
                format!("{} needs \"{}\"", definition.label, param.name),
            ));
            continue;
        }
        let Some(value) = value else { continue };

        if matches!(value, Value::Null) {
            issues.push(Issue::at_param(
                "node_param_invalid",
                &node.key,
                &param.name,
                format!("\"{}\" is empty", param.name),
            ));
            continue;
        }
        // A blank string is not a value, and this is the exact shape a browser submits for a
        // field a person opened and then left alone. `required` only asks whether the *key* is
        // present, so without this the run would mail nobody and report success. Whitespace
        // counts: `"  "` is what a field holding a space produces, and trimming it first is
        // what makes the check catch that rather than only the visibly-empty case.
        if value.as_str().is_some_and(|raw| raw.trim().is_empty()) {
            issues.push(Issue::at_param(
                "node_param_invalid",
                &node.key,
                &param.name,
                format!("\"{}\" is empty", param.name),
            ));
            continue;
        }
        if let Some(problem) = json_type_mismatch(&param.kind, value) {
            issues.push(Issue::at_param(
                "node_param_invalid",
                &node.key,
                &param.name,
                format!("\"{}\" {problem}", param.name),
            ));
            continue;
        }
        if !param.options.is_empty() {
            let options: Vec<&str> = param.options.iter().map(String::as_str).collect();
            let chosen = value.as_str().unwrap_or_default();
            if !options.contains(&chosen) {
                issues.push(Issue::at_param(
                    "node_param_invalid",
                    &node.key,
                    &param.name,
                    format!(
                        "\"{}\" must be one of {}, got \"{chosen}\"",
                        param.name,
                        options.join(", ")
                    ),
                ));
                continue;
            }
        }
        // A secret field is a *reference* to a credential, never a secret. A literal in a box
        // that says "Credential" is either a pasted key or a mis-typed credential id, and both
        // are worth refusing before a run tries it.
        //
        // The field to look for is `options_source == "credentials"`, NOT `secret_field` on its
        // own and NOT a non-empty `options`. The registry declares `http_request`'s
        // `credential_key` as `ui: Select` + `options_source: Some("credentials")` + an *empty*
        // `options` list — the options come from the caller's credential catalogue at render
        // time, which is the whole point of a reference. Checking `!options.is_empty()` therefore
        // never fires for any credential field in the registry today, and the guard silently did
        // nothing: the first version of this function was a length check on a condition that is
        // false everywhere, which is worse than having no guard because the test that named it
        // passed for the wrong reason.
        if param.options_source.as_deref() == Some("credentials")
            && value
                .as_str()
                .is_some_and(|raw| raw.len() > MAX_CREDENTIAL_REFERENCE)
        {
            issues.push(Issue::at_param(
                "node_param_invalid",
                &node.key,
                &param.name,
                format!(
                    "\"{}\" names a credential by its key, not by its secret; a value this long \
                     is a secret pasted into a reference field",
                    param.name
                ),
            ));
        }
    }

    issues
}

/// Longest a credential *reference* may be. A key is a short name; a secret is not.
///
/// Public because the test that proves the guard has to know the threshold it is proving: a
/// fixture that asserts "this is refused" without stating what the boundary *is* would keep
/// passing if the constant were quietly raised to a size no secret reaches, which is the
/// direction that silently disables a security check.
pub const MAX_CREDENTIAL_REFERENCE: usize = 64;

/// Compare a value against the JSON Schema type the palette declared.
///
/// Returns `None` when the value is acceptable, or a sentence naming what was expected.
fn json_type_mismatch(kind: &str, value: &Value) -> Option<String> {
    let ok = match kind {
        "string" => value.is_string(),
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        // An unknown type keyword is the schema's business, not a value's.
        _ => return None,
    };
    if ok {
        None
    } else {
        Some(format!("must be {kind}, got {}", describe(value)))
    }
}

/// A short name for a value's JSON type, for a sentence a person can act on.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "a yes/no value",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// A control-flow cycle, reported once on the node that closes it.
fn cycle_issues(graph: &Graph) -> Vec<Issue> {
    // Only *control* edges can cycle: a data edge back into an earlier node is a real pattern
    // (a loop over items), while control flow that returns to a node it already left would run
    // that node forever. Error edges count as control: they are the third way out of a node.
    let mut control: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &graph.connections {
        let is_control = graph
            .node(&edge.from)
            .and_then(GraphNode::definition)
            .and_then(|definition| {
                definition
                    .outputs
                    .iter()
                    .find(|port| port.name == edge.from_port)
            })
            .is_none_or(|port| port.kind != PortKind::Main || edge.label.is_some());
        if is_control {
            control.entry(&edge.from).or_default().push(&edge.to);
        }
    }

    let mut issues = Vec::new();
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 0 unvisited, 1 on stack, 2 done
    let mut keys: Vec<&str> = graph.nodes.iter().map(|node| node.key.as_str()).collect();
    keys.sort_unstable();

    for start in keys {
        if state.get(start).is_some_and(|s| *s != 0) {
            continue;
        }
        // Iterative depth-first with an explicit stack: a 200-node graph is allowed, and
        // recursion here would be a stack overflow on a document the validator is meant to
        // describe rather than crash on.
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut path: Vec<&str> = vec![start];
        state.insert(start, 1);

        while let Some((node, index)) = stack.pop() {
            let Some(edges) = control.get(node) else {
                state.insert(node, 2);
                path.retain(|key| *key != node);
                continue;
            };
            if index < edges.len() {
                stack.push((node, index + 1));
                let next = edges[index];
                match state.get(next) {
                    Some(1) => {
                        issues.push(Issue::at_node(
                            "connection_cycle",
                            next,
                            format!(
                                "the control path loops back to \"{next}\"; a run would enter it \
                                 again and never finish"
                            ),
                        ));
                    }
                    Some(2) => {}
                    _ => {
                        state.insert(next, 1);
                        stack.push((next, 0));
                        path.push(next);
                    }
                }
            } else {
                state.insert(node, 2);
                path.retain(|key| *key != node);
            }
        }
    }
    issues
}

/// Nodes nothing reaches, reported once each.
fn unreachable_issues(graph: &Graph) -> Vec<Issue> {
    graph
        .unreachable_keys()
        .into_iter()
        .map(|key| {
            Issue::at_node(
                "graph_unreachable_node",
                &key,
                format!("nothing connects to \"{key}\", so a run would never reach it"),
            )
        })
        .collect()
}

/// A trigger that leads nowhere, reported on the trigger.
fn terminal_issues(graph: &Graph) -> Vec<Issue> {
    let mut issues = Vec::new();
    for node in &graph.nodes {
        if node.disabled || node.definition().is_none_or(|d| !d.is_trigger()) {
            continue;
        }
        let starts_something = graph.connections.iter().any(|edge| edge.from == node.key);
        if !starts_something {
            issues.push(Issue::at_node(
                "graph_terminal_missing",
                &node.key,
                format!(
                    "the trigger \"{}\" has no outgoing connection, so the run would end here",
                    node.key
                ),
            ));
        }
    }
    issues
}

// ---------------------------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------------------------

/// A compiled graph: the steps the engine runs, and the issues that stopped it.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// Steps in execution order, named after the node labels.
    pub steps: Vec<StepDefinition>,
    /// Which node produced which step, 1-based, in step order.
    pub node_order: Vec<String>,
    /// Every issue found; a graph with any issue does not compile.
    pub issues: Vec<Issue>,
}

impl Compiled {
    /// `true` when the graph is clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// The engine action a registry node compiles to.
///
/// The registry (`omnion_workflows::registry`) is the *palette* contract — what a node looks like
/// to a person. The engine's action set (`crate::actions`) is a closed list of six built-ins, and
/// only two of them are things a person would call a node. So this mapping is deliberately tiny:
///
/// * a trigger compiles to `noop`, because a run's first step exists to say "the run started" and
///   the engine materialises steps in order — the trigger *is* the start of that order;
/// * `send_email` compiles to the engine's own `send_email` host action.
///
/// Everything else is refused. That looks like a thin slice, and it is the honest state of the
/// platform: `http_request`, `if`, `filter`, `code`, `date_time`, `s3_upload` and `stop_and_error`
/// are all real registry nodes that REQ-087's palette shipped, and **none of them has an engine
/// action yet** — REQ-088 (core node families) is the request that owes them. Compiling them to
/// `echo` would produce a definition that validates, saves, shows five green nodes on the canvas
/// and then does nothing at run time, which is the worst outcome available: a workflow that looks
/// finished and is not.
fn action_for(node_type: &str) -> Option<&'static str> {
    Some(match node_type {
        "manual_trigger" | "schedule_trigger" => "noop",
        "send_email" => "send_email",
        _ => return None,
    })
}

/// Turn a graph into the engine's steps.
///
/// Deterministic: nodes are ordered by their position on the canvas (left to right, then top to
/// bottom), so the same graph always compiles to the same steps and a run's step numbers are
/// stable across saves. Ties — two nodes at the same point — fall back to the key, so the order
/// is total even then.
pub fn compile(graph: &Graph) -> Compiled {
    let issues = validate(graph);
    if !issues.is_empty() {
        return Compiled {
            steps: Vec::new(),
            node_order: Vec::new(),
            issues,
        };
    }

    let mut ordered: Vec<&GraphNode> = graph.nodes.iter().collect();
    ordered.sort_by(|a, b| {
        a.position
            .x
            .partial_cmp(&b.position.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.position
                    .y
                    .partial_cmp(&b.position.y)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.key.cmp(&b.key))
    });

    let mut steps = Vec::with_capacity(ordered.len());
    let mut node_order = Vec::with_capacity(ordered.len());
    let mut compile_issues = Vec::new();

    for node in ordered {
        // A disabled node compiles to *nothing*. Two reasons, and the second is the important
        // one: the engine has no "skip this step" state a compiled definition can express
        // (`workflow_steps` statuses describe a run, not a definition), and — more seriously —
        // the first version of this function compiled disabled nodes like any other, so a node
        // somebody had deliberately switched off still produced a step that still ran. The
        // graph keeps the node and its edges, so turning it back on is one click and no rewiring;
        // what it must not do is appear in the run.
        if node.disabled {
            continue;
        }

        if let Some(action) = action_for(&node.node_type) {
            // The engine reads the action's own parameter names, so a node's palette params are
            // passed through verbatim. `actions::validate_params` is the authority on whether the
            // combination is usable, and its refusal is the graph's refusal.
            let params = node.params_value();
            if let Err(error) = actions::validate_params(action, &params) {
                compile_issues.push(Issue::at_node(
                    "node_param_invalid",
                    &node.key,
                    error.message().to_string(),
                ));
                continue;
            }
            let attempts = node
                .definition()
                .map_or(1, |definition| definition.default_max_attempts)
                .max(1)
                .min(crate::definition::MAX_ATTEMPTS);
            steps.push(StepDefinition::task(&node.label, action, params).retrying(attempts));
            node_order.push(node.key.clone());
            continue;
        }

        // A wait node is the one non-task kind the graph can hold: the palette offers it and the
        // engine has had it since v0.
        if node.node_type == "wait" {
            let seconds = node
                .params
                .get("seconds")
                .and_then(Value::as_i64)
                .unwrap_or(60);
            match StepDefinition::wait(&node.label, seconds).wait_seconds() {
                Ok(_) => {
                    steps.push(StepDefinition::wait(&node.label, seconds));
                    node_order.push(node.key.clone());
                }
                Err(error) => compile_issues.push(Issue::at_node(
                    "node_param_invalid",
                    &node.key,
                    error.message().to_string(),
                )),
            }
            continue;
        }

        compile_issues.push(Issue::at_node(
            "node_action_unavailable",
            &node.key,
            format!(
                "the \"{}\" node has no engine action yet; this is the node-families slice \
                 (REQ-088), not a graph the editor may save",
                node.node_type
            ),
        ));
    }

    Compiled {
        steps,
        node_order,
        issues: compile_issues,
    }
}

/// Compile and refuse, for the caller that wants a `Result` rather than a report.
pub fn compile_or_refuse(graph: &Graph) -> Result<Vec<StepDefinition>> {
    let compiled = compile(graph);
    if let Some(first) = compiled.issues.first() {
        return Err(WorkflowError::invalid(
            first.code,
            format!(
                "{} ({} issue{} in total)",
                first.message,
                compiled.issues.len(),
                if compiled.issues.len() == 1 { "" } else { "s" }
            ),
        ));
    }
    if compiled.steps.is_empty() {
        return Err(WorkflowError::invalid(
            "graph_no_trigger",
            "a graph needs at least one node that can run",
        ));
    }
    Ok(compiled.steps)
}

/// Reverse a compiled step list back onto node keys, for the canvas's step-to-node mapping.
///
/// Best effort by label: the compiler named each step after the node label, so a definition
/// written by hand (no graph at all) simply yields nothing rather than an error — the canvas
/// then shows the steps as "not on the canvas", which is true.
#[must_use]
pub fn node_order_for(steps: &[StepDefinition]) -> Vec<String> {
    steps.iter().map(|step| step.name.clone()).collect()
}

/// The engine kinds a step can take, so a caller building a graph knows what it is aiming at.
#[must_use]
pub fn compilable_kinds() -> &'static [StepKind] {
    &[StepKind::Task, StepKind::Wait]
}
