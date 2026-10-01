//! Eval case properties and the scoring of one output against them (REQ-107, slice 1).
//!
//! Everything in this module is pure: an output in, a verdict out. No database, no provider,
//! no route. That is deliberate, and it is the reason this file can carry the request's
//! claim — that every property is provable by a test fixture — without a running server or a
//! network in the picture.
//!
//! # A case is a set of properties, and all of them must hold
//!
//! The request's data model gives a case one `expected` document naming several properties, so
//! the natural implementation is "collect the failures and report them all". A case that names
//! three properties and fails one is a *failing* case, not a partial pass, and the screen has
//! to be able to say which one — so [`CheckResult`] exists as its own value and the run writes
//! one per property, not one per case.
//!
//! # Two properties need a *model*, and the run layer supplies it
//!
//! `rubric` (free text judged by a second model) and `no_pii` (delegating to REQ-105's
//! detector) cannot be decided here without becoming untestable: the first needs a provider, the
//! second needs the installation's patterns. They are therefore *not* in [`DeterministicCheck`]
//! — they are [`judged_check`] / [`guard_check`], resolved by the run layer against real data.
//! What this module does own is the **envelope**: [`expected_property`] validates a stored
//! document, and [`score_output`] takes the judged results as input. A property that cannot be
//! evaluated in-process is a property the run layer must answer for, and saying so by type is
//! what stops a `rubric` case from silently passing because nothing looked at it.
//!
//! # A case that cannot be evaluated is `error`, never `pass`
//!
//! The default for an unknown property is [`CaseStatus::Error`], not a pass. An eval suite whose
//! unknown properties quietly pass is worse than no suite: the pass rate is a number an
//! operator will gate a release on, and this is the one path where a bug in *us* shows up as
//! the *model* looking good.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The property names a case may carry, in the order the case editor shows them.
///
/// A list rather than a free string, because the panel renders a checkbox group from it and a
/// case whose property is spelled `contains_all` instead of `contains` is a case that looks
/// configured and scores nothing.
pub const PROPERTIES: &[&str] = &[
    "exact",
    "contains",
    "regex",
    "json_schema",
    "citations_required",
    "no_pii",
    "max_steps",
    "max_cost_micros",
    "max_latency_ms",
    "rubric",
];

/// The properties this module decides on its own.
///
/// `no_pii` and `rubric` are deliberately absent: they need the installation's guard patterns
/// and a judge model respectively, so the run layer resolves them. See the module docs.
pub const DETERMINISTIC_PROPERTIES: &[&str] = &[
    "exact",
    "contains",
    "regex",
    "json_schema",
    "citations_required",
    "max_steps",
    "max_cost_micros",
    "max_latency_ms",
];

/// How a case ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    /// Every named property held.
    Pass,
    /// The output was produced and at least one property did not hold.
    Fail,
    /// The output could not be produced or a property could not be evaluated.
    Error,
    /// The run was cancelled or the case was disabled after the run started.
    Skipped,
}

impl CaseStatus {
    /// The wire name, for SQL parameters and JSON that a column stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

/// One property's verdict, as the run writes it into `ai_eval_case_results.checks`.
///
/// `detail` is the sentence the panel shows when a row is expanded. It has to say *what was
/// expected* and not merely `expected X, got Y` for a regex — a diff for a ten-line pattern is
/// unreadable, and the operator reading it is deciding whether to block a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Which property this is.
    pub property: String,
    /// Whether it held.
    pub passed: bool,
    /// The sentence shown when the row is expanded.
    pub detail: String,
    /// Whether this check failed because the property *could not be evaluated*, as opposed to
    /// being evaluated and not holding.
    ///
    /// Not serialized: the column shape is the request's `[{property, passed, detail}]`, and
    /// this is a fact about how the verdict was reached rather than about the output. It is
    /// here because the case status depends on it — an unevaluable property makes the case
    /// `error`, and a `fail` is reserved for "the model was measured and did not meet the
    /// bar". Deriving that from `detail` instead (a substring match on the prose) is how a
    /// reworded message silently turns every budget case into a model failure.
    #[serde(skip)]
    pub unevaluated: bool,
}

impl CheckResult {
    /// A passing check.
    fn ok(property: &str, detail: impl Into<String>) -> Self {
        Self {
            property: property.to_string(),
            passed: true,
            detail: detail.into(),
            unevaluated: false,
        }
    }

    /// A failing check: the property was evaluated and did not hold.
    fn no(property: &str, detail: impl Into<String>) -> Self {
        Self {
            property: property.to_string(),
            passed: false,
            detail: detail.into(),
            unevaluated: false,
        }
    }

