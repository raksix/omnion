//! Expression preview for the canvas (REQ-086 slice 3, REQ-092's evaluator surface).
//!
//! The canvas shows a person what a node's parameter *evaluates to* before the workflow is
//! saved, and the REQ is explicit about where that answer comes from: **server-side only**.
//! Two copies of an evaluator is the defect this module exists to prevent — a client that
//! reimplemented the rules would agree with the server on the day it was written and drift
//! the first time a rule changed, which is the day somebody is relying on the preview to
//! decide whether a step will do what they meant.
//!
//! So this is a *preview* evaluator, not a second execution engine, and the difference is
//! load-bearing:
//!
//! * it resolves against **pinned sample data** the caller supplies, never live data, so
//!   previewing a step cannot read a credential, call an integration or cost money;
//! * it has no side effects by construction — there is nothing to write to, so there is no
//!   dry-run flag to forget to set;
//! * every failure is a *typed refusal* naming what the caller can fix, because a preview
//!   whose error says `invalid` teaches the reader nothing.
//!
//! The grammar is deliberately the narrow one `{{ }}` already uses for bindings (see
//! `omnion_automation::binding`): an expression is a path rooted at a namespace, with an
//! optional `!field` filter and a `|| fallback`. No arithmetic, no function calls, no
//! reaching outside the namespaces the caller passed in. An expression language is REQ-092's
//! deliverable; this is the half of it the editor can hold you to today.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Opening of an expression.
const OPEN: &str = "{{";

/// Closing of an expression.
const CLOSE: &str = "}}";

/// Longest expression a field may carry.
const MAX_EXPRESSION: usize = 200;

/// How many paths a refusal names, so a long sample does not become a wall of text.
const PATHS_IN_MESSAGE: usize = 10;

/// Why a preview could not be produced.
///
/// A string would have been shorter to write and worse to read: the canvas's whole job at
/// this point is to say *which* of the three possible problems it is — a malformed
/// expression, a namespace the caller did not supply, or a path the sample does not carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PreviewError {
    /// The braces do not pair, or the expression inside is empty or too long.
    Malformed {
        /// The field this was found in, for the message the inspector prints.
        field: String,
        /// What is wrong, in one sentence.
        detail: String,
    },
    /// The expression names a namespace the caller did not pass sample data for.
    UnknownNamespace {
        /// The namespace the expression reached for.
        namespace: String,
        /// What the caller did pass, sorted.
        available: Vec<String>,
    },
    /// The namespace exists but the sample carries no such path.
    UnknownPath {
        /// The field this was found in. The route's contract is that a refusal names the
        /// parameter it belongs to, and this variant is the most likely one to reach a
        /// person mid-typing — which is exactly when not knowing which row is wrong is
        /// most expensive.
        field: String,
        /// The full expression, as written.
        expression: String,
        /// The path within the namespace that is not there.
        path: String,
        /// Some of the paths the sample *does* carry.
        available: Vec<String>,
    },
    /// The expression is well-formed but its grammar is narrower than what was written —
    /// a call, an arithmetic operator, a `?` conditional.
    UnsupportedSyntax {
        /// The offending fragment, so the reader knows which part to rewrite.
        fragment: String,
    },
}

impl PreviewError {
    /// A short sentence for the inspector, naming the field where there is one.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Malformed { field, detail } => {
                format!("{field}: {detail}")
            }
            Self::UnknownNamespace {
                namespace,
                available,
            } => format!(
                "`{namespace}` has no sample data here; pass it as a namespace or use one of: {}",
                listing(available)
            ),
            Self::UnknownPath {
                field,
                expression,
                path,
                available,
            } => format!(
                "{field}: {expression} cannot be read: the sample carries no `{path}` under \
                 its namespace. It carries: {}",
                listing(available)
            ),
            Self::UnsupportedSyntax { fragment } => format!(
                "`{fragment}` is not part of the expression grammar yet — a path, an \
                 optional `!field` and a `|| fallback` are what resolve today"
            ),
        }
    }
}

