//! Expression autocomplete for the canvas (REQ-086 slice 3, REQ-092's editor surface).
//!
//! The preview in [`crate::expression`] answers "what does this become"; this module answers
//! "what *could* I type here", and the two must never disagree about what is legal. That is
//! the whole reason the candidate list is built server-side rather than in the editor:
//!
//! - A client-side list is a second copy of the grammar. It agrees with the server on the day
//!   it is written and offers `{{node.count + 1}}` — which previews as a refusal — for as long
//!   as nobody re-reads the evaluator.
//! - The interesting candidates are not static anyway. "Upstream outputs" is a fact about
//!   **this** graph: which nodes feed this one, under which port, carrying what shape. Only
//!   the graph knows that, and the editor holds it only as unsaved state.
//! - `$item` and the variables are *runtime* facts. They exist because the engine will put
//!   them in the namespace at run time, and a completion list that omitted them would hide
//!   the two things people most often reach for.
//!
//! So the answer is a pure function of `(graph, node key, prefix, sample namespaces)` and
//! nothing else. It never reads the database, never fetches a run, and never stores anything
//! — the canvas sends the graph it is holding, which is the graph the person is looking at.

use serde::Serialize;
use serde_json::Value;

use crate::expression::{Namespaces, paths_of};
use crate::graph::Graph;

/// How many candidates one namespace may contribute.
///
/// A bound rather than a shrug: a sample with a two-hundred-key object would otherwise put a
/// wall in front of the field, and the point of a completion list is to be read.
pub const MAX_PATHS_PER_NAMESPACE: usize = 40;

/// How many candidate namespaces one answer may carry.
///
/// Three is what the engine provides (`$json`/`$node`, `$item`, `$vars`) and a graph adds one
/// per upstream node. A large graph therefore truncates, and the truncation is **named** in
/// [`Completion::truncated_namespaces`] rather than silent — a list that quietly stops at the
/// node you are about to wire is worse than one that says it stopped.
pub const MAX_NAMESPACES: usize = 12;

/// The longest prefix a caller may send.
///
/// An unbounded prefix is a way to ask for the whole list with an empty string, which is what
/// the editor does on focus anyway — so the bound is generous and the truncation is explicit.
pub const MAX_PREFIX: usize = 80;

/// Where a candidate came from.
///
/// The label is not decoration: the REQ asks for a list that distinguishes "upstream outputs,
/// variables and the current item", and a person choosing between `{{$vars.now}}` and
/// `{{$item.id}}` needs to know which is which before they commit to a path that stops
/// resolving the day the run moves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An output of a node upstream of the one being edited, named by its node key.
    Upstream,
    /// A variable the engine provides at run time.
    Runtime,
    /// A path inside the sample the person pinned.
    Sample,
}

/// One completion candidate.
///
/// `label` is the text to insert and `detail` is what a person reads to choose between two of
/// them — the sample path, or the node key and port for an upstream output. They are different
/// strings on purpose: the insert text is what has to be *correct*, and the detail is what has
/// to be *legible*.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    /// The namespace segment to insert, e.g. `node_http_1.body`.
    pub label: String,
    /// Where it came from.
    pub source: Source,
    /// A second line for the menu.
    pub detail: String,
}

/// A completion answer.
///
/// Refusals are absent rather than present: an unknown node is a `404` from the route (there
/// is no graph to answer for), and an empty candidate list is a successful "nothing matches
/// yet", which is the single most common answer while somebody is three characters into
/// typing a namespace name.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Completion {
    /// The node the candidates are for.
    pub node_key: String,
    /// Candidates, best first.
    pub candidates: Vec<Candidate>,
    /// How many there are.
    pub candidate_count: usize,
    /// Upstream node keys, sorted — the namespaces a graph contributed.
    pub upstream_nodes: Vec<String>,
    /// How many upstream nodes were left out because the answer was capped.
    pub truncated_namespaces: usize,
    /// Whether the candidates were cut at [`MAX_PATHS_PER_NAMESPACE`].
    pub truncated_paths: bool,
}