    /// A failing check whose property could not be evaluated at all.
    fn unevaluated(property: &str, detail: impl Into<String>) -> Self {
        Self {
            property: property.to_string(),
            passed: false,
            detail: detail.into(),
            unevaluated: true,
        }
    }
}

/// What the caller measured while the output was produced.
///
/// Only the fields some property can ask about exist, which is what keeps the scoring call
/// honest: a case may demand `max_latency_ms` and this struct has to carry the measurement or
/// the property is unanswerable.
#[derive(Debug, Clone, Default)]
pub struct Observations {
    /// How many steps the agent took.
    pub steps: Option<i32>,
    /// What the run was charged, in micros.
    pub cost_micros: Option<i64>,
    /// Wall-clock time for this case, in milliseconds.
    pub latency_ms: Option<i64>,
    /// Citation markers found in the output, by the run layer.
    pub citations: Vec<String>,
    /// The tool calls the case made, as the run layer recorded them.
    pub tool_calls: Vec<String>,
}

/// The verdict for one case, with the per-property detail the run row stores.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseVerdict {
    /// Pass / fail / error / skipped.
    pub status: CaseStatus,
    /// The per-property verdicts, in the order the properties were named.
    pub checks: Vec<CheckResult>,
    /// A one-line summary for the case table.
    pub summary: String,
}

impl CaseVerdict {
    /// The fraction of checks that held, `0.0` when nothing was checkable.
    ///
    /// This is the *unweighted* score. The run's `pass_rate` is the weighted share computed by
    /// the run layer over the cases that were executed; keeping the two apart means a case's
    /// own row can show "2 of 3 properties held" without the score it contributes being
    /// confused with it.
    pub fn score(&self) -> f64 {
        if self.checks.is_empty() {
            return 0.0;
        }
        let held = self.checks.iter().filter(|c| c.passed).count();
        held as f64 / self.checks.len() as f64
    }
}

/// A validated case document, so the scorer never re-parses JSON and never guesses.
///
/// Built by [`expected_property`] / [`expectation_from`], which is also where a malformed
/// stored document is reported — as a validation error naming the property, not as a panic and
/// not as a silent "no expectations".
#[derive(Debug, Clone, Default)]
pub struct Expectation {
    exact: Option<String>,
    contains: Vec<String>,
    regex: Vec<String>,
    json_schema: Option<Value>,
    citations_required: bool,
    no_pii: bool,
    max_steps: Option<i32>,
    max_cost_micros: Option<i64>,
    max_latency_ms: Option<i64>,
    rubric: Option<String>,
}

impl Expectation {
    /// Whether the case names a `rubric` property, which is what makes a judge model required.
    pub fn has_rubric(&self) -> bool {
        self.rubric.is_some()
    }

    /// Whether the document named no property at all.
    ///
    /// Such a case is refused at save time. A case that asserts nothing passes every model
    /// including a broken one, so it contributes only false confidence to a pass rate.
    pub fn is_empty(&self) -> bool {
        self.exact.is_none()
            && self.contains.is_empty()
            && self.regex.is_empty()
            && self.json_schema.is_none()
            && !self.citations_required
            && !self.no_pii
            && self.max_steps.is_none()
            && self.max_cost_micros.is_none()
            && self.max_latency_ms.is_none()
            && self.rubric.is_none()
    }

    /// The property names this document carries, in [`PROPERTIES`] order.
    pub fn property_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.exact.is_some() {
            names.push("exact");
        }
        if !self.contains.is_empty() {
            names.push("contains");
        }
        if !self.regex.is_empty() {
            names.push("regex");
        }
        if self.json_schema.is_some() {
            names.push("json_schema");
        }
        if self.citations_required {
            names.push("citations_required");
        }
        if self.no_pii {
            names.push("no_pii");
        }
        if self.max_steps.is_some() {
            names.push("max_steps");
        }
        if self.max_cost_micros.is_some() {
            names.push("max_cost_micros");
        }
        if self.max_latency_ms.is_some() {
            names.push("max_latency_ms");
        }
        if self.rubric.is_some() {
            names.push("rubric");
        }
        names
    }
}