/// Render a list for a message, bounded.
fn listing(items: &[String]) -> String {
    if items.is_empty() {
        return "nothing".to_owned();
    }
    let shown = items.len().min(PATHS_IN_MESSAGE);
    let mut text = items[..shown].join(", ");
    if items.len() > shown {
        text.push_str(&format!(" ({} more)", items.len() - shown));
    }
    text
}

/// The namespaces a preview may read, keyed by the name an expression reaches for.
///
/// A `Map` rather than a struct because the set of namespaces grows with the node families
/// (REQ-088) and a struct would make every one of them a change to a type the preview owns.
pub type Namespaces = Map<String, Value>;

/// What a preview produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// The field this was for.
    pub field: String,
    /// The value the expression resolved to, with its own type preserved.
    pub value: Value,
    /// The expression as written, echoed so the caller can render it next to the answer.
    pub expression: String,
    /// Every expression in the field, in order. A field mixing text and one placeholder has
    /// more than one, and the inspector shows the whole rendered string.
    pub rendered: String,
    /// `true` when the field is a lone expression, so the value keeps its type rather than
    /// being flattened into text.
    pub typed: bool,
}

/// Preview one parameter value against sample data, storing nothing.
///
/// A value that is not a string, or a string with no placeholder in it, comes back
/// unchanged and *without* a `Preview` — there is nothing to evaluate, and inventing a
/// preview for a constant would put a row in the inspector that says nothing.
///
/// # Errors
///
/// Returns the first [`PreviewError`] in the field's own order, so a parameter with two
/// problems is reported on the first one rather than on whichever the iterator happened to
/// reach last.
pub fn preview_value(
    field: &str,
    value: &Value,
    namespaces: &Namespaces,
) -> Result<Option<Preview>, PreviewError> {
    let Value::String(text) = value else {
        return Ok(None);
    };
    if !text.contains(OPEN) {
        return Ok(None);
    }

    // Resolve into VALUES, not into strings, and render only at the end. Rendering as it
    // goes throws away the one fact the caller most needs — that `{{node.count}}` is the
    // number 2 and not the text "2" — and recovering it afterwards means resolving the
    // expression a second time through a second code path, which is how a preview starts
    // disagreeing with itself.
    //
    // The segments are kept IN ORDER, literal and expression interleaved, rather than
    // collected into two lists. Two lists produce "By :  itemsada2" for
    // "By {{…}}: {{…}} items": every literal is correct on its own and the sentence is
    // nonsense, which no assertion on a single expression would have caught.
    enum Segment {
        Literal(String),
        Expression(Value, Option<String>),
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut rest = text.as_str();

    while let Some(start) = rest.find(OPEN) {
        if start > 0 {
            segments.push(Segment::Literal(rest[..start].to_owned()));
        }
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else {
            return Err(PreviewError::Malformed {
                field: field.to_owned(),
                detail: "an expression is never closed".to_owned(),
            });
        };
        let expression = after[..end].trim();
        if expression.is_empty() {
            return Err(PreviewError::Malformed {
                field: field.to_owned(),
                detail: "an expression is empty".to_owned(),
            });
        }
        if expression.len() > MAX_EXPRESSION {
            return Err(PreviewError::Malformed {
                field: field.to_owned(),
                detail: format!("an expression is at most {MAX_EXPRESSION} characters"),
            });
        }

        let (body, fallback) = split_fallback(expression);
        segments.push(Segment::Expression(
            resolve_one(field, body, namespaces)?.clone(),
            fallback,
        ));
        rest = &after[end + CLOSE.len()..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest.to_owned()));
    }

    let expressions = segments
        .iter()
        .filter(|segment| matches!(segment, Segment::Expression(_, _)))
        .count();
    if expressions == 0 {
        return Err(PreviewError::Malformed {
            field: field.to_owned(),
            detail: "no expression between the braces".to_owned(),
        });
    }

    // A field that is ONE expression and nothing else is typed: the value keeps its own
    // shape. A fallback does not change that — the fallback is text the author wrote, and
    // the value it stands in for is unknown until the run.
    let lone = expressions == 1
        && segments.iter().all(|segment| match segment {
            Segment::Literal(text) => text.trim().is_empty(),
            Segment::Expression(_, _) => true,
        });
    let rendered = if lone {
        match &segments[0] {
            // `render` and not `display`: a lone expression with a fallback is still an
            // expression that may have come back null, and the preview that answers "" is
            // the one thing the fallback exists to prevent.
            Segment::Expression(value, fallback) => render(value, fallback.as_deref()),
            // `lone` requires exactly one expression and this is the only other shape,
            // so the arm is unreachable — and the `else` is still correct if it ever is.
            Segment::Literal(text) => text.clone(),
        }
    } else {
        let mut spliced = String::new();
        for segment in &segments {
            spliced.push_str(&match segment {
                Segment::Literal(text) => text.clone(),
                Segment::Expression(value, fallback) => render(value, fallback.as_deref()),
            });
        }
        spliced
    };

    let value = if lone {
        match &segments[0] {
            Segment::Expression(value, _) => value.clone(),
            Segment::Literal(text) => Value::String(text.clone()),
        }
    } else {
        Value::String(rendered.clone())
    };

    Ok(Some(Preview {
        field: field.to_owned(),
        value,
        expression: expression_list(text).join(", "),
        rendered,
        typed: lone,
    }))
}

