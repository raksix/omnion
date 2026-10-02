//! The graph tests, in one place so the file that decides what a workflow may run also decides
//! what the editor may draw.
//!
//! This is an *integration* test (it lives in `tests/`, beside the crate rather than inside it),
//! so everything is named through the crate's public re-exports — which is also the reason it is
//! worth having: it proves the editor's surface is reachable from outside the engine, not only
//! from the module that defines it.
//!
//! Two shapes of test live here, and the difference matters:
//!
//! * **Validation** tests drive any registry node (`http_request`, `if`, `stop_and_error`) because
//!   `validate` only needs the palette contract. They assert a refusal the REQ names.
//! * **Compilation** tests use only nodes that have an *engine action* — a trigger and
//!   `send_email` — because that is the whole set today. `http_request` and friends are real
//!   registry nodes that REQ-088 owes an action for, and a test that compiled one would be
//!   asserting a promise nobody has made yet.
//!
//! Every test here asserts a *refusal or a round trip*, never "it compiled": a test that only
//! proved the happy path would pass on a validator that accepts everything and produces steps
//! nobody can run.

use omnion_workflows::{
    Compiled, Connection, Graph, GraphNode, Issue, StickyNote, Trigger, WorkflowDefinition,
    compile, compile_or_refuse, validate,
};
use serde_json::json;

/// The smallest graph that compiles *and runs*: a trigger and one mail.
//
// Both nodes have an engine action, which is why this and not `trigger → http_request` is the
// fixture the slice-1 acceptance criterion ("a two-node graph runs end to end through the
/// existing engine") is proved against.
fn simple() -> Graph {
    Graph {
        nodes: vec![
            GraphNode::new("start", "manual_trigger", 0.0, 0.0),
            mail("notify", 200.0, 0.0),
        ],
        connections: vec![Connection::new("start", "out", "notify", "in")],
        notes: Vec::new(),
    }
}

/// `send_email` with the four parameters its schema requires.
fn mail(key: &str, x: f64, y: f64) -> GraphNode {
    GraphNode::new(key, "send_email", x, y)
        .with_param("to", json!("ops@example.com"))
        .with_param("subject", json!("Orders"))
        .with_param("body", json!("there are some"))
        .with_param("credential_key", json!("smtp_prod"))
}

/// The `http_request` node with the params the registry requires. Validation only — it has no engine
/// action yet.
fn http(key: &str, x: f64) -> GraphNode {
    GraphNode::new(key, "http_request", x, 0.0)
        .with_param("url", json!("https://api.example.com/v1/orders"))
        .with_param("method", json!("GET"))
}

/// A credential reference that is far too long to be a reference.
///
/// The prefix is deliberately not any provider's key format. GitHub's push protection scans
/// commits for credential *shapes*, not for real credentials, so a realistic-looking fixture
/// blocks the whole branch with GH013 and the only way out is a dashboard approval nobody is
/// watching. A prefix that cannot be a real key keeps the test honest — the guard under test is
/// the length check, and the value's provenance is irrelevant to it.
const PREFIX: &str = "not-a-real-credential-reference:";

/// `if` with the condition it requires.
fn branch(key: &str, x: f64) -> GraphNode {
    GraphNode::new(key, "if", x, 0.0).with_param("condition", json!("{{ total > 10 }}"))
}

/// `stop_and_error` with its message.
fn failer(key: &str, x: f64) -> GraphNode {
    GraphNode::new(key, "stop_and_error", x, 0.0).with_param("message", json!("no orders today"))
}

/// Every code a graph carries, in report order.
fn codes(graph: &Graph) -> Vec<&'static str> {
    validate(graph)
        .into_iter()
        .map(|issue| issue.code)
        .collect()
}

/// The issues reported against one node.
fn for_node<'a>(graph: &'a Graph, key: &str) -> Vec<&'a str> {
    validate(graph)
        .into_iter()
        .filter(|issue| issue.node_key.as_deref() == Some(key))
        .map(|issue| issue.code)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The document and the compiler
// ---------------------------------------------------------------------------------------------