/// Read the expectations out of a stored `expected` document.
///
/// The error names the property, because that is what the case editor's field needs: "expected
/// `regex` is not a valid regular expression" points at the regex box, and "invalid
/// expectations" does not.
pub fn expectation_from(value: &Value) -> crate::error::Result<Expectation> {
    use crate::error::AiHubError;

    let Some(object) = value.as_object() else {
        return Err(AiHubError::InvalidEval(
            "a case's expected properties must be a JSON object".to_string(),
        ));
    };

    let mut out = Expectation::default();

    if let Some(exact) = object.get("exact") {
        let exact = exact.as_str().ok_or_else(|| {
            AiHubError::InvalidEval("expected `exact` to be a string".to_string())
        })?;
        out.exact = Some(exact.to_string());
    }
    for key in ["contains", "regex"] {
        if let Some(value) = object.get(key) {
            let list = value.as_array().ok_or_else(|| {
                AiHubError::InvalidEval(format!("expected `{key}` to be an array of strings"))
            })?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                let text = item.as_str().ok_or_else(|| {
                    AiHubError::InvalidEval(format!("every entry of `{key}` must be a string"))
                })?;
                if key == "regex" {
                    Regex::new(text).map_err(|e| {
                        AiHubError::InvalidEval(format!("expected `regex` entry is invalid: {e}"))
                    })?;
                }
                items.push(text.to_string());
            }
            if key == "contains" {
                out.contains = items;
            } else {
                out.regex = items;
            }
        }
    }
    if let Some(schema) = object.get("json_schema") {
        if !schema.is_object() {
            return Err(AiHubError::InvalidEval(
                "expected `json_schema` to be a JSON schema object".to_string(),
            ));
        }
        out.json_schema = Some(schema.clone());
    }
    for (key, slot) in [
        ("citations_required", &mut out.citations_required),
        ("no_pii", &mut out.no_pii),
    ] {
        if let Some(value) = object.get(key) {
            *slot = value.as_bool().ok_or_else(|| {
                AiHubError::InvalidEval(format!("expected `{key}` to be true or false"))
            })?;
        }
    }
    // The three budget properties share a name and a shape but not a width, and the loop that
    // read them together only compiled because they all happened to be `Option<i32>` in the
    // first draft. `max_cost_micros` is an `i64` on purpose — a per-case cost in micros is
    // 1000× a millisecond figure, and an `i32` ceiling is 2147 dollars of a *single case*,
    // which is a limit an eval suite could actually hit. So they are read one at a time.
    for (key, value) in [
        ("max_steps", object.get("max_steps")),
        ("max_cost_micros", object.get("max_cost_micros")),
        ("max_latency_ms", object.get("max_latency_ms")),
    ] {
        let Some(value) = value else { continue };
        let number = value.as_i64().ok_or_else(|| {
            AiHubError::InvalidEval(format!("expected `{key}` to be a whole number"))
        })?;
        if number < 0 {
            return Err(AiHubError::InvalidEval(format!(
                "expected `{key}` must be zero or more, got {number}"
            )));
        }
        let number = i64::from(number);
        match key {
            "max_steps" => out.max_steps = Some(i32::try_from(number).map_err(|_| {
                AiHubError::InvalidEval("expected `max_steps` to be a step count".to_string())
            })?),
            "max_cost_micros" => out.max_cost_micros = Some(number),
            "max_latency_ms" => out.max_latency_ms = Some(number),
            _ => unreachable!("the three keys above are the three that reach here"),
        }
    }
    if let Some(rubric) = object.get("rubric") {
        let rubric = rubric.as_str().ok_or_else(|| {
            AiHubError::InvalidEval("expected `rubric` to be a string".to_string())
        })?;
        if rubric.trim().is_empty() {
            return Err(AiHubError::InvalidEval("expected `rubric` must not be blank".to_string()));
        }
        if rubric.chars().count() > 8000 {
            return Err(AiHubError::InvalidEval(
                "expected `rubric` must be 8000 characters or fewer".to_string(),
            ));
        }
        out.rubric = Some(rubric.to_string());
    }

    Ok(out)
}

/// A save-time refusal for a single property, so the editor can mark the field.
pub fn expected_property(
    object: &serde_json::Map<String, Value>,
    property: &str,
) -> crate::error::Result<()> {
    if !PROPERTIES.contains(&property) {
        return Err(crate::error::AiHubError::InvalidEval(format!(
            "`{property}` is not a property a case can check; expected one of {}",
            PROPERTIES.join(", ")
        )));
    }
    // Reuse the document reader by handing it this one key, so a single property cannot be
    // valid in isolation and invalid in a document. Two parsers would drift.
    let single = serde_json::Map::from_iter([(property.to_string(), object[property].clone())]);
    expectation_from(&Value::Object(single))?;
    Ok(())
}

/// The outcome of a `no_pii` check, decided by the run layer against the guard's patterns.
#[derive(Debug, Clone)]
pub struct GuardOutcome {
    /// Whether the output is free of values the guard would mask.
    pub clean: bool,
    /// What was found, rendered for the check's `detail` line.
    pub findings: Vec<String>,
}

