//! Omnion policy engine — the attribute-based half of the decision path (docs/07-IAM.md §11).
//!
//! This crate is pure: it knows nothing about HTTP, SQL or sessions. It answers one question —
//! *does a policy speak about this permission in this request, and what does it say* — over a
//! condition tree the builder writes:
//!
//! ```text
//! {"all": [
//!    {"attribute": "resource.path", "operator": "starts_with", "value": "/blog"},
//!    {"attribute": "action",        "operator": "==",          "value": "content.pages.publish"}
//! ]}
//! ```
//!
//! Two rules come straight from the design document and are pinned by tests here:
//!
//! * **A missing attribute compares as null.** Attributes are resolved by dotted path; a path
//!   that names nothing resolves to JSON `null` and is compared as such, never as a fabricated
//!   value — so `== null` matches an absent attribute and `>` is simply `false` for it.
//! * **Deny wins.** Among the policies that match, the highest priority decides; at equal
//!   priority a `deny` beats an `allow`. Everything else is unchanged, so an allow policy is the
//!   only way a policy can grant a permission RBAC did not, and a deny policy the only way it can
//!   take one away.
//!
//! The persistence side (loading `policies` rows, the overlay over an RBAC decision, the version
//! history) lives in `omnion-permissions`; this crate stays runnable and testable without a
//! database.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use uuid::Uuid;

/// Operators a condition leaf can carry.
///
/// Serialised with their own symbols (`==`, `!=`, `>`, `<`, `in`, `starts_with`, `contains`), so
/// the stored document, the builder and the dry run all speak the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// JSON equality (`null == null` holds, so an absent attribute matches a null value).
    Equals,
    /// Negation of [`Operator::Equals`].
    NotEquals,
    /// Numeric comparison; a non-numeric side (including null) is `false`.
    GreaterThan,
    /// Numeric comparison; a non-numeric side (including null) is `false`.
    LessThan,
    /// Membership: the expected side must be an array that contains the resolved value.
    In,
    /// Prefix: both sides must be strings.
    StartsWith,
    /// Substring for strings; membership for arrays. Both fall back to `false`.
    Contains,
}

impl Operator {
    /// Every operator, in the order the builder lists them.
    #[must_use]
    pub fn all() -> [Self; 7] {
        [
            Self::Equals,
            Self::NotEquals,
            Self::GreaterThan,
            Self::LessThan,
            Self::In,
            Self::StartsWith,
            Self::Contains,
        ]
    }

    /// The operator as it is stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "==",
            Self::NotEquals => "!=",
            Self::GreaterThan => ">",
            Self::LessThan => "<",
            Self::In => "in",
            Self::StartsWith => "starts_with",
            Self::Contains => "contains",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::all().into_iter().find(|op| op.as_str() == text)
    }

    /// Apply the operator to one resolved attribute and the value the policy expected.
    #[must_use]
    pub fn apply(self, resolved: &Value, expected: &Value) -> bool {
        match self {
            Self::Equals => resolved == expected,
            Self::NotEquals => resolved != expected,
            Self::GreaterThan => compare(resolved, expected).is_some_and(|ordering| ordering > 0),
            Self::LessThan => compare(resolved, expected).is_some_and(|ordering| ordering < 0),
            Self::In => expected
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == resolved)),
            Self::StartsWith => match (resolved.as_str(), expected.as_str()) {
                (Some(text), Some(prefix)) => text.starts_with(prefix),
                _ => false,
            },
            Self::Contains => match resolved {
                Value::Array(items) => items.iter().any(|item| item == expected),
                Value::String(text) => expected
                    .as_str()
                    .is_some_and(|needle| !needle.is_empty() && text.contains(needle)),
                _ => false,
            },
        }
    }
}

/// Order two JSON numbers; `None` when either side is not one (a string, an array, null).
fn compare(left: &Value, right: &Value) -> Option<i8> {
    let left = left.as_f64()?;
    let right = right.as_f64()?;
    Some(if left > right {
        1
    } else if left < right {
        -1
    } else {
        0
    })
}

/// One node of a condition tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// Every child must hold (an empty `all` holds — the shape a policy without conditions uses).
    All(Vec<Condition>),
    /// At least one child must hold (an empty `any` never holds).
    Any(Vec<Condition>),
    /// The child must not hold.
    Not(Box<Condition>),
    /// One attribute compared with one operator against one value.
    Leaf {
        /// Dotted path into the attribute set (`resource.path`, `user.plan`).
        attribute: String,
        /// How the comparison runs.
        operator: Operator,
        /// What the attribute is compared against.
        value: Value,
    },
}

