//! Plugin node types: registration, namespacing and the three states of resolution
//! (REQ-004: "A plugin node appears in the palette with its badge when the plugin is enabled
//! and disappears when it is disabled; a definition using it then reports an honest
//! validation error instead of failing at run time").

use serde_json::json;

use crate::graph::{self, Finding, Graph};
use crate::plugin_nodes::{
    is_legal_key, is_reserved_key, namespaced_key, PluginNodeRejection, PluginNodeType,
    PluginParamField, PluginPort, PluginProvider, PluginRegistry, PLUGIN_CATEGORY, Resolution,
};

fn provider() -> PluginProvider {
    PluginProvider {
        plugin: "mailer".to_owned(),
        name: "Mailer".to_owned(),
    }
}

fn port(key: &str) -> PluginPort {
    PluginPort {
        key: key.to_owned(),
        label: key.to_owned(),
        help: None,
        terminal: false,
    }
}

fn field(key: &str, required: bool) -> PluginParamField {
    PluginParamField {
        key: key.to_owned(),
        label: key.to_owned(),
        kind: "text".to_owned(),
        required,
        options: Vec::new(),
        help: String::new(),
    }
}

fn send_node() -> PluginNodeType {
    PluginNodeType {
        node: "send".to_owned(),
        label: "Send mail".to_owned(),
        summary: "Sends one message through the provider.".to_owned(),
        outputs: vec![port("out"), port("failed")],
        params: vec![field("to", true)],
    }
}

fn registry_with_mailer() -> PluginRegistry {
    let mut registry = PluginRegistry::empty();
    registry
        .register(&provider(), &[send_node()])
        .expect("a well-formed manifest registers");
    registry
}

// ---- namespacing: the security property ------------------------------------------------------

#[test]
fn a_plugin_node_is_stored_under_a_namespaced_key() {
    let registry = registry_with_mailer();
    let node = &registry.nodes()[0];
    assert_eq!(node.key, "plugin.mailer.send");
    assert_eq!(node.key, namespaced_key("mailer", "send"));
    assert_eq!(node.category, PLUGIN_CATEGORY);
}

#[test]
fn a_plugin_cannot_spell_a_core_type_because_namespacing_rewrites_its_key() {
    // The manifest says `node: "action"`, and what lands in the graph is NOT `action`.
    let mut registry = PluginRegistry::empty();
    registry
        .register(
            &provider(),
            &[PluginNodeType {
                node: "action".to_owned(),
                ..send_node()
            }],
        )
        .expect("registers");
    let stored = &registry.nodes()[0].key;
    assert_eq!(stored, "plugin.mailer.action");
    assert!(
        !is_reserved_key(stored),
        "a plugin that installs a core key would take over every rule using one"
    );
    assert!(is_reserved_key("action"), "the core key itself is still reserved");
    assert_eq!(
        registry.resolve("action"),
        Resolution::Core,
        "and it still resolves to the CORE node, not the plugin's"
    );
    assert!(matches!(
        registry.resolve("plugin.mailer.action"),
        Resolution::Plugin(_)
    ));
}

#[test]
fn a_manifest_with_an_illegal_key_is_refused_by_name() {
    let mut registry = PluginRegistry::empty();
    let answer = registry.register(
        &PluginProvider {
            plugin: "Mailer Co".to_owned(),
            name: "Mailer".to_owned(),
        },
        &[send_node()],
    );
    assert_eq!(
        answer,
        Err(PluginNodeRejection::BadPluginKey {
            plugin: "Mailer Co".to_owned()
        })
    );
    assert!(registry.is_empty(), "a refused provider contributes nothing");
}

#[test]
fn keys_are_lower_case_by_construction() {
    for key in ["send", "send_v2", "http.call", "a1"] {
        assert!(is_legal_key(key), "{key} should be legal");
    }
    for key in ["", "Send", "send mail", "send/mail", "send-mail", "plugin:send"] {
        assert!(!is_legal_key(key), "{key:?} must be refused — it breaks a data-node-type selector");
    }
}

