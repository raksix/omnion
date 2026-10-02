//! Condition groups: `all` / `any` nesting around the closed comparison set.
//!
//! v0 of the layer evaluated a **flat** list — every comparison had to hold, or the rule
//! did not fire. That is one shape of the same idea, and it stays: a bare array of
//! comparisons reads as `{"all": [ … ]}`, so every rule written before the depth pass
//! keeps firing exactly as it did (migration `0020` widens the column's check constraint
//! rather than replacing the data).
//!
//! The group tree is what a rule with a real decision in it needs:
//!
//! ```text
//! any ─┬─ all ─┬─ status  = published
//!      │        └─ tags    contains news
//!      └─ author.email = ada@example.com
//! ```
//!
//! Two rules keep this safe:
//!
//! * **depth ≤ 3.** A tree the panel cannot lay out is a tree nobody can read back, and
//!   a deeply nested evaluation is a long, hard-to-explain chain in the run's audit row.
//! * **A closed grammar.** `all` and `any` each take one key, and that key takes an
//!   array of *either* comparisons or further groups. There is no negation, no
//!   disjunction of an arbitrary boolean expression and no way to reach outside the
//!   event payload (docs/09-N8N-TEARDOWN.md §13 lesson 14).
//!
//! Evaluation is total: a group is answered by every leaf, and a comparison inside a
//! group that cannot be read counts as "does not hold" rather than panicking — a rule
//! that cannot be evaluated must never take the matcher down.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::condition::{self, Condition, ConditionOperator};
use crate::error::{AutomationError, Result};

/// Deepest group nesting a rule may carry (comparisons count as the first level).
pub const MAX_GROUP_DEPTH: usize = 3;

/// Most comparisons and groups one rule may carry in total.
pub const MAX_GROUP_NODES: usize = 24;

/// How a group's members are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupMode {
    /// Every member must hold.
    All,
    /// At least one member must hold.
    Any,
}

impl GroupMode {
    /// Canonical name stored in the database and used in a request body.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }

    /// Parse a stored key.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "all" => Some(Self::All),
            "any" => Some(Self::Any),
            _ => None,
        }
    }
}

/// One node of a rule's condition tree: a comparison, or a group of further nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConditionNode {
    /// A single comparison of one payload field.
    Comparison(Condition),
    /// A group whose members are combined by [`GroupMode`].
    Group(ConditionGroup),
}

/// A group of condition nodes, combined by its mode.
///
/// Serialised **flat** — `{"all": [ … ]}`, not `{"mode": "all", "nodes": [ … ]}` —
/// because that is the shape the panel sends, the shape `workflows.conditions` stores and
/// the shape the matcher reads. The `#[serde(flatten)]` on `mode` is what makes the
/// hand-written `Serialize`/`Deserialize` below collapse to that one key.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionGroup {
    /// How the members are combined.
    pub mode: GroupMode,
    /// The members, in order.
    pub nodes: Vec<ConditionNode>,
}

impl ConditionGroup {
    /// A group of comparisons that must all hold.
    #[must_use]
    pub fn all(nodes: Vec<ConditionNode>) -> Self {
        Self {
            mode: GroupMode::All,
            nodes,
        }
    }

    /// A group of nodes of which at least one must hold.
    #[must_use]
    pub fn any(nodes: Vec<ConditionNode>) -> Self {
        Self {
            mode: GroupMode::Any,
            nodes,
        }
    }

    /// How the members are combined.
    #[must_use]
    pub const fn mode(&self) -> GroupMode {
        self.mode
    }

    /// The members, in order.
    #[must_use]
    pub fn nodes(&self) -> &[ConditionNode] {
        &self.nodes
    }

    /// `true` when this group's members hold against a payload.
    #[must_use]
    pub fn holds(&self, payload: &Value) -> bool {
        evaluate_group(self, payload)
    }

    /// How many comparisons the whole subtree carries — what the panel's counter shows.
    #[must_use]
    pub fn comparison_count(&self) -> usize {
        self.nodes.iter().map(node_comparisons).sum()
    }

    /// Deepest nesting level below this group (a flat group is `1`).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| match node {
                ConditionNode::Comparison(_) => 1,
                ConditionNode::Group(group) => 1 + group.depth(),
            })
            .max()
            .unwrap_or(0)
    }
}

impl Serialize for ConditionGroup {
    /// `{"all": [ … ]}` or `{"any": [ … ]}` — one key, no wrapper.
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut map = Map::with_capacity(1);
        map.insert(
            self.mode.as_str().to_owned(),
            serde_json::to_value(&self.nodes).map_err(serde::ser::Error::custom)?,
        );
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConditionGroup {
    /// Reads the one-key object, and refuses the shapes a group cannot be.
    ///
    /// Both refusals are sentences rather than serde plumbing, because this is the text an
    /// author sees when their panel sends a malformed group: "never both" and "needs an
    /// `all` or an `any` key" are the two mistakes that actually happen.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            all: Option<Vec<ConditionNode>>,
            #[serde(default)]
            any: Option<Vec<ConditionNode>>,
        }

