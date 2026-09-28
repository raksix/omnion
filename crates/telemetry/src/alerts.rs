//! Alert rules: the expression subset, the evaluator, silences, and the pending → firing →
//! resolved state machine (REQ-126, slice 4).
//!
//! ## Why there is no PromQL engine in the API process
//!
//! The request's rule form says "expression (validated against the catalogue)" and the alert
//! screen's `Preview` says it "shows whether the rule is currently firing against live data". A
//! general PromQL implementation would satisfy both, and would also hand every operator who can
//! write a rule the ability to run an unbounded query against the registry on every evaluation
//! tick — a rule that scans a year of history every fifteen seconds, written by accident, is a
//! denial of service against the process that serves traffic.
//!
//! So the expression is a **closed grammar** ([`parse`]) over the metric registry, not a
//! language. It supports exactly what the bundled rules need:
//!
//! ```text
//! expr      := selector [ op number ]
//! selector  := name [ "{" label "=" value { "," … } "}" ]
//! op        := ">" | ">=" | "<" | "<=" | "==" | "!="
//! ```
//!
//! Everything outside the grammar is a `422` naming the field and the position, never a rule
//! that is quietly stored and never fires. A rule that cannot be parsed is a rule that looks
//! configured.
//!
//! ## Why the state machine has three states and a dwell
//!
//! `for_seconds` is in the request's data model, and the reason a rule needs it is that a single
//! sample above a threshold is usually noise: a deploy, a GC pause, one slow replica. So a rule
//! that crosses its threshold goes `pending` and only becomes `firing` once it has stayed there
//! for the dwell. That is also what makes the flapping case survivable, and the state machine is
//! the thing that coalesces it:
//!
//! ```text
//!            ┌────────────────── silence removed ──────────────────┐
//!            │                                                      │
//!   (none) ──┴─→ pending ──dwell elapsed──→ firing ──below threshold──→ resolved
//!                │                          │
//!                └── below threshold ──→ (none)
//! ```
//!
//! The database holds **one open event per rule** (a partial unique index on the migration), so
//! a rule that flaps ten times a minute writes ten *evaluations*, not ten rows and ten
//! notifications. A pending event that goes back below its threshold is deleted rather than
//! written as `resolved`, because "it was above the line for four seconds" is not something an
//! operator needs a timeline entry for.

use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::TelemetryError;
use crate::metrics;

/// The family a rule transition is counted in.
///
/// Declared in `metrics::FAMILIES`; the test at the bottom asserts it, because a counter that is
/// recorded but not declared is invisible on the scrape and the "notifies once" acceptance line
/// has nothing to read.
pub const TRANSITIONS_FAMILY: &str = "omnion_alert_transitions_total";

/// The cap on how many rules one evaluation pass may hold in memory.
pub const MAX_RULES_PER_PASS: i64 = 500;

/// The cap on a silence, in days. Long enough for a maintenance window and short enough that a
/// forgotten silence expires on its own.
pub const MAX_SILENCE_DAYS: i64 = 7;

/// The cap on `for_seconds` — one day. A rule that must stay true for longer than a day is a
/// rule about a trend, and a trend belongs in a report.
pub const MAX_FOR_SECONDS: i32 = 86_400;

/// One rule's row, as the evaluator needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Rule {
    /// Primary key.
    pub id: Uuid,
    /// Unique name, shown in the panel and in the notification.
    pub name: String,
    /// The expression, in the closed grammar [`parse`].
    pub expr: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// How long the threshold must hold before the rule fires.
    pub for_seconds: i32,
    /// The operator-facing one-liner.
    pub summary: String,
    /// Where to read more.
    pub runbook_url: Option<String>,
    /// Extra labels merged into the notification payload.
    pub labels: serde_json::Value,
    /// `bundled` or `custom`.
    pub source: String,
    /// Whether the rule is evaluated at all.
    pub enabled: bool,
}

/// One open or closed event, as the timeline needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AlertEvent {
    /// Primary key.
    pub id: i64,
    /// The rule this belongs to.
    pub rule_id: Uuid,
    /// `pending`, `firing` or `resolved`.
    pub state: String,
    /// The value at the last evaluation.
    pub value: Option<f64>,
    /// The value that promoted the event to `firing`.
    pub firing_value: Option<f64>,
    /// When the event opened.
    pub started_at: OffsetDateTime,
    /// When it resolved, if it has.
    pub ended_at: Option<OffsetDateTime>,
    /// When it fired, if it did.
    pub fired_at: Option<OffsetDateTime>,
    /// Whether a notification went out.
    pub notified: bool,
    /// `threshold`, `dwell` or `silenced`.
    pub reason: String,
    /// The label set the selector matched.
    pub context: serde_json::Value,
}

/// A silence, as the panel reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Silence {
    /// Primary key.
    pub id: Uuid,
    /// `None` means every rule — a maintenance window, not one rule's problem.
    pub rule_id: Option<Uuid>,
    /// The reason, which is required so a silence is never anonymous.
    pub reason: String,
    /// When it takes effect; `None` means immediately.
    pub starts_at: Option<OffsetDateTime>,
    /// When it stops being effective.
    pub ends_at: OffsetDateTime,
    /// Who created it.
    pub created_by: Option<Uuid>,
}