#[test]
fn a_two_node_graph_validates_compiles_and_is_a_runnable_definition() {
    let graph = simple();
    let issues = validate(&graph);
    assert!(
        issues.is_empty(),
        "a trigger wired to a valid send has no issues, got {issues:?}"
    );

    let compiled = compile(&graph);
    assert!(compiled.is_clean(), "{:?}", compiled.issues);
    assert_eq!(compiled.node_order, vec!["start", "notify"]);

    // The steps are the engine's own type, so the existing engine runs them with no change —
    // that is the whole claim of slice 1.
    let definition = WorkflowDefinition::new(Trigger::manual(), compiled.steps.clone())
        .expect("a compiled graph is a valid workflow definition");
    assert_eq!(definition.steps.len(), 2);
    assert_eq!(definition.steps[0].name, "start");
    assert_eq!(definition.steps[0].action.as_deref(), Some("noop"));
    assert_eq!(definition.steps[1].name, "notify");
    assert_eq!(definition.steps[1].action.as_deref(), Some("send_email"));
    assert_eq!(definition.steps[1].params["to"], json!("ops@example.com"));
}

#[test]
fn the_same_graph_always_compiles_to_the_same_steps() {
    let mut graph = simple();
    graph
        .notes
        .push(StickyNote::new("n1", "remember to add a retry", 10.0, 10.0));
    graph.nodes.push(mail("second", 400.0, 0.0));
    graph
        .connections
        .push(Connection::new("notify", "out", "second", "in"));

    let first = compile(&graph);
    // Re-serialising and reading back is what a save/reload does; the document must survive the
    // round trip field for field or the canvas shifts nodes every time it opens.
    let round_tripped = Graph::from_stored(&graph.to_value());
    let second = compile(&round_tripped);

    assert_eq!(
        first.steps, second.steps,
        "compilation is not deterministic"
    );
    assert_eq!(first.node_order, second.node_order);
    assert_eq!(
        graph, round_tripped,
        "the document does not survive a round trip"
    );
}

#[test]
fn compilation_orders_by_canvas_position_not_by_declaration_order() {
    let mut graph = simple();
    // The mail node is declared first but sits to the RIGHT of the trigger, so the trigger's
    // step must still be step 1 — otherwise a run starts with the thing the trigger feeds.
    graph.nodes.reverse();

    let compiled = compile(&graph);
    assert_eq!(compiled.node_order, vec!["start", "notify"]);
    assert_eq!(compiled.steps[0].name, "start");
}