impl Condition {
    /// The tree a policy without conditions starts from: it matches everything it targets.
    #[must_use]
    pub fn empty() -> Self {
        Self::All(Vec::new())
    }

    /// Read a stored condition node.
    ///
    /// A node is an object carrying exactly one of `all`, `any`, `not` — or `attribute` together
    /// with `operator` and `value`. Anything else is refused with a sentence naming the problem,
    /// because a policy that is silently misread is a policy that silently grants.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let Some(object) = value.as_object() else {
            return Err("a condition must be an object".to_owned());
        };

        if let Some(children) = object.get("all") {
            let children = children
                .as_array()
                .ok_or_else(|| "`all` must be an array of conditions".to_owned())?
                .iter()
                .map(Self::parse)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::All(children));
        }

        if let Some(children) = object.get("any") {
            let children = children
                .as_array()
                .ok_or_else(|| "`any` must be an array of conditions".to_owned())?
                .iter()
                .map(Self::parse)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::Any(children));
        }

        if let Some(child) = object.get("not") {
            return Ok(Self::Not(Box::new(Self::parse(child)?)));
        }

        if object.contains_key("attribute") {
            let attribute = object
                .get("attribute")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|attribute| !attribute.is_empty())
                .ok_or_else(|| "a condition needs a non-empty attribute".to_owned())?;
            let operator = object
                .get("operator")
                .and_then(Value::as_str)
                .and_then(Operator::parse)
                .ok_or_else(|| {
                    format!(
                        "unknown operator — expected one of {}",
                        Operator::all()
                            .iter()
                            .map(|op| op.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            let value = object
                .get("value")
                .cloned()
                .ok_or_else(|| "a condition needs a value".to_owned())?;
            return Ok(Self::Leaf {
                attribute: attribute.to_owned(),
                operator,
                value,
            });
        }

        Err("a condition is `all`, `any`, `not` or an attribute comparison".to_owned())
    }

    /// Write the node back in the stored shape.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::All(children) => Value::Object(map_of("all", children)),
            Self::Any(children) => Value::Object(map_of("any", children)),
            Self::Not(child) => {
                let mut object = Map::new();
                object.insert("not".to_owned(), child.to_json());
                Value::Object(object)
            }
            Self::Leaf {
                attribute,
                operator,
                value,
            } => {
                let mut object = Map::new();
                object.insert("attribute".to_owned(), Value::String(attribute.clone()));
                object.insert(
                    "operator".to_owned(),
                    Value::String(operator.as_str().to_owned()),
                );
                object.insert("value".to_owned(), value.clone());
                Value::Object(object)
            }
        }
    }

    /// Does the whole tree hold for these attributes?
    #[must_use]
    pub fn satisfied(&self, attributes: &Attributes) -> bool {
        match self {
            Self::All(children) => children.iter().all(|child| child.satisfied(attributes)),
            Self::Any(children) => children.iter().any(|child| child.satisfied(attributes)),
            Self::Not(child) => !child.satisfied(attributes),
            Self::Leaf {
                attribute,
                operator,
                value,
            } => operator.apply(&attributes.get(attribute), value),
        }
    }
}

/// `{"<key>": [child, …]}`.
fn map_of(key: &str, children: &[Condition]) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert(
        key.to_owned(),
        Value::Array(children.iter().map(Condition::to_json).collect()),
    );
    object
}

/// The attributes a condition can read: user attributes plus request attributes.
///
/// Paths are dotted (`user.plan`, `resource.site_id`). A path that names nothing — through a
/// missing intermediate object as well as a missing leaf — resolves to `Value::Null`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attributes {
    map: Map<String, Value>,
}

impl Attributes {
    /// Wrap a JSON object.
    #[must_use]
    pub fn new(map: Map<String, Value>) -> Self {
        Self { map }
    }