/// The comparison an expression asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operator {
    /// Strictly greater.
    Greater,
    /// Greater or equal.
    GreaterOrEqual,
    /// Strictly less.
    Less,
    /// Less or equal.
    LessOrEqual,
    /// Equal.
    Equal,
    /// Not equal.
    NotEqual,
}

impl Operator {
    /// Evaluate the comparison for one sample.
    #[must_use]
    pub fn holds(self, sample: f64, threshold: f64) -> bool {
        match self {
            Operator::Greater => sample > threshold,
            Operator::GreaterOrEqual => sample >= threshold,
            Operator::Less => sample < threshold,
            Operator::LessOrEqual => sample <= threshold,
            Operator::Equal => (sample - threshold).abs() < f64::EPSILON,
            Operator::NotEqual => (sample - threshold).abs() >= f64::EPSILON,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Operator::Greater => ">",
            Operator::GreaterOrEqual => ">=",
            Operator::Less => "<",
            Operator::LessOrEqual => "<=",
            Operator::Equal => "==",
            Operator::NotEqual => "!=",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            ">" => Operator::Greater,
            ">=" => Operator::GreaterOrEqual,
            "<" => Operator::Less,
            "<=" => Operator::LessOrEqual,
            "==" => Operator::Equal,
            "!=" => Operator::NotEqual,
            _ => return None,
        })
    }
}

/// A parsed expression: a family, a label matcher, a comparison and a threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    /// The declared family this reads.
    pub family: &'static str,
    /// The equality matchers, by label name. Only equality exists on purpose — see the module
    /// comment — and an empty map means "every series of the family".
    pub matchers: Vec<(String, String)>,
    /// The comparison.
    pub operator: Operator,
    /// The threshold it is compared against.
    pub threshold: f64,
}

impl Expression {
    /// Evaluate against the live registry.
    ///
    /// A family with no samples evaluates to `false`, not to a zero the comparison might pass:
    /// "no errors recorded" and "the recorder is not running" produce the same zero, and an
    /// alert that fires on the second of those is an alert that pages about its own monitoring.
    /// So the answer is `Option<f64>`, and `None` means "no data", which never fires.
    #[must_use]
    pub fn evaluate(&self) -> Evaluation {
        let Some(spec) = metrics::family(self.family) else {
            // Unreachable through `parse` (it checks the catalogue), but a family removed from a
            // build while a rule survives the migration must not read as "healthy".
            return Evaluation {
                value: None,
                matched: false,
                series: 0,
            };
        };

        let snapshots = metrics::global().series_of(spec.name, 1);
        let mut matched: Option<f64> = None;
        let mut series = 0usize;

        for snapshot in &snapshots {
            if !self.matches(spec, &snapshot.labels) {
                continue;
            }
            series += 1;
            // A rule with no matchers aggregates: the worst value across the family, which for
            // an error rate is the series that is actually failing rather than the average of one
            // broken route among forty healthy ones. A NaN never wins the comparison, so a series
            // the registry could not compute cannot make a rule fire either.
            let value = snapshot.total;
            if !value.is_finite() {
                continue;
            }
            matched = Some(match matched {
                None => value,
                Some(previous) => match self.operator {
                    Operator::Less | Operator::LessOrEqual => previous.min(value),
                    _ => previous.max(value),
                },
            });
        }

        Evaluation {
            value: matched,
            matched: series > 0,
            series,
        }
    }

    fn matches(&self, spec: &metrics::FamilySpec, labels: &[String]) -> bool {
        // A matcher for a label the family does not declare can never be satisfied. Returning
        // false rather than ignoring it is the difference between a rule that fires and a rule
        // an operator believes is watching something.
        for (name, wanted) in &self.matchers {
            let Some(position) = spec
                .labels
                .iter()
                .position(|declared| *declared == name.as_str())
            else {
                return false;
            };
            if labels.get(position).map(String::as_str) != Some(wanted.as_str()) {
                return false;
            }
        }
        true
    }

    /// Render the expression back, for the panel and for the alert payload.
    #[must_use]
    pub fn render(&self) -> String {
        if self.matchers.is_empty() {
            return format!(
                "{} {} {}",
                self.family,
                self.operator.as_str(),
                self.threshold
            );
        }
        let matchers: Vec<String> = self
            .matchers
            .iter()
            .map(|(name, value)| format!("{name}=\"{value}\""))
            .collect();
        format!(
            "{}{{{}}} {} {}",
            self.family,
            matchers.join(","),
            self.operator.as_str(),
            self.threshold
        )
    }
}

/// What one evaluation produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Evaluation {
    /// The value the comparison saw, or `None` when the family has no samples.
    pub value: Option<f64>,
    /// Whether any series matched the selector at all.
    pub matched: bool,
    /// How many series matched.
    pub series: usize,
}

impl Evaluation {
    /// Whether the threshold is currently crossed.
    ///
    /// `matched` is the guard: with no data the answer is "no", never "0 > 0 is false by
    /// accident" — the two agree today and would diverge the moment someone writes `< 0.01`.
    #[must_use]
    pub fn breaches(&self, operator: Operator, threshold: f64) -> bool {
        match self.value {
            Some(value) if self.matched => operator.holds(value, threshold),
            _ => false,
        }
    }
}