/// The outcome of a `rubric` check, decided by the judge model in the run layer.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    /// Whether the output met the rubric.
    pub passed: bool,
    /// The judge's reasoning, stored on the case result.
    pub reason: String,
}

/// Score one output against a case's expectations.
///
/// `judge` and `guard` are supplied by the caller because both need state this module does not
/// have — the judge model and the installation's guard patterns. Passing `None` for a case that
/// names the property is **not** a pass: it produces an `error` check naming what was missing,
/// because a case that was never really evaluated must never read as a green row in a release
/// gate.
pub fn score_output(
    expectation: &Expectation,
    output: &str,
    observations: &Observations,
    judge: Option<&JudgeOutcome>,
    guard: Option<&GuardOutcome>,
) -> CaseVerdict {
    let mut checks: Vec<CheckResult> = Vec::new();

    if let Some(exact) = &expectation.exact {
        if output == exact {
            checks.push(CheckResult::ok("exact", "the output matches the expected text"));
        } else {
            checks.push(CheckResult::no(
                "exact",
                format!(
                    "expected exactly {:?}, got {}",
                    exact,
                    preview(output)
                ),
            ));
        }
    }

    for needle in &expectation.contains {
        if output.contains(needle.as_str()) {
            checks.push(CheckResult::ok(
                "contains",
                format!("the output contains {needle:?}"),
            ));
        } else {
            checks.push(CheckResult::no(
                "contains",
                format!("the output does not contain {needle:?}"),
            ));
        }
    }

    for pattern in &expectation.regex {
        // The pattern was compiled at parse time, so a failure here means the string changed
        // underneath us; treating it as a failed check is right — the case asserts something
        // the platform can no longer evaluate.
        match Regex::new(pattern) {
            Ok(re) if re.is_match(output) => {
                checks.push(CheckResult::ok("regex", format!("/{pattern}/ matches the output")));
            }
            Ok(_) => {
                checks.push(CheckResult::no(
                    "regex",
                    format!("/{pattern}/ does not match the output"),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::no(
                    "regex",
                    format!("/{pattern}/ cannot be evaluated: {e}"),
                ));
            }
        }
    }

    if let Some(schema) = &expectation.json_schema {
        match validate_json(output) {
            Ok(value) => {
                let errors = schema_errors(&value, schema);
                if errors.is_empty() {
                    checks.push(CheckResult::ok(
                        "json_schema",
                        "the output parses as JSON and satisfies the schema",
                    ));
                } else {
                    checks.push(CheckResult::no(
                        "json_schema",
                        format!("schema: {}", errors.join("; ")),
                    ));
                }
            }
            Err(e) => {
                checks.push(CheckResult::no(
                    "json_schema",
                    format!("the output is not JSON ({e})"),
                ));
            }
        }
    }

    if expectation.citations_required {
        if observations.citations.is_empty() {
            checks.push(CheckResult::no(
                "citations_required",
                "the output cited nothing",
            ));
        } else {
            checks.push(CheckResult::ok(
                "citations_required",
                format!("the output cited {} source(s)", observations.citations.len()),
            ));
        }
    }

    if expectation.no_pii {
        match guard {
            Some(outcome) if outcome.clean => checks.push(CheckResult::ok(
                "no_pii",
                "the data guard found nothing to mask in the output",
            )),
            Some(outcome) => checks.push(CheckResult::no(
                "no_pii",
                format!("the data guard would mask: {}", outcome.findings.join(", ")),
            )),
            None => checks.push(CheckResult::unevaluated(
                "no_pii",
                "the `no_pii` check needs the installation's guard patterns, which the run \
                 layer did not supply",
            )),
        }
    }

    if let Some(limit) = expectation.max_steps {
        match observations.steps {
            Some(steps) if steps <= limit => checks.push(CheckResult::ok(
                "max_steps",
                format!("{steps} step(s), limit {limit}"),
            )),
            Some(steps) => checks.push(CheckResult::no(
                "max_steps",
                format!("{steps} step(s) exceeds the limit of {limit}"),
            )),
            None => checks.push(CheckResult::unevaluated(
                "max_steps",
                format!("the run recorded no step count, so the limit of {limit} cannot hold"),
            )),
        }
    }

    if let Some(limit) = expectation.max_cost_micros {
        match observations.cost_micros {
            Some(cost) if cost <= limit => checks.push(CheckResult::ok(
                "max_cost_micros",
                format!("{cost}µ$, limit {limit}µ$"),
            )),
            Some(cost) => checks.push(CheckResult::no(
                "max_cost_micros",
                format!("{cost}µ$ exceeds the limit of {limit}µ$"),
            )),
            None => checks.push(CheckResult::unevaluated(
                "max_cost_micros",
                format!("the run recorded no cost, so the limit of {limit}µ$ cannot hold"),
            )),
        }
    }

    if let Some(limit) = expectation.max_latency_ms {
        match observations.latency_ms {
            Some(latency) if latency <= limit => checks.push(CheckResult::ok(
                "max_latency_ms",
                format!("{latency}ms, limit {limit}ms"),
            )),
            Some(latency) => checks.push(CheckResult::no(
                "max_latency_ms",
                format!("{latency}ms exceeds the limit of {limit}ms"),
            )),
            None => checks.push(CheckResult::unevaluated(
                "max_latency_ms",
                format!("the run recorded no latency, so the limit of {limit}ms cannot hold"),
            )),
        }
    }

    if expectation.rubric.is_some() {
        match judge {
            Some(outcome) if outcome.passed => checks.push(CheckResult::ok(
                "rubric",
                if outcome.reason.is_empty() {
                    "the judge accepted the output against the rubric".to_string()
                } else {
                    format!("the judge accepted it: {}", first_line(&outcome.reason))
                },
            )),
            Some(outcome) => checks.push(CheckResult::no(
                "rubric",
                if outcome.reason.is_empty() {
                    "the judge rejected the output against the rubric".to_string()
                } else {
                    format!("the judge rejected it: {}", first_line(&outcome.reason))
                },
            )),
            None => checks.push(CheckResult::unevaluated(
                "rubric",
                "the `rubric` check needs a judge model, which the run layer did not supply",
            )),
        }
    }

    if checks.is_empty() {
        return CaseVerdict {
            status: CaseStatus::Error,
            checks: vec![CheckResult::unevaluated(
                "expectations",
                "the case names no property, so nothing could be checked",
            )],
            summary: "nothing to check".to_string(),
        };
    }

    // A missing measurement makes a case an *error*, not a fail: "the run did not record a
    // step count" is a platform problem, and reporting it as a model problem teaches the
    // operator to distrust the suite. It also keeps `fail` meaning exactly one thing — the
    // model was measured and did not meet the bar.
    let unevaluated = checks.iter().any(|c| c.unevaluated);
    let held = checks.iter().filter(|c| c.passed).count();
    let status = if unevaluated {
        CaseStatus::Error
    } else if held == checks.len() {
        CaseStatus::Pass
    } else {
        CaseStatus::Fail
    };

    let summary = match status {
        CaseStatus::Pass => format!("{held}/{} propert{} held", checks.len(), if checks.len() == 1 { "y" } else { "ies" }),
        CaseStatus::Fail => {
            let failed: Vec<&str> = checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.property.as_str())
                .collect();
            format!("{held}/{} held — {}", checks.len(), failed.join(", "))
        }
        CaseStatus::Error => checks
            .iter()
            .find(|c| !c.passed)
            .map(|c| format!("could not be evaluated — {}", first_line(&c.detail)))
            .unwrap_or_else(|| "could not be evaluated".to_string()),
        CaseStatus::Skipped => "skipped".to_string(),
    };

    CaseVerdict { status, checks, summary }
}