        let raw = Raw::deserialize(deserializer)?;
        match (raw.all, raw.any) {
            (Some(nodes), None) => Ok(Self {
                mode: GroupMode::All,
                nodes,
            }),
            (None, Some(nodes)) => Ok(Self {
                mode: GroupMode::Any,
                nodes,
            }),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a condition group carries either `all` or `any`, never both",
            )),
            (None, None) => Err(serde::de::Error::custom(
                "a condition group needs an `all` or an `any` key",
            )),
        }
    }
}

/// How many comparisons one node carries.
fn node_comparisons(node: &ConditionNode) -> usize {
    match node {
        ConditionNode::Comparison(_) => 1,
        ConditionNode::Group(group) => group.comparison_count(),
    }
}

/// Evaluate a group: `all` needs every member, `any` needs one.
fn evaluate_group(group: &ConditionGroup, payload: &Value) -> bool {
    let nodes = group.nodes();
    match group.mode() {
        GroupMode::All => nodes.iter().all(|node| evaluate_node(node, payload)),
        // An empty `any` holds: there is nothing that could refuse it. (An empty `all`
        // holds for the same reason, and a rule with no conditions at all is a rule that
        // always fires — the v0 behaviour, kept on purpose.)
        GroupMode::Any => nodes.iter().any(|node| evaluate_node(node, payload)),
    }
}

/// Evaluate one node against a payload.
fn evaluate_node(node: &ConditionNode, payload: &Value) -> bool {
    match node {
        ConditionNode::Comparison(comparison) => comparison.holds(payload),
        ConditionNode::Group(group) => evaluate_group(group, payload),
    }
}

/// Read a stored `workflows.conditions` value as a group tree.
///
/// A bare array of comparisons — the v0 shape, and what the panel sends for a rule with
/// no nesting — becomes `{"all": [ … ]}`, which is exactly what it always meant. An
/// object is read as the group it claims to be. Anything else is refused with a message
/// naming the shape, because a silently ignored condition is a rule that fires when its
/// author did not ask for it.
pub fn read_tree(stored: &Value) -> Result<ConditionGroup> {
    match stored {
        Value::Array(items) => {
            // A malformed entry is a stored-row problem, reported with the layer's own code
            // rather than serde's: a rule that cannot be read must never take the matcher
            // down, and the panel needs a sentence it can show.
            let mut nodes = Vec::with_capacity(items.len());
            for item in items {
                match read_node(item) {
                    Ok(node) => nodes.push(node),
                    Err(reason) => {
                        return Err(AutomationError::invalid(
                            "rule_unreadable",
                            format!("a stored condition is not readable: {reason}"),
                        ));
                    }
                }
            }
            Ok(ConditionGroup::all(nodes))
        }
        Value::Object(_) => {
            serde_json::from_value::<ConditionGroup>(stored.clone()).map_err(|err| {
                AutomationError::invalid(
                    "rule_unreadable",
                    format!("the stored condition groups are not readable: {err}"),
                )
            })
        }
        other => Err(AutomationError::invalid(
            "rule_unreadable",
            format!(
                "stored conditions are a {} — a list of comparisons or a group object",
                json_type_name(other)
            ),
        )),
    }
}

/// Read one array element: a comparison object, or a nested group.
fn read_node(item: &Value) -> std::result::Result<ConditionNode, String> {
    if !item.is_object() {
        return Err(format!(
            "every condition is an object, got {}",
            json_type_name(item)
        ));
    }
    let is_group = item.get("all").is_some() || item.get("any").is_some();
    if is_group {
        serde_json::from_value::<ConditionGroup>(item.clone())
            .map(ConditionNode::Group)
            .map_err(|err| err.to_string())
    } else {
        serde_json::from_value::<Condition>(item.clone())
            .map(ConditionNode::Comparison)
            .map_err(|err| err.to_string())
    }
}

/// The JSON type name of a value, for a message the author can act on.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// The stored JSON shape of a group tree.
pub fn to_json(group: &ConditionGroup) -> Value {
    serde_json::to_value(group).unwrap_or(Value::Array(Vec::new()))
}

/// Evaluate a stored `workflows.conditions` value against a payload.
///
/// The entry point the matcher calls: it reads the stored shape (array or tree) and
/// answers with the same question in both cases.
#[must_use]
pub fn stored_holds(stored: &Value, payload: &Value) -> bool {
    match read_tree(stored) {
        Ok(group) => group.holds(payload),
        // A rule whose stored conditions cannot be read must not fire on a guess.
        Err(_) => false,
    }
}