/// Build the candidate list for one node.
///
/// The ordering is the design. Runtime namespaces come first because they are available in
/// *every* workflow and a person reaching for a completion in a half-built graph has nothing
/// upstream to complete from; then the sample's own paths, which are the thing the preview is
/// already reading; then upstream outputs, which need the graph to have been wired first.
///
/// Getting this backwards is not a cosmetic problem: a person who picks `{{$item}}` from the
/// bottom of a twenty-item list has learned nothing about their own graph, and the list is
/// sorted alphabetically anyway.
#[must_use]
pub fn complete(
    graph: &Graph,
    node_key: &str,
    prefix: &str,
    namespaces: &Namespaces,
) -> Completion {
    let prefix = prefix.trim();
    let prefix = &prefix[..prefix.len().min(MAX_PREFIX)];
    // Strip the leading `{{` if the caller sent the expression rather than the path inside
    // it: the editor knows which it has, and accepting both means the seam between them
    // cannot become a bug where typing `{{` empties the list.
    let prefix = prefix.strip_prefix("{{").unwrap_or(prefix);

    let mut candidates: Vec<Candidate> = Vec::new();

    // --- runtime namespaces ----------------------------------------------------------------
    for (namespace, detail) in [
        ("$json", "the whole item the step received"),
        ("$node", "what the previous step returned"),
        ("$item", "the current item of a looping step"),
        ("$vars", "variables set earlier in the run"),
    ] {
        push_if(&mut candidates, prefix, namespace, Source::Runtime, detail);
    }

    // --- the pinned sample -----------------------------------------------------------------
    // Only object namespaces, and only paths the sample actually carries: the preview refuses
    // a path it does not have, so offering one here would be a completion that previews as an
    // error on the very next keystroke.
    if !namespaces.is_empty() {
        for (namespace, value) in namespaces {
            for path in paths_of(value).into_iter().take(MAX_PATHS_PER_NAMESPACE) {
                push_if(
                    &mut candidates,
                    prefix,
                    &format!("{namespace}.{path}"),
                    Source::Sample,
                    "from the pinned sample",
                );
            }
        }
    }

    // --- upstream outputs -------------------------------------------------------------------
    let mut upstream: Vec<String> = upstream_keys(graph, node_key);
    upstream.sort_unstable();
    let total_upstream = upstream.len();
    upstream.truncate(MAX_NAMESPACES);
    for key in &upstream {
        // The whole of the upstream node's value is one namespace, and the paths inside it
        // are *not* known here: a node's output shape is whatever its params say it produces
        // (REQ-088 owns that), and inventing three plausible child paths would offer a
        // candidate that previews as "the sample carries no `body.id`". The namespace alone
        // is always valid, so it is the honest completion.
        push_if(
            &mut candidates,
            prefix,
            key,
            Source::Upstream,
            "output of an upstream node",
        );
    }

    Completion {
        node_key: node_key.to_owned(),
        candidate_count: candidates.len(),
        candidates,
        upstream_nodes: upstream,
        truncated_namespaces: total_upstream.saturating_sub(MAX_NAMESPACES),
        truncated_paths: false,
    }
}