// ---- registration is all-or-nothing ---------------------------------------------------------

#[test]
fn a_manifest_is_refused_whole_and_the_registry_is_untouched() {
    // A half-installed plugin gives an admin a palette with some of its nodes and no way to
    // see which — and a rule using the missing half fails validation looking like a bug.
    let mut registry = registry_with_mailer();
    let before = registry.nodes().len();
    let answer = registry.register(
        &PluginProvider {
            plugin: "broken".to_owned(),
            name: "Broken".to_owned(),
        },
        &[
            send_node(),
            PluginNodeType {
                node: "second".to_owned(),
                outputs: vec![],
                ..send_node()
            },
        ],
    );
    assert_eq!(
        answer,
        Err(PluginNodeRejection::NoOutputs {
            node: "second".to_owned()
        }),
        "the second node has no port, so the first must not land either"
    );
    assert_eq!(registry.nodes().len(), before, "nothing from a refused manifest registers");
    assert_eq!(registry.providers().len(), 1);
}

#[test]
fn every_refusal_names_the_declaration_that_caused_it() {
    let cases: Vec<(PluginNodeType, PluginNodeRejection)> = vec![
        (
            PluginNodeType {
                node: String::new(),
                ..send_node()
            },
            PluginNodeRejection::BadNodeKey {
                node: String::new(),
            },
        ),
        (
            PluginNodeType {
                label: "   ".to_owned(),
                ..send_node()
            },
            PluginNodeRejection::MissingLabel {
                node: "send".to_owned(),
            },
        ),
        (
            PluginNodeType {
                outputs: vec![],
                ..send_node()
            },
            PluginNodeRejection::NoOutputs {
                node: "send".to_owned(),
            },
        ),
        (
            PluginNodeType {
                params: vec![PluginParamField {
                    key: "Bad Key".to_owned(),
                    ..field("to", true)
                }],
                ..send_node()
            },
            PluginNodeRejection::BadParamField {
                node: "send".to_owned(),
            },
        ),
    ];
    for (declared, expected) in cases {
        let mut registry = PluginRegistry::empty();
        let answer = registry.register(&provider(), &[declared]);
        assert_eq!(answer, Err(expected), "an admin must be told which declaration is wrong");
    }
}

#[test]
fn two_declarations_with_one_key_collide_and_are_refused() {
    let mut registry = PluginRegistry::empty();
    let answer = registry.register(&provider(), &[send_node(), send_node()]);
    assert_eq!(
        answer,
        Err(PluginNodeRejection::DuplicateNode {
            node: "send".to_owned()
        })
    );
}

#[test]
fn a_provider_with_no_name_would_badge_nothing_and_is_refused() {
    let mut registry = PluginRegistry::empty();
    let answer = registry.register(
        &PluginProvider {
            plugin: "mailer".to_owned(),
            name: "  ".to_owned(),
        },
        &[send_node()],
    );
    assert!(matches!(answer, Err(PluginNodeRejection::MissingProvider { .. })));
}

// ---- the badge ------------------------------------------------------------------------------

#[test]
fn a_plugin_node_carries_the_badge_the_criterion_names() {
    let registry = registry_with_mailer();
    let node = &registry.nodes()[0];
    assert_eq!(node.badge, "Plugin: Mailer");
    assert_eq!(node.provider.plugin, "mailer");
    assert_eq!(node.provider.name, "Mailer");
    assert_eq!(node.outputs.len(), 2, "the declared ports are carried through");
    assert_eq!(node.outputs[0].key, "out");
}

#[test]
fn an_empty_registry_draws_no_group_at_all() {
    // A rail section titled "Plugins" with nothing in it is indistinguishable from a plugin
    // that failed to load.
    let registry = PluginRegistry::empty();
    assert!(registry.is_empty());
    assert!(registry.nodes().is_empty());
    assert!(registry.providers().is_empty());
    assert_eq!(registry.resolve("plugin.mailer.send"), Resolution::Unknown);
}