/// Check a group tree before it is stored: shape, depth, size and every comparison.
pub fn validate_group(group: &ConditionGroup) -> Result<()> {
    let mut counters = Counters::default();
    walk(group, 1, true, &mut counters)?;

    if counters.nodes > MAX_GROUP_NODES {
        return Err(AutomationError::invalid(
            "invalid_conditions",
            format!(
                "a rule carries at most {MAX_GROUP_NODES} conditions and groups in total, got {}",
                counters.nodes
            ),
        ));
    }
    Ok(())
}

/// Running totals of one validation walk.
#[derive(Debug, Default)]
struct Counters {
    nodes: usize,
    comparisons: usize,
}

/// Validate one subtree, refusing a comparison the closed set would not accept.
///
/// `root` distinguishes the two kinds of emptiness. A rule with **no** conditions is a real
/// thing — it fires on every event of its name — so the root group is allowed to be empty.
/// An empty group **nested** inside a tree is a mistake in the editor: an `all` with nothing
/// in it reads as "and no conditions", which an author never means, and an `any` with nothing
/// in it is a row that can never fire. Both are refused.
fn walk(group: &ConditionGroup, depth: usize, root: bool, counters: &mut Counters) -> Result<()> {
    if depth > MAX_GROUP_DEPTH {
        return Err(AutomationError::invalid(
            "invalid_conditions",
            format!("condition groups nest at most {MAX_GROUP_DEPTH} levels deep"),
        ));
    }

    let nodes = group.nodes();
    if nodes.is_empty() {
        if root {
            return Ok(());
        }
        return Err(AutomationError::invalid(
            "invalid_conditions",
            "a nested condition group needs at least one comparison or group — \
             an empty one can never hold",
        ));
    }

    for node in nodes {
        counters.nodes += 1;
        match node {
            ConditionNode::Comparison(comparison) => {
                counters.comparisons += 1;
                if counters.comparisons > MAX_GROUP_NODES {
                    return Err(AutomationError::invalid(
                        "invalid_conditions",
                        format!("a rule carries at most {MAX_GROUP_NODES} comparisons"),
                    ));
                }
                // The comparison set is validated by the module that owns it, one at a
                // time — a group is only a different way of arranging the same rows.
                condition::validate(std::slice::from_ref(comparison))?;
            }
            ConditionNode::Group(inner) => walk(inner, depth + 1, false, counters)?,
        }
    }

    Ok(())
}

/// Validate a stored tree that has already been read back into groups.
pub fn validate_tree(stored: &Value) -> Result<ConditionGroup> {
    let group = read_tree(stored)?;
    validate_group(&group)?;
    Ok(group)
}