#[test]
fn a_graph_with_no_nodes_is_empty_rather_than_broken() {
    let stored = Graph::from_stored(&json!({}));
    assert!(stored.nodes.is_empty());
    // A row whose graph cannot be read must still open as a blank canvas, not as a 500 that
    // hides a workflow which runs perfectly well from its steps.
    let garbage = Graph::from_stored(&json!("not a graph at all"));
    assert!(garbage.nodes.is_empty());
    assert!(garbage.connections.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Node-level refusals
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unknown_node_type_is_named_with_what_to_do_about_it() {
    let mut graph = simple();
    graph
        .nodes
        .push(GraphNode::new("mystery", "acme.uninstalled", 400.0, 0.0));
    graph
        .connections
        .push(Connection::new("notify", "out", "mystery", "in"));

    let issues = validate(&graph);
    let unknown = issues
        .iter()
        .find(|issue| issue.code == "node_unknown_type")
        .expect("an unknown type is reported");
    assert_eq!(unknown.node_key.as_deref(), Some("mystery"));
    assert!(
        unknown.message.contains("node package"),
        "the message must say what fixes it: {}",
        unknown.message
    );
}

#[test]
fn two_nodes_may_not_share_a_key() {
    let mut graph = simple();
    graph.nodes.push(mail("notify", 400.0, 0.0));
    let issues = validate(&graph);
    assert!(
        issues.iter().any(|i| i.code == "node_duplicate_key"),
        "{issues:?}"
    );
    // Reported on the *second* node, so the panel can point at one place.
    let duplicate = issues
        .iter()
        .find(|i| i.code == "node_duplicate_key")
        .expect("one duplicate");
    assert_eq!(duplicate.node_key.as_deref(), Some("notify"));
}

#[test]
fn a_missing_or_wrongly_typed_parameter_is_refused_with_its_name() {
    let mut graph = simple();
    // The url is required, and the method is a closed set.
    graph.nodes[1] = GraphNode::new("notify", "http_request", 200.0, 0.0)
        .with_param("method", json!("TELEPORT"));
    let found = for_node(&graph, "notify");
    assert!(found.contains(&"node_param_required"), "{found:?}");
    assert!(found.contains(&"node_param_invalid"), "{found:?}");

    graph.nodes[1] = http("notify", 200.0).with_param("url", json!(42));
    let issues = validate(&graph);
    let bad = issues
        .iter()
        .find(|issue| issue.code == "node_param_invalid")
        .expect("a number where a URL belongs is refused");
    assert!(bad.message.contains("url"), "{}", bad.message);
    assert!(bad.message.contains("a number"), "{}", bad.message);
}

#[test]
fn a_blank_parameter_is_not_a_value() {
    let mut graph = simple();
    // The shape a browser submits for a field a person opened and then left alone. `required`
    // only asks whether the *key* is present, so an empty string would otherwise sail through
    // and the run would mail nobody.
    graph.nodes[1] = mail("notify", 200.0, 0.0).with_param("to", json!(""));
    let issues = validate(&graph);
    let bad = issues
        .iter()
        .find(|issue| issue.code == "node_param_invalid")
        .expect("an empty required field is refused");
    assert!(bad.message.contains("empty"), "{}", bad.message);
}

#[test]
fn a_pasted_secret_in_a_credential_field_is_refused() {
    let mut graph = simple();
    // A credential reference is a key like `smtp_prod`; a value this long is a secret somebody
    // pasted into a field that says "Credential", and it would sit in `workflows.graph` in plain
    // text for every future run to read.
    //
    // The literal is built from a prefix that no provider's key format starts with. An earlier
    // version wrote a realistic `sk_live_…` string here, which is exactly the shape a reviewer
    // wants and exactly the shape GitHub's push protection blocks: the secret scanner cannot
    // tell a fixture from a leak, so the whole branch became unpushable. A test that cannot be
    // pushed has proved nothing about the product.
    let pasted = format!("{PREFIX}{}", "A".repeat(80));
    assert!(
        pasted.len() > omnion_workflows::graph::MAX_CREDENTIAL_REFERENCE,
        "the fixture must be long enough to trip the guard"
    );
    graph.nodes[1] = mail("notify", 200.0, 0.0).with_param("credential_key", json!(pasted));

    let issues = validate(&graph);
    let bad = issues
        .iter()
        .find(|issue| issue.code == "node_param_invalid")
        .expect("a secret in a reference field is refused");
    assert!(
        bad.message.contains("not by its secret"),
        "the message must say what is wrong: {}",
        bad.message
    );
}

#[test]
fn a_graph_needs_a_trigger() {
    let mut graph = simple();
    graph.nodes[0] = mail("start", 0.0, 0.0);
    assert!(codes(&graph).contains(&"graph_no_trigger"));
}

#[test]
fn a_trigger_that_leads_nowhere_is_reported_on_the_trigger() {
    let mut graph = simple();
    graph.connections.clear();
    graph.nodes.truncate(1);
    let issues = validate(&graph);
    let terminal = issues
        .iter()
        .find(|issue| issue.code == "graph_terminal_missing")
        .expect("a trigger with no outgoing edge ends the run there");
    assert_eq!(terminal.node_key.as_deref(), Some("start"));
}

#[test]
fn a_node_nothing_connects_to_is_reported_as_unreachable() {
    let mut graph = simple();
    graph.nodes.push(http("orphan", 400.0));
    let issues = validate(&graph);
    let orphan = issues
        .iter()
        .find(|issue| issue.code == "graph_unreachable_node")
        .expect("an orphaned node is reported");
    assert_eq!(orphan.node_key.as_deref(), Some("orphan"));
}

#[test]
fn a_disabled_node_is_kept_out_of_the_run_and_out_of_the_complaints() {
    let mut graph = simple();
    let mut disabled = mail("spare", 400.0, 0.0);
    disabled.disabled = true;
    graph.nodes.push(disabled);

    // A node somebody turned off is not a defect: it is a decision. The graph keeps it so
    // turning it back on does not mean rewiring.
    assert!(
        validate(&graph).is_empty(),
        "a disabled node is not an issue: {:?}",
        validate(&graph)
    );
    assert_eq!(graph.enabled_nodes(), 2);

    // …and it must not appear in the run. The first version of the compiler emitted a step for
    // disabled nodes like any other, so a node a person had deliberately switched off still ran.
    let compiled = compile(&graph);
    assert!(compiled.is_clean(), "{:?}", compiled.issues);
    assert_eq!(compiled.node_order, vec!["start", "notify"]);
    assert!(
        !compiled.node_order.iter().any(|key| key == "spare"),
        "a disabled node compiled into a step: {:?}",
        compiled.node_order
    );
}

// ---------------------------------------------------------------------------------------------
// Connection refusals
// ---------------------------------------------------------------------------------------------

#[test]
fn an_error_output_cannot_feed_a_data_input() {
    let mut graph = simple();
    graph.nodes.push(failer("boom", 400.0));
    // `notify.out` is a main data port and `boom.in` is a main input, so that edge connects.
    graph
        .connections
        .push(Connection::new("notify", "out", "boom", "in"));
    assert!(
        validate(&graph).is_empty(),
        "a data edge into a main input is legal: {:?}",
        validate(&graph)
    );

    // `boom.failed` is an *error* port and `notify.in` is a main data input. Routing a failure
    // into the happy path is exactly the mistake a type-aware canvas exists to prevent — and the
    // registry is what refuses it, so the canvas and the installer lint agree on the reason.
    graph
        .connections
        .push(Connection::new("boom", "failed", "notify", "in"));
    let issues = validate(&graph);
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "connection_type_mismatch"),
        "an error port into a data input is refused: {issues:?}"
    );
}