/// Split `a.b || fallback` into its two halves.
fn split_fallback(expression: &str) -> (&str, Option<String>) {
    match expression.split_once("||") {
        Some((body, fallback)) => (body.trim(), Some(fallback.trim().to_owned())),
        None => (expression.trim(), None),
    }
}

/// Render one resolved value inside a field that also carries other text.
///
/// The fallback applies to exactly one case — a value that is absent rather than empty —
/// and the distinction matters: `0` and `false` are answers, and replacing them with the
/// fallback would make a count of zero read as "no data".
fn render(value: &Value, fallback: Option<&str>) -> String {
    let text = display(value);
    if value.is_null() {
        return fallback.unwrap_or_default().to_owned();
    }
    text
}

/// Render a value as the text it will be inside a string.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Resolve one expression body against the namespaces.
///
/// The order of the two refusals is the substance here. Syntax is checked FIRST, on the
/// text alone, before any lookup: `{{node.count + 1}}` read as a path is a perfectly
/// plausible key that the sample happens not to carry, so a shape-first check reports it
/// as an unknown *path* — "the sample carries no `count + 1`" — which is a confident and
/// completely wrong answer to a question about arithmetic. A syntax rule that runs after
/// the lookup only ever fires on the expressions that also happen to be missing.
fn resolve_one<'a>(
    field: &str,
    body: &str,
    namespaces: &'a Namespaces,
) -> Result<&'a Value, PreviewError> {
    let body = body.strip_prefix('!').unwrap_or(body);

    for fragment in [
        "+", "-", "*", "/", "%", "|", "?", "&", "=>", "(", ")", "[", "]", "$", "@", "#",
    ] {
        if body.contains(fragment) {
            return Err(PreviewError::UnsupportedSyntax {
                fragment: fragment.to_owned(),
            });
        }
    }

    // A namespace on its own is a shape problem, not a missing key, so it is refused
    // BEFORE the lookup: `{{node}}` under a namespace that does not exist should not
    // report an unknown namespace, and `{{node}}` under one that does should not splice
    // the whole sample into a field. The empty-segment check is the same rule one step
    // along: `{{a.}}` is a typo, and reading it as "namespace `a`" turns a spelling slip
    // into a confident claim about which namespaces exist.
    let path: Vec<&str> = body.split('.').map(str::trim).collect();
    let Some(namespace) = path.first().copied() else {
        return Err(PreviewError::Malformed {
            field: field.to_owned(),
            detail: "an expression names no namespace".to_owned(),
        });
    };
    if namespace.is_empty() {
        return Err(PreviewError::Malformed {
            field: field.to_owned(),
            detail: format!("`{body}` names no namespace"),
        });
    }
    if path.len() < 2 {
        return Err(PreviewError::Malformed {
            field: field.to_owned(),
            detail: format!("`{body}` names no field; write it as `{namespace}.field`"),
        });
    }
    if path.iter().any(|segment| segment.is_empty()) {
        return Err(PreviewError::Malformed {
            field: field.to_owned(),
            detail: format!("`{body}` has an empty path segment"),
        });
    }

    let root = namespaces.get(namespace).ok_or_else(|| {
        let mut available: Vec<String> = namespaces.keys().cloned().collect();
        available.sort_unstable();
        PreviewError::UnknownNamespace {
            namespace: namespace.to_owned(),
            available,
        }
    })?;

    let mut current = root;
    for segment in &path[1..] {
        // `Value::get` on an array only answers a *string* key with `None`, so
        // `{{node.items.0.title}}` would be reported as a path the sample does not carry
        // when what it means is "index 0 of items". An index is the one non-key segment
        // the grammar allows, so it is resolved here rather than by pretending the
        // sample is keyed by position.
        current = match current {
            Value::Array(items) => {
                let index: usize = segment.parse().map_err(|_| PreviewError::UnknownPath {
                    field: field.to_owned(),
                    expression: format!("{{{{{body}}}}}"),
                    path: path[1..].join("."),
                    available: paths_of(root),
                })?;
                items.get(index).ok_or_else(|| PreviewError::UnknownPath {
                    field: field.to_owned(),
                    expression: format!("{{{{{body}}}}}"),
                    path: path[1..].join("."),
                    available: paths_of(root),
                })?
            }
            other => other
                .get(*segment)
                .ok_or_else(|| PreviewError::UnknownPath {
                    field: field.to_owned(),
                    expression: format!("{{{{{body}}}}}"),
                    path: path[1..].join("."),
                    available: paths_of(root),
                })?,
        };
    }

    Ok(current)
}