    /// Read a JSON value as an attribute set; a non-object reads as empty.
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        Self {
            map: value.as_object().cloned().unwrap_or_default(),
        }
    }

    /// The attribute at a dotted path, or `null`.
    #[must_use]
    pub fn get(&self, path: &str) -> Value {
        let mut segments = path.split('.');
        let Some(first) = segments.next() else {
            return Value::Null;
        };
        let mut cursor = match self.map.get(first) {
            Some(value) => value,
            None => return Value::Null,
        };
        for segment in segments {
            cursor = match cursor.get(segment) {
                Some(value) => value,
                None => return Value::Null,
            };
        }
        cursor.clone()
    }

    /// Set the attribute at a dotted path, creating intermediate objects.
    pub fn set(&mut self, path: &str, value: Value) {
        let mut segments: Vec<&str> = path.split('.').filter(|part| !part.is_empty()).collect();
        if segments.is_empty() {
            return;
        }
        let last = segments.pop().expect("checked above");
        let mut cursor = &mut self.map;
        for segment in segments {
            let entry = cursor
                .entry(segment.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cursor = entry.as_object_mut().expect("just ensured");
        }
        cursor.insert(last.to_owned(), value);
    }

    /// The underlying map.
    #[must_use]
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.map
    }
}

/// What a policy says when it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEffect {
    /// Grants the targeted permissions.
    Allow,
    /// Refuses them, and wins over an allow of equal priority.
    Deny,
}

impl PolicyEffect {
    /// The stored word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// Parse the stored word.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// `true` for [`PolicyEffect::Deny`].
    #[must_use]
    pub fn is_deny(self) -> bool {
        matches!(self, Self::Deny)
    }
}

/// One policy, in the shape the decision needs (the database row carries more).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Policy id — the source a decision reports.
    pub id: Uuid,
    /// Name the reader sees.
    pub name: String,
    /// What it does when it applies.
    pub effect: PolicyEffect,
    /// Higher wins; equal priority resolves to deny.
    pub priority: i32,
    /// Disabled policies are never evaluated.
    pub enabled: bool,
    /// Permission keys (exact or with `*` wildcards) the policy speaks about.
    pub target_permissions: Vec<String>,
    /// The condition tree.
    pub conditions: Condition,
}

/// The policy that decided an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyMatch {
    /// Id of the policy.
    pub policy_id: Uuid,
    /// Its name.
    pub policy_name: String,
    /// What it said.
    pub effect: PolicyEffect,
    /// The priority it won with.
    pub priority: i32,
}

/// One policy, seen by the dry run and the simulator: whether it targets the permission and
/// whether its conditions hold.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyCandidate {
    /// Policy id.
    pub policy_id: Uuid,
    /// Its name.
    pub policy_name: String,
    /// Nothing
    pub effect: PolicyEffect,
    /// Its priority.
    pub priority: i32,
    /// Whether it is enabled at all.
    pub enabled: bool,
    /// Whether its `target_permissions` cover the permission in question.
    pub targeted: bool,
    /// Whether its conditions hold for the attributes in question.
    pub satisfied: bool,
}

impl PolicyCandidate {
    /// `true` when this policy decides the answer (enabled, targeted and satisfied).
    #[must_use]
    pub fn applies(&self) -> bool {
        self.enabled && self.targeted && self.satisfied
    }
}

/// A set of policies, ready to answer.
#[derive(Debug, Clone, Default)]
pub struct PolicySet {
    policies: Vec<Policy>,
}

impl PolicySet {
    /// Wrap the policies of one organization.
    #[must_use]
    pub fn new(policies: Vec<Policy>) -> Self {
        Self { policies }
    }

    /// How many policies were loaded (enabled or not).
    #[must_use]
    pub fn len(&self) -> usize {
        self.policies.len()
    }

    /// `true` when there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Does one policy speak about this permission, condition included?
    #[must_use]
    pub fn matches(policy: &Policy, permission: &str, attributes: &Attributes) -> bool {
        policy.enabled
            && policy
                .target_permissions
                .iter()
                .any(|pattern| targets(pattern, permission))
            && policy.conditions.satisfied(attributes)
    }