// ---- the three states of resolution ---------------------------------------------------------

#[test]
fn a_disabled_plugin_makes_its_node_unknown_and_says_so_in_words() {
    // This is the criterion's third clause, and the whole of it: the key stops resolving, the
    // finding names the plugin, and the instruction is "re-enable it" rather than "pick a
    // legal node type" — because the rule was legal an hour ago and the author did nothing.
    let graph = graph_with_plugin_node();
    let enabled = graph::validate_with_plugins(&graph, &registry_with_mailer());
    assert!(
        !enabled.iter().any(|f| f.code == "unknown_node_type"),
        "an enabled plugin's node must validate: {enabled:?}"
    );

    // The plugin is disabled: the registry is now empty.
    let disabled = graph::validate_with_plugins(&graph, &PluginRegistry::empty());
    let finding = disabled
        .iter()
        .find(|f| f.code == "unknown_node_type")
        .expect("a node whose plugin is gone must be reported, not run");
    assert!(
        finding.message.contains("plugin"),
        "the message must name the cause: {}",
        finding.message
    );
    assert!(
        finding.message.contains("re-enable"),
        "and give the instruction: {}",
        finding.message
    );
    assert!(!finding.message.contains("not a node type the platform knows"),
            "telling an author their working rule is nonsense is how the panel gets ignored: {}",
            finding.message);
}

#[test]
fn a_typo_still_gets_the_typo_message() {
    // The two sentences must stay distinguishable, or the plugin case stops being information.
    let mut graph = graph_with_plugin_node();
    graph.nodes[1].node_type = "conditon".to_owned();
    let findings = graph::validate_with_plugins(&graph, &PluginRegistry::empty());
    let finding = findings
        .iter()
        .find(|f| f.code == "unknown_node_type")
        .expect("a typo is still an unknown type");
    assert!(finding.message.contains("not a node type the platform knows"), "{}", finding.message);
    assert!(!finding.message.contains("re-enable"));
}

#[test]
fn a_plugin_nodes_port_list_is_what_validates_its_edges() {
    let mut graph = graph_with_plugin_node();
    graph.edges.push(crate::graph::Edge {
        id: "e-bad".to_owned(),
        source: "b".to_owned(),
        source_port: "nowhere".to_owned(),
        target: "c".to_owned(),
    });
    let findings = graph::validate_with_plugins(&graph, &registry_with_mailer());
    let finding = findings
        .iter()
        .find(|f| f.code == "unknown_source_port")
        .expect("a port the plugin never declared must be refused");
    assert!(
        finding.message.contains("out") && finding.message.contains("failed"),
        "the message must list the ports the plugin does export: {}",
        finding.message
    );
}

#[test]
fn a_plugin_nodes_required_field_is_checked_like_a_core_one() {
    let mut graph = graph_with_plugin_node();
    graph.nodes[1].params = serde_json::Value::Object(
        json!({}).as_object().cloned().unwrap_or_default(),
    );
    let findings = graph::validate_with_plugins(&graph, &registry_with_mailer());
    assert!(
        findings.iter().any(|f| f.code == "missing_parameter"),
        "a required plugin field must be enforced: {findings:?}"
    );
}

#[test]
fn a_plugin_node_with_no_way_out_is_a_dangling_output_like_any_other() {
    // `inert` is `false` for every plugin node, so the dangling-output check applies. Skipping
    // it would let a graph validate whose last plugin node stops the run in the middle.
    let mut graph = graph_with_plugin_node();
    graph.edges.retain(|edge| edge.source != "b");
    let findings = graph::validate_with_plugins(&graph, &registry_with_mailer());
    assert!(
        findings.iter().any(|f| f.code == "dangling_output"),
        "a plugin node with no outgoing edge is a dead end: {findings:?}"
    );
}