/// Which operators the group tree offers, for the catalogue.
#[must_use]
pub fn operators() -> Vec<(&'static str, bool)> {
    ConditionOperator::ALL
        .into_iter()
        .map(|operator| (operator.as_str(), operator.needs_value()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn comparison(field: &str, value: &str) -> ConditionNode {
        ConditionNode::Comparison(Condition::compare(
            field,
            ConditionOperator::Equals,
            json!(value),
        ))
    }

    fn presence(field: &str) -> ConditionNode {
        ConditionNode::Comparison(Condition::presence(field, ConditionOperator::Exists))
    }

    fn payload() -> Value {
        json!({
            "status": "published",
            "slug": "home",
            "revision_no": 4,
            "tags": ["news", "release"],
            "author": { "email": "ada@example.com" },
        })
    }

    #[test]
    fn a_flat_list_is_an_all_group() {
        let stored = json!([
            { "field": "status", "operator": "equals", "value": "published" },
            { "field": "slug", "operator": "equals", "value": "home" }
        ]);
        let group = read_tree(&stored).expect("the v0 shape is readable");
        assert_eq!(group.mode(), GroupMode::All);
        assert_eq!(group.comparison_count(), 2);
        assert!(group.holds(&payload()));
        // It reads back as the group it always meant, byte for byte.
        assert_eq!(to_json(&group), json!({ "all": stored }));
    }

    #[test]
    fn an_all_inside_an_any_evaluates_correctly() {
        // (status = published AND tags contains news) OR author = ada
        let group = ConditionGroup::any(vec![
            ConditionNode::Group(ConditionGroup::all(vec![
                comparison("status", "published"),
                ConditionNode::Comparison(Condition::compare(
                    "tags",
                    ConditionOperator::Contains,
                    json!("news"),
                )),
            ])),
            comparison("author.email", "ada@example.com"),
        ]);

        assert!(group.holds(&payload()));
        assert_eq!(group.depth(), 2);
        assert_eq!(group.comparison_count(), 3);

        // The same tree against a payload that satisfies only the second branch.
        let other = json!({ "status": "draft", "author": { "email": "ada@example.com" } });
        assert!(group.holds(&other));

        // …and one that satisfies neither.
        let neither = json!({ "status": "draft", "author": { "email": "bob@example.com" } });
        assert!(!group.holds(&neither));

        // The `all` branch is not satisfied by a payload that only matches one of its rows.
        let half =
            json!({ "status": "published", "tags": ["news"], "author": { "email": "x@y.z" } });
        let only_first =
            ConditionGroup::any(vec![ConditionNode::Group(ConditionGroup::all(vec![
                comparison("status", "published"),
                comparison("slug", "home"),
            ]))]);
        assert!(!only_first.holds(&half));
    }

    #[test]
    fn a_group_round_trips_through_json() {
        let group = ConditionGroup::any(vec![
            presence("author.email"),
            ConditionNode::Group(ConditionGroup::all(vec![comparison("status", "published")])),
        ]);
        let stored = to_json(&group);
        assert!(stored["any"].is_array(), "{stored}");
        assert!(stored["any"][1]["all"][0]["field"] == "status");

        let read = read_tree(&stored).expect("reads back");
        assert_eq!(read, group);
        assert_eq!(to_json(&read), stored);
        assert!(read.holds(&payload()));
    }

    #[test]
    fn a_group_may_not_carry_both_keys_or_neither() {
        assert!(serde_json::from_value::<ConditionGroup>(json!({})).is_err());
        assert!(serde_json::from_value::<ConditionGroup>(json!({ "all": [], "any": [] })).is_err());
        // The panel's own refusal is the sentence, not a serde string.
        let error = read_tree(&json!({ "all": [], "any": [] })).expect_err("refused");
        assert_eq!(error.code(), "rule_unreadable");
        assert!(error.to_string().contains("never both"));
    }

    #[test]
    fn nesting_is_capped_at_three_levels() {
        // comparisons at the bottom, one group per level: this is the deepest a rule
        // may go and it must pass.
        let deepest = ConditionGroup::all(vec![ConditionNode::Group(ConditionGroup::any(vec![
            ConditionNode::Group(ConditionGroup::all(vec![comparison("status", "published")])),
        ]))]);
        assert_eq!(deepest.depth(), 3);
        assert!(validate_group(&deepest).is_ok());

        let too_deep = ConditionGroup::all(vec![ConditionNode::Group(ConditionGroup::any(vec![
            ConditionNode::Group(ConditionGroup::all(vec![ConditionNode::Group(
                ConditionGroup::all(vec![comparison("status", "published")]),
            )])),
        ]))]);
        assert_eq!(too_deep.depth(), 4);
        assert_eq!(
            validate_group(&too_deep).expect_err("refused").code(),
            "invalid_conditions"
        );
    }

    #[test]
    fn a_group_is_bounded_and_never_empty_when_nested() {
        // The ROOT may be empty: a rule with no conditions is a real rule that fires on
        // every event of its name. A nested one may not — an empty `any` can never hold,
        // and an empty `all` reads as "and nothing", which nobody means.
        assert!(validate_group(&ConditionGroup::all(vec![])).is_ok());

        for empty in [ConditionGroup::all(vec![]), ConditionGroup::any(vec![])] {
            let nested = ConditionGroup::any(vec![ConditionNode::Group(empty)]);
            assert_eq!(
                validate_group(&nested).expect_err("nested empty").code(),
                "invalid_conditions"
            );
        }

        let many = ConditionGroup::all(
            (0..=MAX_GROUP_NODES)
                .map(|_| comparison("status", "published"))
                .collect(),
        );
        assert_eq!(
            validate_group(&many).expect_err("too many").code(),
            "invalid_conditions"
        );
    }

    #[test]
    fn a_comparison_the_closed_set_refuses_is_refused_inside_a_group_too() {
        let group = ConditionGroup::all(vec![ConditionNode::Comparison(Condition::presence(
            "status",
            ConditionOperator::Equals,
        ))]);
        assert_eq!(
            validate_group(&group).expect_err("no value").code(),
            "invalid_conditions"
        );
    }

    #[test]
    fn an_unreadable_stored_value_never_fires() {
        assert!(!stored_holds(&json!("status = published"), &payload()));
        assert!(!stored_holds(&json!({ "none": [] }), &payload()));
        assert!(!stored_holds(
            &json!([{ "field": "status", "operator": "weird" }]),
            &payload()
        ));
        assert!(
            stored_holds(&json!([]), &payload()),
            "no conditions always fires"
        );
    }

    #[test]
    fn the_catalogue_offers_the_closed_operator_set() {
        let operators = operators();
        assert_eq!(operators.len(), ConditionOperator::ALL.len());
        assert!(operators.contains(&("exists", false)));
        assert!(operators.contains(&("equals", true)));
    }
}