/// Whether the installed guard would flag anything in `text`, and what it found.
///
/// The verdict is the tenant's own [`crate::guard_store::load_guard`] answer for the `eval`
/// feature, so "would REQ-105's detector mask this" is asked once, by the same rules the
/// production call path uses. Re-implementing a second pattern set here is exactly how the two
/// drift until the eval passes output the guard would have stopped in production.
///
/// The findings are the **masked** text with a label per hit, never the raw value: this string
/// is stored on a case result and rendered on the eval screen, so writing the value there would
/// copy personal data into the one table nobody thinks to protect. The masked rendering is
/// [`crate::guard_data::Finding::text`], which is the very string the provider would have got.
pub fn pii_findings(guard: &crate::guard_store::LoadedGuard, text: &str) -> GuardOutcome {
    let finding = guard.detector.inspect(text, None, Some("eval"), &guard.policy, "");
    let labels: Vec<String> = match &finding.verdict {
        crate::guard_data::GuardVerdict::Clear => Vec::new(),
        crate::guard_data::GuardVerdict::Allowed { matches, .. }
        | crate::guard_data::GuardVerdict::Masked { matches } => {
            matches.iter().map(|m| m.label.clone()).collect()
        }
        crate::guard_data::GuardVerdict::Blocked { label, message, .. } => {
            return GuardOutcome { clean: false, findings: vec![format!("{label}: {message}")] };
        }
    };
    let mut findings: Vec<String> = labels
        .iter()
        .map(|label| format!("{label} ({})", finding.label_counts.get(label).copied().unwrap_or(0)))
        .collect();
    if !findings.is_empty() {
        // The masked rendering is what makes the check actionable for a human reading the
        // expanded row, and it is already the text the provider would have received.
        findings.push(format!("the provider would have seen: {}", first_line(&finding.text)));
    }
    GuardOutcome { clean: findings.is_empty(), findings }
}