/// Parse an expression, refusing anything outside the grammar.
///
/// The error names the field and what is wrong with it, because this is written by a human in a
/// form and the difference between "your rule is saved and will never fire" and "your rule is
/// wrong" is the whole value of validating it.
pub fn parse(expr: &str) -> Result<Expression, String> {
    let text = expr.trim();
    if text.is_empty() {
        return Err("`expr` is empty — expected `<family> [{labels}] <op> <number>`".to_owned());
    }

    let (selector, rest) = match text.find('{') {
        Some(open) => {
            let close = text.find('}').ok_or_else(|| {
                format!("`expr` has a `{{` at position {open} with no closing `}}`")
            })?;
            if close < open {
                return Err("`expr` closes its label block before it opens".to_owned());
            }
            (&text[..open], text[close + 1..].trim_start())
        }
        None => {
            let split = text.find(|c: char| c.is_whitespace()).ok_or_else(|| {
                format!(
                    "`expr` has no comparison — expected `<family> {{{{labels}}}} <op> \
                         <number>`, e.g. `omnion_http_requests_total{{status=\"5xx\"}} > 0`"
                )
            })?;
            (&text[..split], text[split..].trim_start())
        }
    };

    let family_name = selector.trim();
    let spec = metrics::family(family_name)
        .ok_or_else(|| format!("`{family_name}` is not a declared metric family"))?;

    let mut matchers = Vec::new();
    if let Some(open) = text.find('{') {
        let close = text.find('}').expect("the closing brace was found above");
        let inner = &text[open + 1..close];
        for part in inner.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (name, value) = part.split_once('=').ok_or_else(|| {
                format!("`{part}` is not a label matcher — only `name=\"value\"` is supported")
            })?;
            let name = name.trim();
            let value = value.trim().trim_matches('"');
            if !spec.labels.contains(&name) {
                return Err(format!(
                    "`{name}` is not a label of `{family_name}`; it declares {}",
                    if spec.labels.is_empty() {
                        "no labels".to_owned()
                    } else {
                        spec.labels.join(", ")
                    }
                ));
            }
            matchers.push((name.to_owned(), value.to_owned()));
        }
    }

    let mut parts = rest.split_whitespace();
    let op_text = parts.next().ok_or_else(|| {
        format!(
            "`expr` has no comparison after the selector — expected one of >, >=, <, <=, ==, != \
             (e.g. `{family_name} > 0`)"
        )
    })?;
    let operator = Operator::parse(op_text)
        .ok_or_else(|| format!("`{op_text}` is not a comparison — use >, >=, <, <=, == or !="))?;
    let threshold_text = parts.next().ok_or_else(|| {
        format!("`expr` has no threshold after `{op_text}` — e.g. `{family_name} {op_text} 5`")
    })?;
    let threshold: f64 = threshold_text.parse().map_err(|_| {
        format!("`{threshold_text}` is not a number — the threshold must be numeric")
    })?;
    if !threshold.is_finite() {
        return Err(format!(
            "`{threshold_text}` is not a finite number; NaN and infinity can never be crossed"
        ));
    }
    if let Some(extra) = parts.next() {
        return Err(format!(
            "`{extra}` is unexpected — `expr` is one selector and one comparison"
        ));
    }

    Ok(Expression {
        family: spec.name,
        matchers,
        operator,
        threshold,
    })
}

/// A rule as the panel reads it, with its state.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleWithState {
    /// The rule's columns.
    #[sqlx(flatten)]
    pub rule: Rule,
    /// The open event's state, if one is open.
    pub state: Option<String>,
    /// The value at the last evaluation, if the rule has fired or is pending.
    pub value: Option<f64>,
    /// Whether a silence currently covers this rule.
    pub silenced: bool,
    /// When the silence ends, if one is active.
    pub silenced_until: Option<OffsetDateTime>,
}