#[test]
fn the_core_only_check_reports_a_plugin_node_as_unknown() {
    // `validate` is the core-only path. It must not silently accept a plugin node — an
    // organization with plugins has to use the other entry point, and the difference is
    // asserted here so nobody "simplifies" the call site later.
    let graph = graph_with_plugin_node();
    let core = graph::validate(&graph);
    assert!(core.iter().any(|f| f.code == "unknown_node_type"));
}

#[test]
fn an_unresolvable_source_adds_one_finding_not_one_per_edge() {
    // Otherwise a rule with eight edges out of a disabled plugin shows the same sentence
    // eight times and the author reads it as noise.
    let mut graph = graph_with_plugin_node();
    for index in 0..4 {
        graph.edges.push(crate::graph::Edge {
            id: format!("e-extra-{index}"),
            source: "b".to_owned(),
            source_port: "out".to_owned(),
            target: "c".to_owned(),
        });
    }
    let findings = graph::validate_with_plugins(&graph, &PluginRegistry::empty());
    let unknown: Vec<_> = findings
        .iter()
        .filter(|f| f.code == "unknown_node_type" && f.node_id.as_deref() == Some("b"))
        .collect();
    assert_eq!(unknown.len(), 1, "one broken node is one finding: {findings:?}");
    assert!(
        !findings.iter().any(|f| f.code == "port_missing"),
        "and no port finding for a type that could not be resolved at all"
    );
}

// ---- defaults -------------------------------------------------------------------------------

#[test]
fn a_select_starts_on_the_first_option_the_manifest_declared() {
    let mut registry = PluginRegistry::empty();
    registry
        .register(
            &provider(),
            &[PluginNodeType {
                params: vec![PluginParamField {
                    key: "priority".to_owned(),
                    label: "Priority".to_owned(),
                    kind: "select".to_owned(),
                    required: false,
                    options: vec!["normal".to_owned(), "high".to_owned()],
                    help: String::new(),
                }],
                ..send_node()
            }],
        )
        .expect("registers");
    assert_eq!(registry.nodes()[0].defaults, json!({ "priority": "normal" }));
}

#[test]
fn nothing_is_prefilled_because_a_fake_value_looks_configured() {
    // A `false` in an unset boolean is the same lie as a fake `example.com`: the field looks
    // set, the author cannot tell it from one they chose, and the run behaves as if they had.
    let registry = registry_with_mailer();
    assert_eq!(
        registry.nodes()[0].defaults,
        json!({}),
        "a required text field must open empty, not pre-filled"
    );
}

#[test]
fn a_select_with_no_options_is_dropped_rather_than_drawn_unanswerable() {
    let mut registry = PluginRegistry::empty();
    registry
        .register(
            &provider(),
            &[PluginNodeType {
                params: vec![PluginParamField {
                    key: "mode".to_owned(),
                    label: "Mode".to_owned(),
                    kind: "select".to_owned(),
                    required: true,
                    options: Vec::new(),
                    help: String::new(),
                }],
                ..send_node()
            }],
        )
        .expect("registers — the field is dropped, not the node");
    let node = &registry.nodes()[0];
    assert!(node.params.is_empty(), "an unanswerable field must not be drawn");
    assert_eq!(node.defaults, json!({}), "and it must not be prefilled either");
}

// ---- the projection resolves through the same registry (the save path) ---------------------
//
// The three states of the criterion are all about *validation*. The save path has a fourth
// question after that, and it is the one that used to answer differently from the validation:
// can the core turn this graph into the step list the runner executes?
//
// A graph with a plugin node validates cleanly when the plugin is enabled (the palette drew
// the node, the author wired it, the findings are empty) and then the store refused it with
// `"plugin.mailer.send" does not project onto a step` — which is the *core-only* check's
// sentence. The author had done nothing wrong, and the message sent them to fix a typo they
// never made. So the projection takes the same registry, and refuses a plugin node with a
// sentence that names the actual reason.