/// Parse an output that is supposed to be JSON, tolerating a fenced block.
///
/// Models wrap JSON in ```json fences constantly, and a `json_schema` case that fails on the
/// fence measures the model's markdown habits rather than the thing the schema is about.
fn validate_json(output: &str) -> std::result::Result<Value, String> {
    let trimmed = output.trim();
    let body = if trimmed.starts_with("```") {
        let without_open = trimmed
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or(trimmed);
        without_open
            .trim_end()
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    serde_json::from_str(body).map_err(|e| e.to_string())
}

/// The subset of JSON Schema a case can assert.
///
/// Deliberately not a general validator. The request names `json_schema` as a property and the
/// panel offers a small editor for it; a validator that silently accepted a keyword it did not
/// implement would let a case say `{"type":"object","required":["a"]}` and pass on an object
/// with no `a`, which is the exact failure the property exists to catch. Every keyword this
/// does not know is therefore **refused** at validation time rather than ignored at scoring
/// time.
const SUPPORTED_SCHEMA_KEYWORDS: &[&str] =
    &["type", "required", "properties", "items", "enum", "additionalProperties"];

/// Whether a schema uses only keywords this validator implements.
pub fn schema_is_supported(schema: &Value) -> std::result::Result<(), String> {
    fn walk(value: &Value, path: &str) -> std::result::Result<(), String> {
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        for (key, child) in object {
            if !SUPPORTED_SCHEMA_KEYWORDS.contains(&key.as_str()) {
                return Err(format!(
                    "`{path}` uses `{key}`, which this platform's `json_schema` check does not \
                     implement; supported keywords are {}",
                    SUPPORTED_SCHEMA_KEYWORDS.join(", ")
                ));
            }
            match (key.as_str(), child) {
                ("properties", Value::Object(map)) => {
                    for (name, sub) in map {
                        walk(sub, &format!("{path}.{name}"))?;
                    }
                }
                ("items", _) => walk(child, &format!("{path}[]"))?,
                _ => {}
            }
        }
        Ok(())
    }
    walk(schema, "schema")
}

/// Every way a value fails the schema, in the order the keywords are listed.
///
/// Returns a list rather than the first failure so the expanded row shows everything that is
/// wrong with the output at once.
pub fn schema_errors(value: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(object) = schema.as_object() else {
        return errors;
    };

    if let Some(expected) = object.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.is_i64() || value.is_u64(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => {
                errors.push(format!("schema asks for unknown type `{expected}`"));
                false
            }
        };
        if !matches {
            errors.push(format!("expected type {expected}, found {}", type_name(value)));
        }
    }

    if let Some(list) = object.get("required").and_then(Value::as_array) {
        if let Some(map) = value.as_object() {
            for key in list.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    errors.push(format!("missing required property `{key}`"));
                }
            }
        }
    }

    if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let names: Vec<String> = allowed.iter().map(|v| v.to_string()).collect();
            errors.push(format!("expected one of [{}], found {value}", names.join(", ")));
        }
    }

    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        if let Some(map) = value.as_object() {
            for (name, sub_schema) in properties {
                if let Some(child) = map.get(name) {
                    for error in schema_errors(child, sub_schema) {
                        errors.push(format!("{name}{error}"));
                    }
                }
            }
        }
    }

    if let Some(items) = object.get("items") {
        if let Some(list) = value.as_array() {
            for (index, child) in list.iter().enumerate() {
                for error in schema_errors(child, items) {
                    errors.push(format!("[{index}]{error}"));
                }
            }
        }
    }

    if object.get("additionalProperties") == Some(&Value::Bool(false))
        && let Some(map) = value.as_object()
    {
        for key in map.keys() {
            let known = object
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key(key));
            if !known {
                errors.push(format!("unexpected property `{key}`"));
            }
        }
    }

    errors
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 80;
    if text.chars().count() <= LIMIT {
        return format!("{:?}", text);
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head:?}… ({} chars)", text.chars().count())
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 160 {
        return line.to_string();
    }
    let head: String = line.chars().take(160).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expectations(value: Value) -> Expectation {
        expectation_from(&value).expect("expectations parse")
    }

    #[test]
    fn exact_match_passes_and_a_near_miss_fails() {
        let e = expectations(json!({ "exact": "hello" }));
        let pass = score_output(&e, "hello", &Observations::default(), None, None);
        assert_eq!(pass.status, CaseStatus::Pass);
        assert!(pass.checks.iter().all(|c| c.passed));

        let near = score_output(&e, "hello ", &Observations::default(), None, None);
        assert_eq!(near.status, CaseStatus::Fail);
        assert!(near.summary.contains("exact"), "the summary names the property: {}", near.summary);
    }

    #[test]
    fn every_named_property_must_hold_and_the_summary_names_the_failing_ones() {
        let e = expectations(json!({ "exact": "yes", "contains": ["extra"] }));
        let verdict = score_output(&e, "yes", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Fail);
        assert_eq!(verdict.checks.len(), 2);
        assert!(verdict.summary.contains("contains"), "{}", verdict.summary);
    }

    #[test]
    fn contains_checks_every_entry_separately() {
        let e = expectations(json!({ "contains": ["alpha", "beta"] }));
        let one_missing =
            score_output(&e, "alpha only", &Observations::default(), None, None);
        assert_eq!(one_missing.status, CaseStatus::Fail);
        assert_eq!(one_missing.checks.iter().filter(|c| c.passed).count(), 1);
        assert!(one_missing.checks[1].detail.contains("beta"));
    }

    #[test]
    fn an_invalid_regex_is_refused_at_parse_time_with_the_property_named() {
        let error = expectation_from(&json!({ "regex": ["a("] })).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("regex"), "{message}");
    }

    #[test]
    fn regex_matches_against_the_whole_output() {
        let e = expectations(json!({ "regex": ["order-\\d{4}"] }));
        let hit = score_output(&e, "see order-4821 for details", &Observations::default(), None, None);
        assert_eq!(hit.status, CaseStatus::Pass);
        let miss = score_output(&e, "see order-ABC for details", &Observations::default(), None, None);
        assert_eq!(miss.status, CaseStatus::Fail);
    }

    #[test]
    fn json_schema_accepts_a_fenced_block_because_models_fence_their_json() {
        let e = expectations(json!({
            "json_schema": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } }
        }));
        let fenced = score_output(
            &e,
            "```json\n{\"id\": 7}\n```",
            &Observations::default(),
            None,
            None,
        );
        assert_eq!(fenced.status, CaseStatus::Pass, "{}", fenced.summary);
    }

    #[test]
    fn json_schema_reports_every_failure_at_once() {
        let e = expectations(json!({
            "json_schema": {
                "type": "object",
                "required": ["id", "total"],
                "properties": { "id": { "type": "integer" } }
            }
        }));
        let verdict = score_output(&e, "{\"id\": \"seven\"}", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Fail);
        let detail = &verdict.checks[0].detail;
        assert!(detail.contains("id"), "{detail}");
        assert!(detail.contains("total"), "a missing required property is reported too: {detail}");
    }

    #[test]
    fn an_unimplemented_schema_keyword_is_refused_rather_than_ignored() {
        // A validator that skipped `pattern` would report a pass for a value that violates it,
        // and the case would read as green in a release gate.
        let error = schema_is_supported(&json!({ "type": "string", "pattern": "^a" }))
            .expect_err("pattern is not implemented here");
        assert!(error.contains("pattern"), "{error}");
        assert!(schema_is_supported(&json!({
            "type": "object",
            "required": ["a"],
            "properties": { "a": { "type": "string", "enum": ["x"] } },
            "additionalProperties": false
        }))
        .is_ok());
    }

    #[test]
    fn additional_properties_false_catches_the_extra_key() {
        let e = expectations(json!({
            "json_schema": {
                "type": "object",
                "properties": { "a": { "type": "string" } },
                "additionalProperties": false
            }
        }));
        let verdict = score_output(&e, "{\"a\": \"x\", \"b\": 1}", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Fail);
        assert!(verdict.checks[0].detail.contains("`b`"), "{}", verdict.checks[0].detail);
    }

    #[test]
    fn budget_properties_compare_the_measurement_and_say_so_when_it_is_missing() {
        let e = expectations(json!({ "max_steps": 3, "max_cost_micros": 100, "max_latency_ms": 500 }));
        let within = Observations { steps: Some(3), cost_micros: Some(100), latency_ms: Some(500), ..Default::default() };
        assert_eq!(score_output(&e, "x", &within, None, None).status, CaseStatus::Pass);

        let over = Observations { steps: Some(4), cost_micros: Some(101), latency_ms: Some(501), ..Default::default() };
        assert_eq!(score_output(&e, "x", &over, None, None).status, CaseStatus::Fail);

        // A missing measurement is an ERROR, not a fail: the model was never measured.
        let verdict = score_output(&e, "x", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Error, "{}", verdict.summary);
        assert!(verdict.summary.contains("could not be evaluated"), "{}", verdict.summary);
    }

    #[test]
    fn a_case_naming_no_property_is_an_error_not_a_pass() {
        let e = Expectation::default();
        assert!(e.is_empty());
        let verdict = score_output(&e, "anything", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Error);
        assert!(verdict.summary.contains("nothing to check"), "{}", verdict.summary);
    }

    #[test]
    fn a_rubric_without_a_judge_is_an_error_rather_than_a_green_row() {
        let e = expectations(json!({ "rubric": "explains the refund policy in one sentence" }));
        let verdict = score_output(&e, "we refund within 30 days", &Observations::default(), None, None);
        assert_eq!(verdict.status, CaseStatus::Error, "a case nobody judged must not pass");
        assert!(verdict.checks[0].detail.contains("judge model"), "{}", verdict.checks[0].detail);
    }

    #[test]
    fn a_rubric_verdict_carries_the_judges_reason_into_the_check() {
        let e = expectations(json!({ "rubric": "one sentence" }));
        let judge = JudgeOutcome { passed: false, reason: "It listed two policies.\nSecond line.".to_string() };
        let verdict = score_output(&e, "a\nb", &Observations::default(), Some(&judge), None);
        assert_eq!(verdict.status, CaseStatus::Fail);
        let detail = &verdict.checks[0].detail;
        assert!(detail.contains("two policies"), "{detail}");
        assert!(!detail.contains("Second line"), "the check line is one line: {detail}");
    }

    #[test]
    fn a_clean_output_passes_no_pii_and_a_dirty_one_fails_with_the_findings_named() {
        let e = expectations(json!({ "no_pii": true }));
        let clean = GuardOutcome { clean: true, findings: vec![] };
        assert_eq!(
            score_output(&e, "nothing here", &Observations::default(), None, Some(&clean)).status,
            CaseStatus::Pass
        );
        let dirty = GuardOutcome { clean: false, findings: vec!["email (a***@x.com)".into()] };
        let verdict = score_output(&e, "write to a@x.com", &Observations::default(), None, Some(&dirty));
        assert_eq!(verdict.status, CaseStatus::Fail);
        assert!(verdict.checks[0].detail.contains("a***@x.com"), "{}", verdict.checks[0].detail);
    }

    #[test]
    fn citations_required_reads_the_observations_not_the_text() {
        let e = expectations(json!({ "citations_required": true }));
        let none = Observations::default();
        assert_eq!(score_output(&e, "x", &none, None, None).status, CaseStatus::Fail);
        let some = Observations { citations: vec!["doc:1".into()], ..Default::default() };
        assert_eq!(score_output(&e, "x", &some, None, None).status, CaseStatus::Pass);
    }

    #[test]
    fn the_score_is_the_share_of_checks_that_held() {
        let e = expectations(json!({ "exact": "a", "contains": ["b"], "regex": ["c"] }));
        let verdict = score_output(&e, "a", &Observations::default(), None, None);
        assert_eq!(verdict.score(), 1.0 / 3.0);
    }

    #[test]
    fn property_names_are_reported_in_the_documented_order() {
        let e = expectations(json!({ "rubric": "r", "exact": "a", "max_steps": 2 }));
        assert_eq!(e.property_names(), vec!["exact", "max_steps", "rubric"]);
        assert!(e.has_rubric());
    }

    #[test]
    fn a_negative_budget_property_is_refused_at_parse_time() {
        let error = expectation_from(&json!({ "max_steps": -1 })).unwrap_err();
        assert!(error.to_string().contains("max_steps"), "{error}");
    }

    #[test]
    fn a_blank_rubric_is_refused_because_it_asserts_nothing() {
        let error = expectation_from(&json!({ "rubric": "   " })).unwrap_err();
        assert!(error.to_string().contains("rubric"), "{error}");
    }

    #[test]
    fn an_unknown_property_is_refused_by_name_with_the_valid_list() {
        let object = json!({ "containsAll": ["a"] });
        let map = object.as_object().unwrap();
        let error = expected_property(map, "containsAll").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("containsAll"), "{message}");
        assert!(message.contains("contains"), "the message lists the real ones: {message}");
    }

    // The `no_pii` property is NOT asserted here. Its meaning is "REQ-105's detector would mask
    // a value here", and that detector needs the tenant's compiled rules and policy — which live
    // in the database, not in this pure module. The assertion lives in the crate's database
    // tests instead. Writing a second pattern set here would make this unit test pass and
    // disagree with production on the very first value the guard knows, which is the failure
    // mode the delegation exists to prevent.
}