    /// The policy that decides, or `None` when the set says nothing about this request.
    ///
    /// Ordering: higher priority first, then `deny` before `allow`, then the name — so a tie is
    /// never decided by insertion order.
    #[must_use]
    pub fn decide(&self, permission: &str, attributes: &Attributes) -> Option<PolicyMatch> {
        let mut candidates: Vec<&Policy> = self
            .policies
            .iter()
            .filter(|policy| Self::matches(policy, permission, attributes))
            .collect();

        candidates.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| right.effect.is_deny().cmp(&left.effect.is_deny()))
                .then_with(|| left.name.cmp(&right.name))
        });

        candidates.first().map(|policy| PolicyMatch {
            policy_id: policy.id,
            policy_name: policy.name.clone(),
            effect: policy.effect,
            priority: policy.priority,
        })
    }

    /// Every policy with its target and condition verdict — the dry run's and simulator's list.
    #[must_use]
    pub fn candidates(&self, permission: &str, attributes: &Attributes) -> Vec<PolicyCandidate> {
        let mut candidates: Vec<PolicyCandidate> = self
            .policies
            .iter()
            .map(|policy| PolicyCandidate {
                policy_id: policy.id,
                policy_name: policy.name.clone(),
                effect: policy.effect,
                priority: policy.priority,
                enabled: policy.enabled,
                targeted: policy
                    .target_permissions
                    .iter()
                    .any(|pattern| targets(pattern, permission)),
                satisfied: policy.conditions.satisfied(attributes),
            })
            .collect();

        candidates.sort_by(|left, right| {
            right
                .applies()
                .cmp(&left.applies())
                .then_with(|| right.priority.cmp(&left.priority))
                .then_with(|| left.policy_name.cmp(&right.policy_name))
        });
        candidates
    }
}

/// Does a target pattern cover a permission key?
///
/// An exact key matches itself; `*` stands for any run of characters, so `content.*` covers
/// `content.pages.publish` and `*` covers everything.
#[must_use]
pub fn targets(pattern: &str, permission: &str) -> bool {
    if pattern == permission {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    wildcard_match(pattern, permission)
}

/// Glob matching with a single wildcard character.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pattern_at, mut text_at) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;

    while text_at < text.len() {
        if pattern_at < pattern.len() && pattern[pattern_at] == text[text_at] {
            pattern_at += 1;
            text_at += 1;
        } else if pattern_at < pattern.len() && pattern[pattern_at] == '*' {
            star = Some(pattern_at);
            mark = text_at;
            pattern_at += 1;
        } else if let Some(star_at) = star {
            pattern_at = star_at + 1;
            mark += 1;
            text_at = mark;
        } else {
            return false;
        }
    }

    while pattern_at < pattern.len() && pattern[pattern_at] == '*' {
        pattern_at += 1;
    }
    pattern_at == pattern.len()
}

/// Check one target pattern against the permission catalogue.
///
/// An exact key must exist; a wildcard must match at least one known key — a policy that targets
/// nothing would sit in the list and never fire, which reads as a bug.
pub fn validate_target(pattern: &str, known: &[&str]) -> Result<(), String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("a target permission is required".to_owned());
    }
    if !pattern.contains('*') {
        return if known.contains(&pattern) {
            Ok(())
        } else {
            Err(format!("unknown permission {pattern:?}"))
        };
    }
    if known.iter().any(|key| targets(pattern, key)) {
        Ok(())
    } else {
        Err(format!("{pattern:?} matches no known permission"))
    }
}

/// One leaf of a condition tree, with what it resolved to — the builder's `Test` highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct LeafTrace {
    /// Where the leaf sits in the tree (`all[0].any[1]`).
    pub path: String,
    /// The attribute path.
    pub attribute: String,
    /// The operator, as stored.
    pub operator: String,
    /// What the policy expected.
    pub expected: Value,
    /// What the request carried (null when the attribute is absent).
    pub resolved: Value,
    /// Whether the leaf held.
    pub satisfied: bool,
}

/// Flatten a condition tree into its leaves with their verdicts.
#[must_use]
pub fn trace(condition: &Condition, attributes: &Attributes) -> Vec<LeafTrace> {
    let mut out = Vec::new();
    walk(condition, "", attributes, &mut out);
    out
}