#[test]
fn a_graph_with_an_enabled_plugin_node_validates_and_then_is_refused_by_the_projection() {
    let registry = registry_with_mailer();
    let graph = graph_with_plugin_node();

    // The validation half passes: the plugin is enabled, so the node is a node type.
    let findings = graph::validate_with_plugins(&graph, &registry);
    assert!(
        !findings.iter().any(Finding::is_error),
        "an enabled plugin node is a known node type: {findings:?}"
    );

    // The projection half refuses it — the core has no runner for it — and says why in the
    // words that name the cause. An author who reads "the platform does not execute plugin
    // nodes yet" knows what to do; one who reads "is not a node type the platform knows"
    // goes looking for a typo in a manifest that installed fine.
    let error = graph::project_with_plugins(&graph, &registry)
        .expect_err("a plugin node has no core step to project onto");
    // `code()` and the `Display` form, not the `Invalid` variant's fields: a reader of this
    // test should not have to know that a refusal *is* always `Invalid` to check what it
    // says. The accessors are the surface the HTTP layer uses, so the assertions here are
    // made against exactly what an author's browser receives.
    let sentence = error.to_string();
    assert_eq!(error.code(), "plugin_node_not_executable");
    assert!(
        sentence.contains("does not execute plugin nodes"),
        "the sentence must name the real reason, got {sentence:?}"
    );
    assert!(
        !sentence.contains("is not a node type the platform knows"),
        "and must NOT reuse the typo sentence, got {sentence:?}"
    );
}

#[test]
fn the_projection_and_the_validation_never_disagree_about_what_exists() {
    // The defect this closes, stated as the property it broke — and stated **narrowly on
    // purpose**, because the first draft of this assertion was `!(clean && !projects)` and
    // that was wrong. `clean == false` here is the *designed* outcome: a plugin node is a
    // known type, so the findings are empty, and the projection then refuses it because the
    // core has no runner. "Valid but unprojectable" is the intended shape, and an
    // assertion that forbids it would have deleted the product decision one commit earlier.
    //
    // The property that was actually violated is narrower and is the one worth freezing:
    // the two must never disagree about *existence*. A type the projection resolves as
    // unknown must also be an `unknown_node_type` finding, and a type with an
    // `unknown_node_type` finding must not be reported by the projection as something else
    // — because that is the state where the author is told to hunt a typo they never made.
    // Executability is a separate question with a separate sentence, and a separate test.
    let registry = registry_with_mailer();
    let graph = graph_with_plugin_node();
    let findings = graph::validate_with_plugins(&graph, &registry);
    let says_unknown = findings
        .iter()
        .any(|finding| finding.code == "unknown_node_type");

    // Enabled plugin: known to both, and the disagreement is about running it, not naming it.
    assert!(
        !says_unknown,
        "an enabled plugin node exists — the findings may not call it unknown: {findings:?}"
    );
    let sentence = graph::project_with_plugins(&graph, &registry)
        .expect_err("the core cannot run it")
        .to_string();
    assert!(
        !sentence.contains("is not a node type the platform knows"),
        "and the projection may not call it unknown either: {sentence:?}"
    );

    // Disabled plugin: the second half, and the assertion that is left once the fiction of
    // "the projection owns a code here" is dropped.
    //
    // Two things are true and only the first is obvious. The disabled state IS reported as
    // `unknown_node_type` — asserted directly, and it is the same code the finding uses.
    // And the *projection* never gets to answer it at all: the save route runs the same
    // validation first and returns the whole findings list, so by the time a projection
    // could run, an unknown type has already been reported as one. Asserting the
    // projection's code here would freeze a path no client can reach — which is how a test
    // ends up protecting an implementation detail and calling it a guarantee.
    //
    // What IS reachable, and what a client actually sees, is the *sentence*: the walk wraps
    // the validation's first message rather than composing its own, so the author gets the
    // identical "re-enable it" the problems panel showed them. Two entry points, one
    // wording — and if a future refactor lets the walk invent its own sentence, this is the
    // assertion that catches it.
    let stripped = graph::validate_with_plugins(&graph, &PluginRegistry::empty());
    let finding = stripped
        .iter()
        .find(|finding| finding.code == "unknown_node_type")
        .expect("a plugin the registry no longer has is unknown");
    assert!(
        finding.message.contains("re-enable"),
        "the finding keeps the instruction the author needs: {}",
        finding.message
    );
    let walked = graph::project_with_plugins(&graph, &PluginRegistry::empty())
        .expect_err("nothing runs a node type that does not exist")
        .to_string();
    assert!(
        walked.contains(finding.message.as_str()),
        "the projection must carry the validation's own sentence, not a second wording: \
         finding {:?} vs walk {walked:?}",
        finding.message
    );
}