#[test]
fn a_self_loop_and_a_control_cycle_are_both_refused() {
    let mut graph = simple();
    graph.nodes.push(branch("decide", 400.0));
    graph
        .connections
        .push(Connection::new("notify", "out", "decide", "in"));

    // A node feeding itself.
    graph
        .connections
        .push(Connection::new("decide", "true", "decide", "in"));
    assert!(codes(&graph).contains(&"connection_cycle"), "self loop");

    // And a two-node control loop between two ordinary nodes — the shape a person draws by
    // accident. Both edges are legal on their own; only the loop is not.
    graph.connections.clear();
    graph.nodes.push(branch("decide_2", 600.0));
    graph
        .connections
        .push(Connection::new("start", "out", "decide", "in"));
    graph
        .connections
        .push(Connection::new("decide", "true", "decide_2", "in").labelled("true"));
    graph
        .connections
        .push(Connection::new("decide_2", "true", "decide", "in").labelled("true"));
    let found = codes(&graph);
    assert!(
        found.contains(&"connection_cycle"),
        "control cycle: {found:?}"
    );
}

#[test]
fn a_data_loop_is_allowed_because_only_control_flow_may_not_loop() {
    let mut graph = simple();
    graph.nodes.push(http("page", 600.0));
    graph
        .connections
        .push(Connection::new("notify", "out", "page", "in"));
    // A data edge back into an earlier node is a real pattern (paging through a result set). Only
    // *control* flow — a labelled branch or an error port — would send a run round again.
    graph
        .connections
        .push(Connection::new("page", "out", "notify", "in"));

    let found = codes(&graph);
    assert!(
        !found.contains(&"connection_cycle"),
        "a data loop is not a control cycle: {found:?}"
    );
}

#[test]
fn a_drawing_the_same_edge_twice_is_refused_once() {
    let mut graph = simple();
    graph
        .connections
        .push(Connection::new("start", "out", "notify", "in"));
    let issues = validate(&graph);
    let duplicates: Vec<_> = issues
        .iter()
        .filter(|issue| issue.code == "connection_duplicate")
        .collect();
    assert_eq!(duplicates.len(), 1, "{issues:?}");
    assert_eq!(duplicates[0].connection_index, Some(1));
}

#[test]
fn an_edge_to_a_node_that_is_not_there_is_named() {
    let mut graph = simple();
    graph
        .connections
        .push(Connection::new("notify", "out", "ghost", "in"));
    let issues = validate(&graph);
    let unknown = issues
        .iter()
        .find(|issue| issue.code == "connection_node_unknown")
        .expect("an edge to a missing node is reported");
    assert!(unknown.message.contains("ghost"), "{}", unknown.message);
}

#[test]
fn the_connection_codes_the_canvas_badges_are_the_registry_s_own() {
    // The `leak_code` mapping claims to be the registry's whole vocabulary. This test is what
    // makes the claim true: it drives two of the three refusals the registry can answer a
    // connection with, and fails if the canvas reports a code of its own invention instead.
    //
    // (`connection_port_closed` is the third, and no bundled node ships a closed port, so no graph
    // a person can draw reaches it. Its arm is in the match because the registry has the code —
    // and the `debug_assert` in `leak_code` is what will tell the next person who adds a code to
    // the registry that they must come back here.)
    let mut graph = Graph {
        nodes: vec![
            GraphNode::new("start", "manual_trigger", 0.0, 0.0),
            mail("notify", 200.0, 0.0),
            failer("boom", 400.0),
        ],
        connections: vec![
            Connection::new("start", "out", "notify", "in"),
            // An error port into a main input: the registry's `connection_type_mismatch`.
            Connection::new("boom", "failed", "notify", "in"),
            // A port that does not exist: the registry's `connection_port_unknown`.
            Connection::new("notify", "nope", "boom", "in"),
            Connection::new("notify", "out", "boom", "in"),
        ],
        notes: Vec::new(),
    };

    let found = codes(&graph);
    assert!(found.contains(&"connection_type_mismatch"), "{found:?}");
    assert!(found.contains(&"connection_port_unknown"), "{found:?}");
}