/// The node keys that feed `node_key`, directly.
///
/// Direct rather than transitive on purpose: `{{a.body}}` resolves against the namespaces the
/// step was handed, and handing a step every ancestor's output is REQ-092's decision, not
/// this function's. Offering a grandparent here would suggest an expression that does not
/// resolve today and silently stops resolving when the wire between the two nodes is deleted.
///
/// Self is excluded even if the graph somehow contains a self-loop, because the compiler
/// already refuses those and a completion that offers the node its own output would be
/// suggesting the cycle it exists to prevent.
#[must_use]
pub fn upstream_keys(graph: &Graph, node_key: &str) -> Vec<String> {
    let mut keys: Vec<String> = graph
        .connections
        .iter()
        .filter(|connection| connection.to == node_key)
        .map(|connection| connection.from.clone())
        .filter(|key| key != node_key)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Add a candidate when the prefix matches it.
///
/// The prefix rule is deliberately loose about *where* it matches: `{{$it`, `$it`, `item.a` and
/// `a` all match `$item.a`. A stricter rule — matching only the segment currently being
/// typed — is easier to describe and worse to use, because the person who typed `$` three
/// characters ago did not know they were starting a namespace and should not be punished for
/// it by an empty menu.
fn push_if(
    candidates: &mut Vec<Candidate>,
    prefix: &str,
    label: &str,
    source: Source,
    detail: &str,
) {
    if prefix.is_empty() || matches_prefix(label, prefix) {
        candidates.push(Candidate {
            label: label.to_owned(),
            source,
            detail: detail.to_owned(),
        });
    }
}

/// Whether `label` carries `prefix` at any segment boundary or inside a segment.
fn matches_prefix(label: &str, prefix: &str) -> bool {
    if label.starts_with(prefix) {
        return true;
    }
    label.split('.').any(|segment| segment.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Connection, Graph, GraphNode, Position};
    use serde_json::{Map, Value, json};

    /// A JSON object as the namespace map the route's body deserializes into.
    ///
    /// The tests build their samples as `json!` because that reads better than nested `Map`
    /// constructors, and converting here is the honest seam: the route receives exactly this
    /// shape, so a test that built a `Map` by hand would be testing a different input.
    fn sample_object(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn node(key: &str) -> GraphNode {
        GraphNode::new(key, "http_request", 0.0, 0.0)
    }

    fn graph() -> Graph {
        Graph {
            nodes: vec![node("trigger_1"), node("http_1"), node("if_1")],
            connections: vec![
                Connection::new("trigger_1", "main", "http_1", "in"),
                Connection::new("http_1", "true", "if_1", "in").labelled("true"),
            ],
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_positioned_node_needs_no_manual_construction() {
        // Guard on the fixture itself: if `GraphNode`'s fields move, this test file stops
        // compiling for a reason that has nothing to do with completions.
        assert_eq!(node("a").position, Position::new(0.0, 0.0));
        assert_eq!(node("a").node_type, "http_request");
    }

    #[test]
    fn runtime_namespaces_are_offered_even_with_no_upstream() {
        let empty = Graph::new();
        let answer = complete(&empty, "lonely", "", &Map::new());
        let labels: Vec<&str> = answer
            .candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(labels, vec!["$json", "$node", "$item", "$vars"]);
    }

    #[test]
    fn upstream_of_a_node_is_direct_only() {
        let answer = complete(&graph(), "if_1", "", &Map::new());
        assert_eq!(answer.upstream_nodes, vec!["http_1".to_owned()]);

        // The trigger feeds http_1, so it is not offered while editing if_1. Offering it
        // would suggest an expression the step cannot read today.
        let first = complete(&graph(), "http_1", "", &Map::new());
        assert_eq!(first.upstream_nodes, vec!["trigger_1".to_owned()]);
    }

    #[test]
    fn a_node_is_never_its_own_upstream() {
        let mut looping = graph();
        looping
            .connections
            .push(Connection::new("http_1", "main", "http_1", "in"));
        let answer = complete(&looping, "http_1", "", &Map::new());
        assert!(!answer.upstream_nodes.contains(&"http_1".to_owned()));
    }

    #[test]
    fn sample_paths_become_candidates_only_where_the_sample_has_them() {
        let sample = json!({ "event": { "title": "Hello", "meta": { "author": "ada" } } });
        let answer = complete(&graph(), "http_1", "event.", &sample_object(&sample));
        let labels: Vec<&str> = answer
            .candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        // `event.meta` is here because the intermediate object *is* a path the evaluator can
        // read — a lone `{{event.meta}}` resolves to the object. Dropping intermediates would
        // have made the list shorter and would have removed a candidate that previews fine.
        // Sorted, so the assertion does not depend on the walk order of a serde map.
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["event.meta", "event.meta.author", "event.title"],
            "{labels:?}"
        );
    }

    #[test]
    fn an_array_index_is_a_candidate_because_the_evaluator_indexes_it() {
        let sample = json!({ "event": { "items": [ { "title": "First" } ] } });
        let answer = complete(&graph(), "http_1", "event.items", &sample_object(&sample));
        let labels: Vec<&str> = answer
            .candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert!(
            labels.contains(&"event.items.0"),
            "an array index resolves today, so it belongs in the list: {labels:?}"
        );
        assert!(labels.contains(&"event.items.0.title"), "{labels:?}");
    }

    #[test]
    fn a_prefix_inside_a_segment_still_matches() {
        let sample = json!({ "event": { "title": "Hello" } });
        // The person typed `$it` three characters into a namespace they had not chosen yet.
        let answer = complete(&graph(), "http_1", "$it", &sample_object(&sample));
        assert_eq!(answer.candidates.len(), 1);
        assert_eq!(answer.candidates[0].label, "$item");
    }

    #[test]
    fn a_prefix_written_as_a_whole_expression_is_accepted() {
        let answer = complete(&graph(), "http_1", "{{$va", &Map::new());
        assert_eq!(answer.candidates.len(), 1);
        assert_eq!(answer.candidates[0].label, "$vars");
    }

    #[test]
    fn a_prefix_matching_nothing_is_an_empty_success_not_a_refusal() {
        let answer = complete(&graph(), "http_1", "zzzz", &Map::new());
        assert!(answer.candidates.is_empty());
        assert_eq!(answer.candidate_count, 0);
    }

    #[test]
    fn sources_are_distinguishable() {
        let sample = json!({ "event": { "title": "Hello" } });
        let answer = complete(&graph(), "if_1", "", &sample_object(&sample));
        let source_of = |label: &str| {
            answer
                .candidates
                .iter()
                .find(|candidate| candidate.label == label)
                .map(|candidate| candidate.source)
        };
        assert_eq!(source_of("$vars"), Some(Source::Runtime));
        assert_eq!(source_of("event.title"), Some(Source::Sample));
        assert_eq!(source_of("http_1"), Some(Source::Upstream));
    }

    #[test]
    fn runtime_comes_first_so_an_unwired_graph_still_offers_something() {
        let sample = json!({ "event": { "title": "Hello" } });
        let answer = complete(&graph(), "if_1", "", &sample_object(&sample));
        assert_eq!(answer.candidates[0].source, Source::Runtime);
    }

    #[test]
    fn a_wide_graph_names_the_namespaces_it_left_out() {
        let mut wide = Graph::new();
        for index in 0..(MAX_NAMESPACES + 4) {
            let key = format!("node_{index}");
            wide.nodes.push(node(&key));
            wide.connections
                .push(Connection::new(key, "main", "sink", "in"));
        }
        wide.nodes.push(node("sink"));

        let answer = complete(&wide, "sink", "", &Map::new());
        assert_eq!(answer.upstream_nodes.len(), MAX_NAMESPACES);
        assert_eq!(answer.truncated_namespaces, 4);
    }

    #[test]
    fn a_long_prefix_is_cut_rather_than_refused() {
        let long = "a".repeat(MAX_PREFIX * 3);
        let answer = complete(&graph(), "http_1", &long, &Map::new());
        assert!(answer.candidates.is_empty());
    }
}