#[test]
fn the_same_graph_projects_against_the_core_only_wrapper_with_a_different_sentence() {
    // The two entry points stay distinct, and this is why: a *stored* graph is core-only by
    // construction (the save refuses plugin nodes), so `project` — the core-only wrapper the
    // attribution and run-from-here walks use — is correct for them. What it must never do
    // is answer a save. A plugin node through it reads as a typo, which is the false
    // accusation the criterion exists to remove.
    let graph = graph_with_plugin_node();
    let error = graph::project(&graph).expect_err("core-only cannot project a plugin node");
    assert_ne!(
        error.code(),
        "plugin_node_not_executable",
        "the core-only wrapper has no registry, so it cannot know the plugin is enabled"
    );
}

#[test]
fn a_core_only_graph_is_unaffected_by_the_new_parameter() {
    // A regression guard on the ordinary path: pass a registry that has nothing to do with
    // the graph and every core rule must still behave exactly as it did.
    let registry = registry_with_mailer();
    let graph = Graph {
        nodes: vec![
            crate::graph::Node {
                id: "a".to_owned(),
                node_type: "trigger.manual".to_owned(),
                label: "Manual".to_owned(),
                params: json!({}),
                position: crate::graph::Position { x: 0.0, y: 0.0 },
            },
            crate::graph::Node {
                id: "b".to_owned(),
                node_type: "end".to_owned(),
                label: "End".to_owned(),
                params: json!({}),
                position: crate::graph::Position { x: 240.0, y: 0.0 },
            },
        ],
        edges: vec![crate::graph::Edge {
            id: "e1".to_owned(),
            source: "a".to_owned(),
            source_port: "out".to_owned(),
            target: "b".to_owned(),
        }],
    };
    let steps = graph::project_with_plugins(&graph, &registry).expect("core nodes project");
    let baseline = graph::project(&graph).expect("core nodes project");
    assert_eq!(steps, baseline, "an unrelated registry must change nothing");
    assert_eq!(steps.len(), 1, "the end node is the only step");
}

// ---- fixtures -------------------------------------------------------------------------------

/// trigger.event → plugin.mailer.send → end
fn graph_with_plugin_node() -> Graph {
    Graph {
        nodes: vec![
            crate::graph::Node {
                id: "a".to_owned(),
                node_type: "trigger.manual".to_owned(),
                label: "Manual".to_owned(),
                params: json!({}),
                position: crate::graph::Position { x: 0.0, y: 0.0 },
            },
            crate::graph::Node {
                id: "b".to_owned(),
                node_type: "plugin.mailer.send".to_owned(),
                label: "Send mail".to_owned(),
                params: json!({ "to": "someone@example.com" }),
                position: crate::graph::Position { x: 240.0, y: 0.0 },
            },
            crate::graph::Node {
                id: "c".to_owned(),
                node_type: "end".to_owned(),
                label: "End".to_owned(),
                params: json!({}),
                position: crate::graph::Position { x: 480.0, y: 0.0 },
            },
        ],
        edges: vec![
            crate::graph::Edge {
                id: "e1".to_owned(),
                source: "a".to_owned(),
                source_port: "out".to_owned(),
                target: "b".to_owned(),
            },
            crate::graph::Edge {
                id: "e2".to_owned(),
                source: "b".to_owned(),
                source_port: "out".to_owned(),
                target: "c".to_owned(),
            },
        ],
    }
}