fn walk(node: &Condition, path: &str, attributes: &Attributes, out: &mut Vec<LeafTrace>) {
    match node {
        Condition::All(children) => {
            for (index, child) in children.iter().enumerate() {
                walk(child, &format!("{path}/all[{index}]"), attributes, out);
            }
        }
        Condition::Any(children) => {
            for (index, child) in children.iter().enumerate() {
                walk(child, &format!("{path}/any[{index}]"), attributes, out);
            }
        }
        Condition::Not(child) => walk(child, &format!("{path}/not"), attributes, out),
        Condition::Leaf {
            attribute,
            operator,
            value,
        } => {
            let resolved = attributes.get(attribute);
            out.push(LeafTrace {
                path: path.trim_start_matches('/').to_owned(),
                attribute: attribute.clone(),
                operator: operator.as_str().to_owned(),
                expected: value.clone(),
                satisfied: operator.apply(&resolved, value),
                resolved,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attributes(value: Value) -> Attributes {
        Attributes::from_json(&value)
    }

    fn policy(
        effect: PolicyEffect,
        priority: i32,
        targets: &[&str],
        conditions: Condition,
    ) -> Policy {
        Policy {
            id: Uuid::new_v4(),
            name: format!("{} policy", effect.as_str()),
            effect,
            priority,
            enabled: true,
            target_permissions: targets.iter().map(|key| (*key).to_owned()).collect(),
            conditions,
        }
    }

    fn leaf(attribute: &str, operator: Operator, value: Value) -> Condition {
        Condition::Leaf {
            attribute: attribute.to_owned(),
            operator,
            value,
        }
    }

    #[test]
    fn a_missing_attribute_compares_as_null() {
        let attrs = attributes(json!({"user": {"plan": "pro"}}));

        assert_eq!(attrs.get("user.plan"), json!("pro"));
        assert_eq!(attrs.get("user.missing"), Value::Null);
        assert_eq!(attrs.get("nothing.at.all"), Value::Null);

        // Null == null, and anything compared against null is not equal to it.
        assert!(leaf("user.team", Operator::Equals, Value::Null).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::Equals, json!("platform")).satisfied(&attrs));
        // The literal reading: a null left side is not "different from" a value — it *is*
        // different, and `!=` says so, while `>` is simply false for it.
        assert!(leaf("user.team", Operator::NotEquals, json!("platform")).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::GreaterThan, json!(3)).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::LessThan, json!(3)).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::In, json!(["a"])).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::StartsWith, json!("a")).satisfied(&attrs));
        assert!(!leaf("user.team", Operator::Contains, json!("a")).satisfied(&attrs));
    }

    #[test]
    fn every_operator_behaves() {
        let attrs = attributes(json!({
            "action": "content.pages.publish",
            "resource": {"path": "/blog/hello", "site_id": "11111111-1111-1111-1111-111111111111"},
            "user": {"level": 7, "roles": ["editor", "reviewer"], "name": "Furkan"},
        }));

        assert!(leaf("action", Operator::Equals, json!("content.pages.publish")).satisfied(&attrs));
        assert!(!leaf("action", Operator::Equals, json!("content.pages.delete")).satisfied(&attrs));
        assert!(
            leaf("action", Operator::NotEquals, json!("content.pages.delete")).satisfied(&attrs)
        );
        assert!(leaf("user.level", Operator::GreaterThan, json!(5)).satisfied(&attrs));
        assert!(leaf("user.level", Operator::LessThan, json!(10)).satisfied(&attrs));
        assert!(!leaf("user.level", Operator::GreaterThan, json!(10)).satisfied(&attrs));
        assert!(
            leaf(
                "action",
                Operator::In,
                json!(["content.pages.read", "content.pages.publish"])
            )
            .satisfied(&attrs)
        );
        assert!(leaf("resource.path", Operator::StartsWith, json!("/blog")).satisfied(&attrs));
        assert!(!leaf("resource.path", Operator::StartsWith, json!("/legal")).satisfied(&attrs));
        assert!(leaf("resource.path", Operator::Contains, json!("blog")).satisfied(&attrs));
        assert!(leaf("user.roles", Operator::Contains, json!("editor")).satisfied(&attrs));
        assert!(!leaf("user.roles", Operator::Contains, json!("owner")).satisfied(&attrs));
        // A string is compared as a number only when it is one.
        assert!(!leaf("user.name", Operator::GreaterThan, json!(1)).satisfied(&attrs));
    }

    #[test]
    fn and_or_not_group_the_leaves() {
        let attrs = attributes(json!({"user": {"plan": "pro", "country": "TR"}}));

        let both = Condition::All(vec![
            leaf("user.plan", Operator::Equals, json!("pro")),
            leaf("user.country", Operator::Equals, json!("TR")),
        ]);
        assert!(both.satisfied(&attrs));

        let either = Condition::Any(vec![
            leaf("user.plan", Operator::Equals, json!("free")),
            leaf("user.country", Operator::Equals, json!("TR")),
        ]);
        assert!(either.satisfied(&attrs));

        let neither = Condition::Any(vec![
            leaf("user.plan", Operator::Equals, json!("free")),
            leaf("user.country", Operator::Equals, json!("DE")),
        ]);
        assert!(!neither.satisfied(&attrs));

        let negated = Condition::Not(Box::new(leaf("user.plan", Operator::Equals, json!("free"))));
        assert!(negated.satisfied(&attrs));

        // An empty `all` holds (a policy without conditions), an empty `any` never does.
        assert!(Condition::empty().satisfied(&attrs));
        assert!(!Condition::Any(Vec::new()).satisfied(&attrs));
    }

    #[test]
    fn conditions_round_trip_through_their_stored_shape() {
        let stored = json!({"all": [
            {"attribute": "resource.path", "operator": "starts_with", "value": "/blog"},
            {"any": [
                {"attribute": "user.level", "operator": ">", "value": 3},
                {"not": {"attribute": "user.blocked", "operator": "==", "value": true}}
            ]}
        ]});

        let parsed = Condition::parse(&stored).expect("the stored shape parses");
        assert_eq!(parsed.to_json(), stored, "a round trip changes nothing");

        // The table's own default.
        let default = Condition::parse(&json!({"all": []})).expect("the default parses");
        assert_eq!(default, Condition::empty());
    }

    #[test]
    fn a_malformed_condition_is_refused_with_a_sentence() {
        for (value, needle) in [
            (json!("nonsense"), "must be an object"),
            (json!({"all": 3}), "`all` must be an array"),
            (json!({"any": {}}), "`any` must be an array"),
            (
                json!({"attribute": "", "operator": "==", "value": 1}),
                "non-empty attribute",
            ),
            (
                json!({"attribute": "a", "operator": "~", "value": 1}),
                "unknown operator",
            ),
            (json!({"attribute": "a", "operator": "=="}), "needs a value"),
            (
                json!({"wat": 1}),
                "`all`, `any`, `not` or an attribute comparison",
            ),
        ] {
            let error = Condition::parse(&value).expect_err("must be refused");
            assert!(error.contains(needle), "{value} → {error}");
        }
    }

    #[test]
    fn targets_cover_exact_keys_and_wildcards() {
        assert!(targets("content.pages.read", "content.pages.read"));
        assert!(!targets("content.pages.read", "content.pages.write"));
        assert!(targets("content.*", "content.pages.publish"));
        assert!(targets("content.*.publish", "content.pages.publish"));
        assert!(!targets("content.*.publish", "media.pages.publish"));
        assert!(targets("*", "anything.at.all"));
        assert!(targets("iam.*", "iam.sessions.revoke"));
        assert!(!targets("iam.*", "users.read"));
    }

    #[test]
    fn validate_target_names_the_problem() {
        let known = ["content.pages.read", "content.pages.publish", "media.read"];

        assert!(validate_target("content.pages.read", &known).is_ok());
        assert!(validate_target("content.*", &known).is_ok());
        assert!(validate_target("media.*", &known).is_ok());
        assert!(validate_target("", &known).is_err());
        assert!(validate_target("nope.nothing", &known).is_err());
        assert!(
            validate_target("iam.*", &known).is_err(),
            "a wildcard must match something"
        );
    }

    #[test]
    fn the_set_decides_by_priority_then_deny() {
        let attrs = attributes(json!({"action": "content.pages.publish"}));
        let allow = policy(
            PolicyEffect::Allow,
            500,
            &["content.pages.publish"],
            Condition::empty(),
        );
        let deny = policy(
            PolicyEffect::Deny,
            500,
            &["content.pages.*"],
            Condition::empty(),
        );
        let set = PolicySet::new(vec![allow.clone(), deny.clone()]);

        let decided = set
            .decide("content.pages.publish", &attrs)
            .expect("a decision");
        assert_eq!(
            decided.effect,
            PolicyEffect::Deny,
            "at equal priority deny wins"
        );
        assert_eq!(decided.priority, 500);

        // A higher-priority allow takes it back.
        let mut stronger = allow.clone();
        stronger.priority = 800;
        let set = PolicySet::new(vec![stronger, deny.clone()]);
        assert_eq!(
            set.decide("content.pages.publish", &attrs)
                .expect("a decision")
                .effect,
            PolicyEffect::Allow
        );

        // And a policy that does not speak about the permission is not consulted at all.
        assert!(set.decide("media.read", &attrs).is_none());
    }

    #[test]
    fn a_disabled_policy_never_decides() {
        let attrs = attributes(json!({}));
        let mut disabled = policy(PolicyEffect::Deny, 900, &["content.*"], Condition::empty());
        disabled.enabled = false;
        let allow = policy(PolicyEffect::Allow, 100, &["content.*"], Condition::empty());

        let set = PolicySet::new(vec![disabled, allow]);
        assert_eq!(
            set.decide("content.pages.read", &attrs)
                .expect("a decision")
                .effect,
            PolicyEffect::Allow
        );
    }

    #[test]
    fn a_policy_without_conditions_covers_everything_it_targets() {
        let attrs = attributes(json!({"resource": {"path": "/anything"}}));
        let set = PolicySet::new(vec![policy(
            PolicyEffect::Deny,
            600,
            &["media.*"],
            Condition::empty(),
        )]);

        assert!(set.decide("media.read", &attrs).is_some());
        assert!(set.decide("media.delete", &attrs).is_some());
        assert!(set.decide("content.pages.read", &attrs).is_none());
    }

    #[test]
    fn conditions_gate_the_target() {
        let set = PolicySet::new(vec![policy(
            PolicyEffect::Deny,
            700,
            &["content.pages.*"],
            leaf("resource.path", Operator::StartsWith, json!("/legal")),
        )]);

        let legal = attributes(json!({"resource": {"path": "/legal/terms"}}));
        let blog = attributes(json!({"resource": {"path": "/blog/hello"}}));

        assert!(set.decide("content.pages.read", &legal).is_some());
        assert!(
            set.decide("content.pages.read", &blog).is_none(),
            "the condition does not hold, so the policy stays silent"
        );
    }

    #[test]
    fn candidates_report_why_a_policy_stayed_quiet() {
        let attrs = attributes(json!({"resource": {"path": "/blog"}}));
        let set = PolicySet::new(vec![
            policy(
                PolicyEffect::Deny,
                700,
                &["content.*"],
                leaf("resource.path", Operator::StartsWith, json!("/legal")),
            ),
            policy(
                PolicyEffect::Allow,
                400,
                &["content.pages.read"],
                Condition::empty(),
            ),
        ]);

        let candidates = set.candidates("content.pages.read", &attrs);
        let denied = candidates
            .iter()
            .find(|entry| entry.effect.is_deny())
            .expect("the deny is listed");
        assert!(denied.targeted && !denied.satisfied && !denied.applies());

        let allowed = candidates
            .iter()
            .find(|entry| !entry.effect.is_deny())
            .expect("the allow is listed");
        assert!(allowed.applies());
        assert_eq!(
            candidates[0].policy_id, allowed.policy_id,
            "applying policies come first"
        );
    }

    #[test]
    fn the_trace_names_every_leaf_and_its_verdict() {
        let condition = Condition::All(vec![
            leaf("user.plan", Operator::Equals, json!("pro")),
            Condition::Any(vec![leaf("user.level", Operator::GreaterThan, json!(5))]),
        ]);
        let attrs = attributes(json!({"user": {"plan": "free"}}));

        let leaves = trace(&condition, &attrs);
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].path, "all[0]");
        assert_eq!(leaves[0].attribute, "user.plan");
        assert_eq!(leaves[0].resolved, json!("free"));
        assert!(!leaves[0].satisfied);
        assert_eq!(leaves[1].path, "all[1]/any[0]");
        assert_eq!(
            leaves[1].resolved,
            Value::Null,
            "the absent level resolves as null"
        );
        assert!(!leaves[1].satisfied);
    }

    #[test]
    fn attribute_paths_set_and_read_back() {
        let mut attrs = Attributes::default();
        attrs.set("resource.site_id", json!("abc"));
        attrs.set("action", json!("content.pages.read"));

        assert_eq!(attrs.get("resource.site_id"), json!("abc"));
        assert_eq!(attrs.get("action"), json!("content.pages.read"));
        assert_eq!(attrs.get("resource"), json!({"site_id": "abc"}));
        // A path through a leaf that is not an object reads as null rather than panicking.
        assert_eq!(attrs.get("action.deeper"), Value::Null);
    }
}