// ---------------------------------------------------------------------------------------------
// Sticky notes and branch labels
// ---------------------------------------------------------------------------------------------

#[test]
fn a_sticky_note_persists_and_never_compiles() {
    let mut graph = simple();
    graph.notes.push(StickyNote::new(
        "n1",
        "ask about the rate limit",
        40.0,
        60.0,
    ));
    let before = compile(&graph);

    let round_tripped = Graph::from_stored(&graph.to_value());
    assert_eq!(round_tripped.notes.len(), 1);
    assert_eq!(round_tripped.notes[0].text, "ask about the rate limit");
    assert_eq!(round_tripped.notes[0].color, "amber", "the default colour");
    assert_eq!(round_tripped.notes[0].width, 240.0);

    // Two steps before, two steps after: a note is a comment, not a node.
    assert_eq!(before.steps.len(), 2);
    assert_eq!(compile(&round_tripped).steps, before.steps);
}

#[test]
fn a_branch_label_survives_the_document() {
    let mut graph = simple();
    graph.nodes.push(branch("decide", 400.0));
    graph.nodes.push(mail("yes_mail", 600.0, -80.0));
    graph.nodes.push(
        GraphNode::new("no_note", "http_request", 600.0, 80.0)
            .with_param("url", json!("https://x.test"))
            .with_param("method", json!("GET")),
    );
    graph
        .connections
        .push(Connection::new("notify", "out", "decide", "in"));
    graph
        .connections
        .push(Connection::new("decide", "true", "yes_mail", "in").labelled("true"));
    graph
        .connections
        .push(Connection::new("decide", "false", "no_note", "in").labelled("false"));

    let round_tripped = Graph::from_stored(&graph.to_value());
    let labels: Vec<_> = round_tripped
        .connections
        .iter()
        .filter_map(|edge| edge.label.clone())
        .collect();
    assert_eq!(
        labels,
        vec!["true", "false"],
        "branch labels survive a save"
    );

    assert!(
        validate(&round_tripped).is_empty(),
        "{:?}",
        validate(&round_tripped)
    );
}

// ---------------------------------------------------------------------------------------------
// What a save refuses
// ---------------------------------------------------------------------------------------------

#[test]
fn a_graph_with_issues_does_not_compile_and_says_which_one_first() {
    let mut graph = simple();
    graph.nodes.push(http("orphan", 400.0));
    let error = compile_or_refuse(&graph).expect_err("an unreachable node is refused");
    assert!(error.message().contains("orphan"), "{error}");

    let compiled = compile(&graph);
    assert!(
        compiled.steps.is_empty(),
        "a refused graph yields no steps at all"
    );
    assert!(!compiled.is_clean());
}

#[test]
fn a_registry_node_without_an_engine_action_is_refused_with_the_request_that_owes_it() {
    let mut graph = simple();
    // `http_request` is a real registry node — REQ-087's palette shipped it and the bundled lint
    // passes it — but the engine's closed action set has no HTTP call. Saving it would produce a
    // definition that validates, saves, shows a green node on the canvas and then does nothing
    // at run time, so the compiler names the request that owes the action instead.
    graph.nodes.push(http("fetch", 400.0));
    graph
        .connections
        .push(Connection::new("notify", "out", "fetch", "in"));

    // The palette contract is satisfied: this is a *compilation* refusal, not a validation one.
    assert!(
        !codes(&graph).contains(&"node_action_unavailable"),
        "an unavailable action is found at compile time: {:?}",
        validate(&graph)
    );

    let compiled: Compiled = compile(&graph);
    let blocked = compiled
        .issues
        .iter()
        .find(|issue| issue.code == "node_action_unavailable")
        .expect("the node is refused at compile time");
    assert!(blocked.message.contains("REQ-088"), "{}", blocked.message);
    assert!(
        !compiled.node_order.iter().any(|key| key == "fetch"),
        "a refused node must not appear in the step list: {:?}",
        compiled.node_order
    );
}