/// Every leaf path a sample value carries, for a refusal message.
#[must_use]
pub fn paths_of(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk_paths(value, "", &mut found);
    found.sort_unstable();
    found
}

fn walk_paths(value: &Value, prefix: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                found.push(path.clone());
                walk_paths(item, &path, found);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let path = if prefix.is_empty() {
                    index.to_string()
                } else {
                    format!("{prefix}.{index}")
                };
                found.push(path.clone());
                walk_paths(item, &path, found);
            }
        }
        _ => {}
    }
}

/// Every expression in a string, in order.
fn expression_list(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else {
            break;
        };
        found.push(after[..end].trim().to_owned());
        rest = &after[end + CLOSE.len()..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn namespaces() -> Namespaces {
        let mut map = Namespaces::new();
        map.insert(
            "node".to_owned(),
            json!({
                "items": [{ "title": "First" }, { "title": "Second" }],
                "count": 2,
                "author": { "name": "ada", "email": "ada@example.com" },
                "summary": null
            }),
        );
        map.insert("vars".to_owned(), json!({ "site": "example.com" }));
        map
    }

    fn run(field: &str, text: &str) -> Result<Option<Preview>, PreviewError> {
        preview_value(field, &Value::String(text.to_owned()), &namespaces())
    }

    #[test]
    fn a_lone_expression_keeps_its_type() {
        let preview = run("count", "{{node.count}}")
            .expect("resolves")
            .expect("a preview");
        assert_eq!(preview.value, json!(2), "a number stays a number");
        assert!(preview.typed, "a lone expression is typed");
        assert_eq!(preview.rendered, "2");
    }

    #[test]
    fn a_nested_path_resolves() {
        let preview = run("to", "{{node.author.email}}")
            .expect("resolves")
            .expect("preview");
        assert_eq!(preview.value, json!("ada@example.com"));
    }

    #[test]
    fn an_array_index_resolves() {
        let preview = run("title", "{{node.items.0.title}}")
            .expect("resolves")
            .expect("preview");
        assert_eq!(preview.value, json!("First"));
    }

    #[test]
    fn mixed_text_renders_every_expression() {
        let preview = run("subject", "By {{node.author.name}}: {{node.count}} items")
            .expect("resolves")
            .expect("preview");
        assert_eq!(preview.rendered, "By ada: 2 items");
        assert!(!preview.typed, "mixed text is text");
        assert_eq!(
            preview.value,
            json!("By ada: 2 items"),
            "a mixed field previews as the string it will be"
        );
    }

    #[test]
    fn a_fallback_replaces_a_null() {
        let preview = run("summary", "{{node.summary || nothing yet}}")
            .expect("resolves")
            .expect("preview");
        assert_eq!(preview.rendered, "nothing yet");
    }

    #[test]
    fn a_fallback_does_not_replace_a_zero_or_a_false() {
        // `0` and `false` are answers. A fallback that swallowed them would make a count
        // of zero read as "no data" — the exact bug a preview is consulted to avoid.
        let preview = run("count", "{{node.count || none}}")
            .expect("resolves")
            .expect("preview");
        assert_eq!(preview.rendered, "2", "a zero count is not an absent value");
        assert_eq!(preview.value, json!(2));
    }

    #[test]
    fn a_constant_field_previews_nothing() {
        assert!(
            run("subject", "a literal string")
                .expect("resolves")
                .is_none(),
            "there is nothing to evaluate, so there is no preview row"
        );
        assert!(
            preview_value("retries", &json!(3), &namespaces())
                .expect("resolves")
                .is_none(),
            "a number is not an expression"
        );
    }

    #[test]
    fn an_unclosed_expression_is_named() {
        let error = run("subject", "{{node.title").expect_err("refused");
        assert_eq!(error.message(), "subject: an expression is never closed");
    }

    #[test]
    fn an_empty_expression_is_named() {
        let error = run("subject", "{{}}").expect_err("refused");
        assert_eq!(error.message(), "subject: an expression is empty");
    }

    #[test]
    fn an_unknown_namespace_lists_the_ones_that_exist() {
        let error = run("subject", "{{item.title}}").expect_err("refused");
        match &error {
            PreviewError::UnknownNamespace {
                namespace,
                available,
            } => {
                assert_eq!(namespace, "item");
                assert_eq!(available, &["node".to_owned(), "vars".to_owned()]);
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        let message = error.message();
        assert!(message.contains("node, vars"), "{message}");
    }

    #[test]
    fn an_unknown_path_lists_what_the_sample_carries() {
        let error = run("subject", "{{node.titel}}").expect_err("refused");
        match &error {
            PreviewError::UnknownPath {
                path, available, ..
            } => {
                assert_eq!(path, "titel");
                assert!(
                    available.contains(&"author.name".to_owned()),
                    "{available:?}"
                );
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        let message = error.message();
        assert!(message.contains("author.name"), "{message}");
    }

    #[test]
    fn a_namespace_with_no_field_is_a_mistake_not_a_whole_object() {
        let error = run("subject", "{{node}}").expect_err("refused");
        assert!(
            error.message().contains("names no field"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn syntax_the_grammar_does_not_have_is_refused_by_name() {
        for (text, fragment) in [
            ("{{node.count + 1}}", "+"),
            ("{{node.items | length}}", "|"),
            ("{{node.author.name ?? 'x'}}", "?"),
        ] {
            let error = run("subject", text).expect_err("refused");
            match error {
                PreviewError::UnsupportedSyntax { fragment: found } => assert_eq!(found, fragment),
                other => panic!("{text} gave {other:?}"),
            }
        }
    }

    #[test]
    fn the_expression_cap_is_enforced() {
        let long = format!("{{{{node.{}}}}}", "a".repeat(MAX_EXPRESSION));
        let error = run("subject", &long).expect_err("refused");
        assert!(error.message().contains("at most"), "{}", error.message());
    }

    #[test]
    fn the_two_first_failures_are_named_on_their_own_field() {
        let error =
            preview_value("subject", &json!("{{a.}}\n{{}}"), &namespaces()).expect_err("refused");
        assert!(
            error.message().starts_with("subject:"),
            "{}",
            error.message()
        );
    }
}