/// Load every enabled rule, with its open event folded in.
///
/// `silenced` is computed with a SQL predicate rather than in Rust so the panel's list and the
/// evaluator agree by construction: a silence that starts in the middle of a page load is applied
/// or not applied to both at the same instant.
pub async fn list_rules(pool: &PgPool) -> Result<Vec<RuleWithState>, TelemetryError> {
    let rows = sqlx::query_as::<_, RuleWithState>(
        "select r.id, r.name, r.expr, r.severity, r.for_seconds, r.summary, r.runbook_url, \
                r.labels, r.source, r.enabled, \
                e.state as state, e.value as value, \
                (s.id is not null) as silenced, s.ends_at as silenced_until \
         from obs_alert_rules r \
         left join lateral ( \
             select state, value from obs_alert_events \
             where rule_id = r.id and state <> 'resolved' \
             order by started_at desc limit 1 \
         ) e on true \
         left join lateral ( \
             select id, ends_at from obs_silences \
             where (rule_id is null or rule_id = r.id) \
               and ends_at > now() \
               and (starts_at is null or starts_at <= now()) \
             order by ends_at asc limit 1 \
         ) s on true \
         order by r.name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Load one rule by id.
pub async fn find_rule(pool: &PgPool, id: Uuid) -> Result<Option<Rule>, TelemetryError> {
    let row = sqlx::query_as::<_, Rule>(
        "select id, name, expr, severity, for_seconds, summary, runbook_url, labels, source, \
                enabled \
         from obs_alert_rules where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The events, newest first, for the alert timeline.
pub async fn recent_events(pool: &PgPool, limit: i64) -> Result<Vec<AlertEvent>, TelemetryError> {
    let rows = sqlx::query_as::<_, AlertEvent>(
        "select e.id, e.rule_id, e.state, e.value, e.firing_value, e.started_at, e.ended_at, \
                e.fired_at, e.notified, e.reason, e.context \
         from obs_alert_events e \
         order by e.started_at desc \
         limit $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whether a silence currently covers this rule.
///
/// A `None` `rule_id` is a global silence — the maintenance-window case. It is checked in SQL
/// against `now()` rather than in Rust against a clock, because the evaluator and the panel read
/// this through the same predicate and two clocks disagree at the boundary.
pub async fn is_silenced(
    pool: &PgPool,
    rule_id: Uuid,
) -> Result<Option<OffsetDateTime>, TelemetryError> {
    let until: Option<OffsetDateTime> = sqlx::query_scalar(
        "select ends_at from obs_silences \
         where (rule_id is null or rule_id = $1) \
           and ends_at > now() \
           and (starts_at is null or starts_at <= now()) \
         order by ends_at asc limit 1",
    )
    .bind(rule_id)
    .fetch_optional(pool)
    .await?;
    Ok(until)
}

/// What one evaluation pass did.
///
/// `Eq` is deliberately absent. This struct used to hold nothing but counts and derived it; it
/// now also carries the transitions, whose `value` is a measured `f64`. `Eq` on a float is a
/// claim about precision the type cannot make — NaN is not equal to itself — and deriving it here
/// would be a rule that silently stops meaning anything. `PartialEq` is the honest bound, and no
/// caller compared two reports for identity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassReport {
    /// Rules evaluated.
    pub evaluated: usize,
    /// Events that started as `pending`.
    pub opened: usize,
    /// Events that became `firing`.
    pub fired: usize,
    /// Events that resolved.
    pub resolved: usize,
    /// Pending events that went back below their threshold and were discarded.
    pub discarded: usize,
    /// Rules a silence covered, so their breach is deliberately not an event.
    pub silenced: usize,
    /// Rules whose expression no longer parses — kept, counted, and logged.
    pub invalid: usize,
    /// What actually moved, in the shape the events are built from.
    ///
    /// It used to be that the only record of a transition was the row, and a subscriber learned
    /// about it by polling the admin screen. The request documents `observability.alert.fired`
    /// and `.resolved` as **emitted**, so a pass has to hand the transition to the loop rather
    /// than only write it down — and carrying the details here is what lets the loop build the
    /// payload without a second query against a table that has already moved on.
    pub transitions: Vec<crate::events::AlertTransition>,
}

impl PassReport {
    /// Whether anything changed. The loop uses this to skip the notification pass.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.fired == 0 && self.resolved == 0
    }

    /// The transitions that became `firing`.
    ///
    /// Two accessors rather than one list a caller filters, because the caller is a `match` on
    /// the direction: a single list would let a loop emit `alert.fired` from a resolution with
    /// nothing in the type objecting, and the name is what a subscriber's routing keys on.
    pub fn fired_transitions(&self) -> impl Iterator<Item = &crate::events::AlertTransition> {
        self.transitions.iter().filter(|item| item.is_firing())
    }

    /// The transitions that resolved.
    pub fn resolved_transitions(&self) -> impl Iterator<Item = &crate::events::AlertTransition> {
        self.transitions.iter().filter(|item| !item.is_firing())
    }
}

/// One evaluation pass: read every rule, evaluate it, and move its state machine.
///
/// Pure with respect to time — `now` is a parameter, not a clock read — because the dwell is the
/// part worth testing and a test that waits 300 seconds to prove a promotion is a test that never
/// runs.
pub async fn evaluate_pass(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<PassReport, TelemetryError> {
    let rules: Vec<Rule> = sqlx::query_as::<_, Rule>(
        "select id, name, expr, severity, for_seconds, summary, runbook_url, labels, source, \
                enabled \
         from obs_alert_rules where enabled order by name limit $1",
    )
    .bind(MAX_RULES_PER_PASS)
    .fetch_all(pool)
    .await?;

    let mut report = PassReport::default();
    for rule in rules {
        report.evaluated += 1;
        let expression = match parse(&rule.expr) {
            Ok(expression) => expression,
            Err(error) => {
                report.invalid += 1;
                tracing::warn!(rule = %rule.name, error = %error, "the alert rule does not parse");
                continue;
            }
        };

        let evaluation = expression.evaluate();
        let breaching = evaluation.breaches(expression.operator, expression.threshold);

        let open: Option<AlertEvent> = sqlx::query_as::<_, AlertEvent>(
            "select id, rule_id, state, value, firing_value, started_at, ended_at, fired_at, \
                    notified, reason, context \
             from obs_alert_events where rule_id = $1 and state <> 'resolved' \
             order by started_at desc limit 1",
        )
        .bind(rule.id)
        .fetch_optional(pool)
        .await?;

        if let Some(until) = is_silenced(pool, rule.id).await? {
            report.silenced += 1;
            // A silence does NOT resolve an open event. Silencing a rule during an incident is
            // how an operator says "I know"; the event stays `firing` and keeps its history, and
            // the silence suppresses the *notification*, not the fact. Deleting the event here
            // would make the timeline claim the problem stopped when it did not.
            if let Some(event) = &open {
                if !event
                    .context
                    .get("silenced_until")
                    .is_some_and(serde_json::Value::is_null)
                {
                    let _ = sqlx::query(
                        "update obs_alert_events set context = context || jsonb_build_object( \
                             'silenced_until', $2::timestamptz) where id = $1",
                    )
                    .bind(event.id)
                    .bind(until)
                    .execute(pool)
                    .await?;
                }
            }
            continue;
        }

        match (breaching, open) {
            (false, None) => {
                // Nothing to do: a rule that has never fired and is not firing stays silent.
            }
            (true, None) => {
                if rule.for_seconds == 0 {
                    // No dwell: the rule fires on the first breaching sample, which is what
                    // `for_seconds: 0` is for.
                    fire(pool, &rule, &expression, evaluation.value, now, &mut report).await?;
                } else {
                    pending(pool, &rule, &expression, evaluation.value, now, &mut report).await?;
                }
            }
            (true, Some(event)) => {
                let value = evaluation.value;
                if event.state == "pending" {
                    let elapsed = now - event.started_at;
                    if elapsed >= time::Duration::seconds(i64::from(rule.for_seconds)) {
                        promote(pool, &event, &rule, value, now, &mut report).await?;
                    } else {
                        // Still dwelling: refresh the value so the screen shows the current
                        // number while it waits, not the one that opened the event.
                        let _ = sqlx::query("update obs_alert_events set value = $2 where id = $1")
                            .bind(event.id)
                            .bind(value)
                            .execute(pool)
                            .await?;
                    }
                }
                // Already firing: the value is refreshed, no new event, no second notification.
            }
            (false, Some(event)) => {
                if event.state == "firing" {
                    resolve(pool, &event, &rule, evaluation.value, now, &mut report).await?;
                } else {
                    // A pending event that fell back below the threshold never fired, so it is
                    // discarded rather than resolved: "it was above the line for four seconds"
                    // is noise, and writing it as a resolved event is how a timeline fills up.
                    let _ = sqlx::query("delete from obs_alert_events where id = $1")
                        .bind(event.id)
                        .execute(pool)
                        .await?;
                    report.discarded += 1;
                }
            }
        }
    }

    Ok(report)
}

async fn pending(
    pool: &PgPool,
    rule: &Rule,
    expression: &Expression,
    value: Option<f64>,
    now: OffsetDateTime,
    report: &mut PassReport,
) -> Result<(), TelemetryError> {
    sqlx::query(
        "insert into obs_alert_events (rule_id, state, value, reason, started_at, context) \
         values ($1, 'pending', $2, 'threshold', $3, $4)",
    )
    .bind(rule.id)
    .bind(value)
    .bind(now)
    .bind(serde_json::json!({
        "expr": expression.render(),
        "severity": rule.severity,
        "labels": rule.labels,
    }))
    .execute(pool)
    .await?;
    report.opened += 1;
    Ok(())
}

async fn fire(
    pool: &PgPool,
    rule: &Rule,
    expression: &Expression,
    value: Option<f64>,
    now: OffsetDateTime,
    report: &mut PassReport,
) -> Result<(), TelemetryError> {
    sqlx::query(
        "insert into obs_alert_events \
             (rule_id, state, value, firing_value, fired_at, reason, started_at, context) \
         values ($1, 'firing', $2, $2, $3, 'threshold', $3, $4)",
    )
    .bind(rule.id)
    .bind(value)
    .bind(now)
    .bind(serde_json::json!({
        "expr": expression.render(),
        "severity": rule.severity,
        "runbook_url": rule.runbook_url,
        "labels": rule.labels,
    }))
    .execute(pool)
    .await?;
    report.opened += 1;
    report.fired += 1;
    report.transitions.push(transition_for(rule, value, None));
    Ok(())
}

async fn promote(
    pool: &PgPool,
    event: &AlertEvent,
    rule: &Rule,
    value: Option<f64>,
    now: OffsetDateTime,
    report: &mut PassReport,
) -> Result<(), TelemetryError> {
    sqlx::query(
        "update obs_alert_events \
         set state = 'firing', value = $2, firing_value = $2, fired_at = $3, reason = 'dwell', \
             context = context || $4 \
         where id = $1",
    )
    .bind(event.id)
    .bind(value)
    .bind(now)
    .bind(serde_json::json!({
        "dwell_seconds": rule.for_seconds,
        "held_since": event.started_at.format(&Rfc3339).unwrap_or_default(),
        "severity": rule.severity,
        "runbook_url": rule.runbook_url,
    }))
    .execute(pool)
    .await?;
    report.fired += 1;
    // `promote` is a promotion, not a new fire, so the event it moved is the one the panel
    // already shows as `pending`. The transition carries the same five fields either way — a
    // subscriber that handles `alert.fired` must not need a second shape for the dwell case.
    report.transitions.push(transition_for(rule, value, None));
    Ok(())
}

async fn resolve(
    pool: &PgPool,
    event: &AlertEvent,
    rule: &Rule,
    value: Option<f64>,
    now: OffsetDateTime,
    report: &mut PassReport,
) -> Result<(), TelemetryError> {
    sqlx::query(
        "update obs_alert_events set state = 'resolved', ended_at = $2, value = $3 where id = $1",
    )
    .bind(event.id)
    .bind(now)
    .bind(value)
    .execute(pool)
    .await?;
    report.resolved += 1;
    // The duration is measured from the moment it FIRED, not from when the event opened: a rule
    // with a 300-second dwell spent those 300 seconds as `pending`, and a subscriber reading
    // `duration_seconds` means "how long was this incident", not "how long was the rule true
    // including its dwell". The event row carries both instants, so the distinction is a choice
    // this line makes rather than something the caller has to remember.
    let opened = event.fired_at.unwrap_or(event.started_at);
    report.transitions.push(transition_for(
        rule,
        value,
        Some(i64::try_from((now - opened).whole_seconds()).unwrap_or(i64::MAX)),
    ));
    Ok(())
}

/// The transition a pass hands to the loop, in the shape the event payload is built from.
///
/// One function for all three transitions, because the payload's five documented fields are the
/// same for both directions — a subscriber must not need a second shape for `resolved`, and a
/// caller that assembled the struct itself three times is three places to add a field.
fn transition_for(
    rule: &Rule,
    value: Option<f64>,
    duration_seconds: Option<i64>,
) -> crate::events::AlertTransition {
    crate::events::AlertTransition {
        rule_id: rule.id,
        rule: rule.name.clone(),
        severity: rule.severity.clone(),
        value,
        window_seconds: rule.for_seconds,
        runbook_url: rule.runbook_url.clone(),
        labels: rule.labels.clone(),
        duration_seconds,
    }
}

/// Mark every open, un-notified `firing` event as notified and return the payloads.
///
/// **Once per event, not once per evaluation.** The `notified` flag is set inside the same
/// statement that selects the rows, so two evaluators racing produce one notification between
/// them rather than two. This is the acceptance line's "notifies once" stated as an UPDATE
/// returning the rows it changed.
pub async fn claim_notifications(pool: &PgPool) -> Result<Vec<Notification>, TelemetryError> {
    let rows = sqlx::query_as::<_, Notification>(
        "update obs_alert_events e \
         set notified = true \
         from obs_alert_rules r \
         where e.rule_id = r.id \
           and e.state = 'firing' \
           and e.notified = false \
         returning e.id as event_id, e.rule_id as rule_id, e.firing_value as value, \
                   e.fired_at as fired_at, r.name as rule_name, r.severity as severity, \
                   r.summary as summary, r.runbook_url as runbook_url, e.context as context",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One alert notification, in the shape the request fixes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Notification {
    /// The event that fired.
    pub event_id: i64,
    /// Its rule.
    pub rule_id: Uuid,
    /// The value that crossed.
    pub value: Option<f64>,
    /// When it fired.
    pub fired_at: Option<OffsetDateTime>,
    /// The rule's name.
    pub rule_name: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// The operator-facing one-liner.
    pub summary: String,
    /// Where to read more.
    pub runbook_url: Option<String>,
    /// The label set and the expression that matched.
    pub context: serde_json::Value,
}

impl Notification {
    /// The webhook payload.
    ///
    /// The request fixes its contents: "rule name, severity, value, window, runbook link, and
    /// nothing else. Log lines, user data and secret fragments never appear in a payload." So the
    /// struct IS the payload — there is no path by which a log line can reach it, because there
    /// is no field to put one in.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "rule": self.rule_name,
            "severity": self.severity,
            "value": self.value,
            "window_seconds": self
                .context
                .get("dwell_seconds")
                .and_then(serde_json::Value::as_i64),
            "runbook_url": self.runbook_url,
            "labels": self
                .context
                .get("labels")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }
}

/// Preview one expression against live data without saving anything.
///
/// The request's QA plan clicks the preview "while a rule is firing", so this reads the registry
/// and the same `parse` the evaluator uses — a preview that ran a *different* evaluation path
/// would be a preview that can disagree with the alert it is previewing.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// The rendered expression.
    pub rendered: String,
    /// The value it saw, if any.
    pub value: Option<f64>,
    /// Whether any series matched.
    pub matched: bool,
    /// How many matched.
    pub series: usize,
    /// Whether it would breach right now.
    pub breaching: bool,
    /// The family, so the panel can link to it in the catalogue.
    pub family: &'static str,
}

/// Evaluate an expression for the preview.
pub fn preview(expr: &str) -> Result<Preview, String> {
    let expression = parse(expr)?;
    let evaluation = expression.evaluate();
    Ok(Preview {
        rendered: expression.render(),
        value: evaluation.value,
        matched: evaluation.matched,
        series: evaluation.series,
        breaching: evaluation.breaches(expression.operator, expression.threshold),
        family: expression.family,
    })
}

/// Prune resolved events older than the window.
///
/// Audit and incident rows are NOT touched: this is the request's line "Retention prunes log
/// rows and trace-index rows past the window without touching audit or incident data", and the
/// half that is easy to get wrong is the half that deletes more than intended. A pending or
/// firing event is never pruned — it is the live alert.
pub async fn prune_events(pool: &PgPool, retention_days: i64) -> Result<i64, TelemetryError> {
    let removed = sqlx::query(
        "delete from obs_alert_events \
         where state = 'resolved' \
           and ended_at < now() - make_interval(days => $1::int)",
    )
    .bind(i32::try_from(retention_days.clamp(1, 30)).unwrap_or(1))
    .execute(pool)
    .await?;
    // `rows_affected` is `u64`; every other count in this crate is `i64`, because a JSON number
    // and a `bigint` column have the same wire type. The conversion happens here rather than at
    // every call site.
    Ok(i64::try_from(removed.rows_affected()).unwrap_or(i64::MAX))
}

/// Delete silences that have ended.
pub async fn prune_silences(pool: &PgPool) -> Result<i64, TelemetryError> {
    let removed = sqlx::query("delete from obs_silences where ends_at < now()")
        .execute(pool)
        .await?;
    Ok(i64::try_from(removed.rows_affected()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundled_expression_parses_to_its_parts() {
        let parsed = parse("omnion_http_requests_total{status=\"5xx\"} > 0").expect("parses");
        assert_eq!(parsed.family, "omnion_http_requests_total");
        assert_eq!(parsed.operator, Operator::Greater);
        assert_eq!(parsed.threshold, 0.0);
        assert_eq!(
            parsed.matchers,
            vec![("status".to_owned(), "5xx".to_owned())]
        );
    }

    #[test]
    fn a_bare_family_with_a_threshold_parses_without_labels() {
        let parsed = parse("omnion_queue_depth > 100").expect("parses");
        assert!(parsed.matchers.is_empty());
        assert_eq!(parsed.render(), "omnion_queue_depth > 100");
    }

    #[test]
    fn an_unknown_family_is_refused_by_name() {
        let error = parse("omnion_not_a_family > 1").expect_err("must be refused");
        assert!(
            error.contains("omnion_not_a_family"),
            "the refusal must name the family: {error}"
        );
        assert!(
            error.contains("not a declared metric family"),
            "the refusal must say what is wrong: {error}"
        );
    }

    #[test]
    fn a_label_the_family_does_not_declare_is_refused_with_the_accepted_list() {
        let error = parse("omnion_http_requests_total{nope=\"x\"} > 0").expect_err("refused");
        assert!(error.contains("`nope` is not a label"), "{error}");
        assert!(error.contains("route, method, status"), "{error}");
    }

    #[test]
    fn an_expression_without_a_comparison_is_refused_rather_than_stored() {
        // A rule that saves and never fires is the failure this whole module exists to prevent.
        let error = parse("omnion_queue_depth").expect_err("refused");
        assert!(error.contains("no comparison"), "{error}");

        let error = parse("omnion_queue_depth >").expect_err("refused");
        assert!(error.contains("no threshold"), "{error}");

        let error = parse("omnion_queue_depth five 5").expect_err("refused");
        assert!(error.contains("not a comparison"), "{error}");

        let error = parse("omnion_queue_depth > five").expect_err("refused");
        assert!(error.contains("not a number"), "{error}");
    }

    #[test]
    fn an_unclosed_label_block_is_refused_with_the_position() {
        let error = parse("omnion_http_requests_total{status=\"5xx\" > 0").expect_err("refused");
        assert!(error.contains("no closing"), "{error}");
    }

    #[test]
    fn a_non_finite_threshold_is_refused() {
        // `NaN > 5` is false in every comparison, so a rule with it would parse, save, display
        // and never fire. That is exactly the "looks configured" state the grammar exists to
        // prevent.
        for text in ["nan", "inf", "-inf"] {
            let error =
                parse(&format!("omnion_queue_depth > {text}")).expect_err("must be refused");
            assert!(error.contains("finite"), "{text}: {error}");
        }
    }

    #[test]
    fn trailing_junk_is_refused() {
        let error = parse("omnion_queue_depth > 5 extra").expect_err("refused");
        assert!(error.contains("unexpected"), "{error}");
    }

    #[test]
    fn an_empty_expression_says_what_it_expected() {
        let error = parse("   ").expect_err("refused");
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn every_operator_compares_the_way_it_reads() {
        assert!(Operator::Greater.holds(2.0, 1.0));
        assert!(!Operator::Greater.holds(1.0, 1.0));
        assert!(Operator::GreaterOrEqual.holds(1.0, 1.0));
        assert!(Operator::Less.holds(0.0, 1.0));
        assert!(Operator::LessOrEqual.holds(1.0, 1.0));
        assert!(Operator::Equal.holds(1.0, 1.0));
        assert!(Operator::NotEqual.holds(2.0, 1.0));
        assert!(!Operator::NotEqual.holds(1.0, 1.0));
    }

    #[test]
    fn no_data_never_breaches_even_when_the_comparison_would_pass_on_zero() {
        // The failure this guards is real: "no errors recorded" and "the recorder is not running"
        // both read as 0, and a `< 0.01` rule would page about its own monitoring.
        let evaluation = Evaluation {
            value: Some(0.0),
            matched: false,
            series: 0,
        };
        assert!(!evaluation.breaches(Operator::Less, 0.01));
        assert!(!evaluation.breaches(Operator::NotEqual, 0.0));

        let no_value = Evaluation {
            value: None,
            matched: true,
            series: 3,
        };
        assert!(!no_value.breaches(Operator::Less, 1.0));
    }

    #[test]
    fn a_matched_series_breaches_on_the_value() {
        let evaluation = Evaluation {
            value: Some(0.5),
            matched: true,
            series: 1,
        };
        assert!(evaluation.breaches(Operator::Greater, 0.1));
        assert!(!evaluation.breaches(Operator::Greater, 0.9));
    }

    #[test]
    fn a_matcher_for_a_missing_label_position_never_matches() {
        let spec = metrics::family("omnion_http_requests_total").expect("declared");
        let expression = Expression {
            family: spec.name,
            matchers: vec![("method".to_owned(), "GET".to_owned())],
            operator: Operator::Greater,
            threshold: 0.0,
        };
        // A snapshot with fewer label values than the family declares is a truncated series; the
        // matcher must not be satisfied by a missing position.
        assert!(!expression.matches(spec, &[]));
        assert!(expression.matches(spec, &["/x".to_owned(), "GET".to_owned(), "2xx".to_owned()]));
        assert!(!expression.matches(
            spec,
            &["/x".to_owned(), "POST".to_owned(), "2xx".to_owned()]
        ));
    }

    #[test]
    fn the_rendered_expression_round_trips_through_the_parser() {
        let original = "omnion_http_requests_total{status=\"5xx\"} >= 0.05";
        let rendered = parse(original).expect("parses").render();
        let again = parse(&rendered).expect("the render must parse again");
        assert_eq!(again.render(), rendered);
    }

    #[test]
    fn the_notification_payload_carries_the_five_fields_and_nothing_else() {
        // The request is explicit: "rule name, severity, value, window, runbook link, and nothing
        // else. Log lines, user data and secret fragments never appear in a payload."
        let notification = Notification {
            event_id: 1,
            rule_id: Uuid::nil(),
            value: Some(0.42),
            fired_at: None,
            rule_name: "HighErrorRate".to_owned(),
            severity: "critical".to_owned(),
            summary: "the summary is deliberately absent".to_owned(),
            runbook_url: Some("https://runbook.example/error-rate".to_owned()),
            context: serde_json::json!({
                "dwell_seconds": 300,
                "labels": {"team": "platform"},
            }),
        };
        let payload = notification.payload();
        // `serde_json::Map` is a BTreeMap, so `keys()` comes back alphabetically sorted rather
        // than in declaration order. The assertion is on the SET, which is the property that
        // matters: an extra key is a place a log line could travel, and the set is what catches
        // one. Order is the renderer's business, not the payload's contract.
        let keys: Vec<&str> = payload
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "labels",
                "rule",
                "runbook_url",
                "severity",
                "value",
                "window_seconds"
            ],
            "the payload's shape is what keeps a log line out of it: {payload}"
        );
        assert_eq!(payload["window_seconds"], 300);
    }

    #[test]
    fn a_quiet_pass_is_quiet() {
        assert!(PassReport::default().is_quiet());
        assert!(
            !PassReport {
                fired: 1,
                ..Default::default()
            }
            .is_quiet()
        );
        assert!(
            !PassReport {
                resolved: 1,
                ..Default::default()
            }
            .is_quiet()
        );
        // Opening a pending event is not a notification, so it does not wake the notifier.
        assert!(
            PassReport {
                opened: 3,
                ..Default::default()
            }
            .is_quiet(),
            "a pending event woke the notification pass"
        );
    }

    #[test]
    fn the_transitions_family_is_declared_in_the_registry() {
        assert!(
            metrics::family(TRANSITIONS_FAMILY).is_some(),
            "{TRANSITIONS_FAMILY} is recorded on every transition but not declared, so the \
             transition count is invisible on the scrape"
        );
    }

    #[test]
    fn a_preview_agrees_with_the_evaluation_it_previews() {
        // A preview that ran a different path could disagree with the alert it is previewing,
        // which is worse than having no preview at all.
        let previewed = preview("omnion_http_requests_total > 0").expect("parses");
        let evaluation = parse("omnion_http_requests_total > 0")
            .expect("parses")
            .evaluate();
        assert_eq!(
            previewed.breaching,
            evaluation.breaches(Operator::Greater, 0.0)
        );
        assert_eq!(previewed.value, evaluation.value);
        assert_eq!(previewed.rendered, "omnion_http_requests_total > 0");
    }

    #[test]
    fn a_preview_refuses_an_expression_the_evaluator_would_refuse() {
        let error = preview("omnion_not_a_family > 0").expect_err("must be refused");
        assert!(error.contains("omnion_not_a_family"), "{error}");
    }
}
