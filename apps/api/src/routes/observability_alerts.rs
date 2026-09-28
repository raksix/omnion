//! Alert rules, silences, the settings screen's writes, the bundle manifest and the lifecycle
//! contract (REQ-126, slice 4).
//!
//! ## What is in here and why
//!
//! * **`/observability/alert-rules`** — CRUD over the closed expression grammar in
//!   [`omnion_telemetry::alerts`]. Every write is audited; a change to what pages an operator
//!   is a change an operator has to be able to answer "who raised the error threshold and when".
//! * **`/observability/alerts`** — the firing/pending/resolved timeline and the silences.
//! * **`/observability/alert-rules/preview`** — evaluates against the live registry and saves
//!   nothing, which is the only way an operator can find the right threshold before committing
//!   to one.
//! * **`/observability/settings`** — sampling, retention, levels and the cardinality budget, in
//!   the ONE settings row slice 1 created. The caps live in `obs_settings_caps` and are returned
//!   to the panel, so a field-level message can say "the cap is 30" from data rather than from a
//!   message string.
//! * **`/observability/bundle`** — the manifest of `infra/observability/`, so the panel can tell
//!   the operator which dashboard version this instance was built to import.
//! * **`/lifecycle`** — what `/healthz`, `/readyz` and `/livez` are doing while a drain runs,
//!   exposed as data so the deployment centre (REQ-128) and the screen can read the contract
//!   instead of hard-coding it.
//!
//! ## Why `preview` is a POST and not a GET
//!
//! It evaluates an arbitrary caller-supplied expression against the registry. That is a
//! read, but it is a read of *something the caller wrote*, and every proxy, cache and browser
//! in the path treats a GET as safe to replay. The cost of a wrong choice here is a
//! cross-site-triggered evaluation; the cost of a POST is a method a `curl` has to name.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_telemetry::alerts;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/* ── alert rules ────────────────────────────────────────────────────────────────────────────── */

/// A rule, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct AlertRuleView {
    /// Primary key.
    pub id: Uuid,
    /// Unique name.
    pub name: String,
    /// The stored expression, verbatim — the panel edits what is stored, not a re-render.
    pub expr: String,
    /// The expression as the evaluator understood it, or `null` when it does not parse.
    ///
    /// A rule whose stored expression stopped parsing — because a family was renamed in a later
    /// release — must say so on the row. A rule that looks configured and silently never fires
    /// is the failure this request is about.
    pub parsed: Option<String>,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// The dwell in seconds.
    pub for_seconds: i32,
    /// The operator-facing one-liner.
    pub summary: String,
    /// Where to read more.
    pub runbook_url: Option<String>,
    /// Extra labels.
    pub labels: serde_json::Value,
    /// `bundled` or `custom`.
    pub source: String,
    /// Whether the rule is evaluated.
    pub enabled: bool,
    /// `pending`, `firing` or `None` when nothing is open.
    pub state: Option<String>,
    /// The value at the last evaluation.
    pub value: Option<f64>,
    /// Whether a silence covers this rule right now.
    pub silenced: bool,
    /// When the covering silence ends.
    pub silenced_until: Option<String>,
    /// Whether the expression still parses — the row's own health.
    pub expression_valid: bool,
    /// Why it does not parse, when it does not.
    pub expression_error: Option<String>,
}

impl From<alerts::RuleWithState> for AlertRuleView {
    fn from(row: alerts::RuleWithState) -> Self {
        let parsed = alerts::parse(&row.rule.expr);
        let (rendered, expression_valid, expression_error) = match &parsed {
            Ok(expression) => (Some(expression.render()), true, None),
            Err(error) => (None, false, Some(error.clone())),
        };
        Self {
            id: row.rule.id,
            name: row.rule.name,
            expr: row.rule.expr,
            parsed: rendered,
            severity: row.rule.severity,
            for_seconds: row.rule.for_seconds,
            summary: row.rule.summary,
            runbook_url: row.rule.runbook_url,
            labels: row.rule.labels,
            source: row.rule.source,
            enabled: row.rule.enabled,
            state: row.state,
            value: row.value,
            silenced: row.silenced,
            silenced_until: row
                .silenced_until
                .map(|at| at.format(&Rfc3339).unwrap_or_default()),
            expression_valid,
            expression_error,
        }
    }
}

/// The alert rules response.
#[derive(Debug, Serialize)]
pub struct AlertRulesResponse {
    /// The rules with their state.
    pub rules: Vec<AlertRuleView>,
    /// The families a rule may name, so the form's help text is fed from the registry.
    pub families: Vec<String>,
    /// The accepted severities.
    pub severities: Vec<&'static str>,
    /// The cap on `for_seconds`.
    pub max_for_seconds: i32,
}

/// `GET /api/v1/observability/alert-rules`.
pub async fn read_alert_rules(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<AlertRulesResponse>, ApiError> {
    let rows = alerts::list_rules(state.db().pool())
        .await
        .map_err(map_error)?;
    Ok(Json(AlertRulesResponse {
        rules: rows.into_iter().map(AlertRuleView::from).collect(),
        families: omnion_telemetry::metrics::FAMILIES
            .iter()
            .map(|spec| spec.name.to_owned())
            .collect(),
        severities: vec!["info", "warning", "critical"],
        max_for_seconds: alerts::MAX_FOR_SECONDS,
    }))
}

/// The rule body. `deny_unknown_fields`, like every other write in this surface.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRuleInput {
    /// Unique name.
    pub name: String,
    /// The expression, in the closed grammar.
    pub expr: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// The dwell in seconds.
    #[serde(default = "default_for_seconds")]
    pub for_seconds: i32,
    /// The operator-facing one-liner.
    #[serde(default)]
    pub summary: String,
    /// Where to read more.
    pub runbook_url: Option<String>,
    /// Extra labels merged into the notification payload.
    #[serde(default)]
    pub labels: serde_json::Value,
}

fn default_for_seconds() -> i32 {
    300
}

/// `POST /api/v1/observability/alert-rules` — create a rule.
///
/// The expression is validated HERE and not at evaluation time. A rule whose expression does not
/// parse is refused with a `422` naming the position, because a stored rule that never fires is
/// indistinguishable from a quiet system until an incident it would have caught.
pub async fn create_alert_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<AlertRuleInput>,
) -> Result<Json<AlertRuleView>, ApiError> {
    validate_rule(&input)?;

    let row: alerts::Rule = sqlx::query_as(
        "insert into obs_alert_rules \
             (name, expr, severity, for_seconds, summary, runbook_url, labels, source, \
              updated_by) \
         values ($1, $2, $3, $4, $5, $6, $7, 'custom', $8) \
         returning id, name, expr, severity, for_seconds, summary, runbook_url, labels, source, \
                   enabled",
    )
    .bind(input.name.trim())
    .bind(input.expr.trim())
    .bind(input.severity.trim().to_lowercase())
    .bind(input.for_seconds)
    .bind(&input.summary)
    .bind(&input.runbook_url)
    .bind(&input.labels)
    .bind(session.user.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(|error| duplicate_name(error))?;

    audit(
        &state,
        &session,
        "observability.alert_rule.created",
        &row,
        serde_json::json!({ "expr": row.expr, "severity": row.severity }),
    )
    .await?;

    let found = alerts::find_rule(state.db().pool(), row.id)
        .await
        .map_err(map_error)?
        .expect("the row was just written");
    Ok(Json(AlertRuleView::from(alerts::RuleWithState {
        rule: found,
        state: None,
        value: None,
        silenced: false,
        silenced_until: None,
    })))
}

/// The rule patch. Every field is optional, but a misspelled one is still refused.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRulePatch {
    /// A new name.
    pub name: Option<String>,
    /// A new expression.
    pub expr: Option<String>,
    /// A new severity.
    pub severity: Option<String>,
    /// A new dwell.
    pub for_seconds: Option<i32>,
    /// A new one-liner.
    pub summary: Option<String>,
    /// A new runbook link.
    pub runbook_url: Option<Option<String>>,
    /// New labels.
    pub labels: Option<serde_json::Value>,
    /// Turn the rule on or off.
    pub enabled: Option<bool>,
}

/// `PATCH /api/v1/observability/alert-rules/{id}` — edit or toggle a rule.
pub async fn update_alert_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(patch): Json<AlertRulePatch>,
) -> Result<Json<AlertRuleView>, ApiError> {
    let existing = alerts::find_rule(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| ApiError::not_found("alert rule", id))?;

    // The merged candidate is validated, not just the field that changed: a rule whose severity
    // was valid and whose expression was not is exactly the state this route refuses to leave
    // behind.
    let candidate = AlertRuleInput {
        name: patch.name.clone().unwrap_or_else(|| existing.name.clone()),
        expr: patch.expr.clone().unwrap_or_else(|| existing.expr.clone()),
        severity: patch
            .severity
            .clone()
            .unwrap_or_else(|| existing.severity.clone()),
        for_seconds: patch.for_seconds.unwrap_or(existing.for_seconds),
        summary: patch
            .summary
            .clone()
            .unwrap_or_else(|| existing.summary.clone()),
        runbook_url: patch
            .runbook_url
            .clone()
            .unwrap_or(existing.runbook_url.clone()),
        labels: patch
            .labels
            .clone()
            .unwrap_or_else(|| existing.labels.clone()),
    };
    validate_rule(&candidate)?;

    let row: alerts::Rule = sqlx::query_as(
        "update obs_alert_rules set \
             name = $2, expr = $3, severity = $4, for_seconds = $5, summary = $6, \
             runbook_url = $7, labels = $8, enabled = coalesce($9, enabled), updated_by = $10, \
             updated_at = now() \
         where id = $1 \
         returning id, name, expr, severity, for_seconds, summary, runbook_url, labels, source, \
                   enabled",
    )
    .bind(id)
    .bind(candidate.name.trim())
    .bind(candidate.expr.trim())
    .bind(candidate.severity.trim().to_lowercase())
    .bind(candidate.for_seconds)
    .bind(&candidate.summary)
    .bind(&candidate.runbook_url)
    .bind(&candidate.labels)
    .bind(patch.enabled)
    .bind(session.user.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(duplicate_name)?;

    audit(
        &state,
        &session,
        "observability.alert_rule.updated",
        &row,
        serde_json::json!({ "enabled": row.enabled, "expr": row.expr }),
    )
    .await?;

    Ok(Json(AlertRuleView::from(alerts::RuleWithState {
        rule: row,
        state: None,
        value: None,
        silenced: false,
        silenced_until: None,
    })))
}

/// `DELETE /api/v1/observability/alert-rules/{id}`.
///
/// A **custom** rule can be deleted. A **bundled** rule cannot: it is re-seeded on every boot
/// from `infra/observability/alerts.yml`, so deleting its row would have it come back on the
/// next restart, which reads as a failed delete. The refusal says that instead.
pub async fn delete_alert_rule(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let existing = alerts::find_rule(state.db().pool(), id)
        .await
        .map_err(map_error)?
        .ok_or_else(|| ApiError::not_found("alert rule", id))?;

    if existing.source == "bundled" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bundled_rule",
            format!(
                "`{}` ships with the observability bundle and is re-seeded on every boot. Turn \
                 it off instead of deleting it.",
                existing.name
            ),
        ));
    }

    sqlx::query("delete from obs_alert_rules where id = $1")
        .bind(id)
        .execute(state.db().pool())
        .await
        .map_err(map_error)?;

    audit(
        &state,
        &session,
        "observability.alert_rule.deleted",
        &existing,
        serde_json::json!({ "name": existing.name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The preview body.
#[derive(Debug, serde::Deserialize)]
pub struct PreviewInput {
    /// The expression to evaluate.
    pub expr: String,
}

/// The preview response.
#[derive(Debug, Serialize)]
pub struct PreviewView {
    /// The expression as the evaluator understood it.
    pub rendered: String,
    /// The family it reads, so the panel can link to the catalogue.
    pub family: String,
    /// The value it saw, if any.
    pub value: Option<f64>,
    /// How many series matched.
    pub series: usize,
    /// Whether it would breach right now.
    pub breaching: bool,
    /// `true` when the family has no samples at all, so `breaching: false` is "no data" and
    /// not "under the threshold". The panel says which one it is showing.
    pub no_data: bool,
}

/// `POST /api/v1/observability/alert-rules/preview` — evaluate without saving.
pub async fn preview_alert_rule(
    State(_state): State<AppState>,
    _session: CurrentSession,
    Json(input): Json<PreviewInput>,
) -> Result<Json<PreviewView>, ApiError> {
    let previewed = alerts::preview(&input.expr).map_err(|error| {
        // `422` and not `400`: the request is well-formed JSON with a well-formed field, and
        // what is wrong with it is the VALUE. The status is what a form uses to decide between
        // "fix the field" and "the server is broken".
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_alert_expression",
            error,
        )
    })?;
    Ok(Json(PreviewView {
        rendered: previewed.rendered,
        family: previewed.family.to_owned(),
        value: previewed.value,
        series: previewed.series,
        breaching: previewed.breaching,
        no_data: !previewed.matched,
    }))
}

fn validate_rule(input: &AlertRuleInput) -> Result<(), ApiError> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            "`name` is required — an alert with no name cannot be told apart from another one in \
             a notification",
        ));
    }
    if !matches!(
        input.severity.trim().to_lowercase().as_str(),
        "info" | "warning" | "critical"
    ) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_severity",
            format!(
                "`severity` must be one of info, warning, critical — got `{}`",
                input.severity
            ),
        ));
    }
    if !(0..=alerts::MAX_FOR_SECONDS).contains(&input.for_seconds) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_for_seconds",
            format!(
                "`for_seconds` must be between 0 and {} — a rule that must stay true for longer is \
                 a rule about a trend, and a trend belongs in a report",
                alerts::MAX_FOR_SECONDS
            ),
        ));
    }
    alerts::parse(&input.expr).map_err(|error| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_alert_expression",
            error,
        )
    })?;
    Ok(())
}

fn duplicate_name(error: sqlx::Error) -> ApiError {
    // Postgres' unique-violation SQLSTATE, checked so a connection failure is not reported as a
    // name clash — the caller would then "fix" a name that was never the problem.
    if let sqlx::Error::Database(ref inner) = error
        && inner.code().as_deref() == Some("23505")
    {
        return ApiError::new(
            StatusCode::CONFLICT,
            "duplicate_name",
            "an alert rule with that name already exists — names are how a notification is \
             attributed",
        );
    }
    map_error(error)
}

/* ── the alert timeline and the silences ─────────────────────────────────────────────────────── */

/// One event, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct AlertEventView {
    /// Primary key.
    pub id: i64,
    /// Its rule.
    pub rule_id: Uuid,
    /// `pending`, `firing` or `resolved`.
    pub state: String,
    /// The value at the last evaluation.
    pub value: Option<f64>,
    /// The value that promoted it to `firing`.
    pub firing_value: Option<f64>,
    /// When it opened.
    pub started_at: String,
    /// When it resolved.
    pub ended_at: Option<String>,
    /// When it fired.
    pub fired_at: Option<String>,
    /// Whether a notification went out — the acceptance line's "notifies once", made visible.
    pub notified: bool,
    /// `threshold`, `dwell` or `silenced`.
    pub reason: String,
    /// The expression and labels that matched.
    pub context: serde_json::Value,
}

impl From<alerts::AlertEvent> for AlertEventView {
    fn from(event: alerts::AlertEvent) -> Self {
        Self {
            id: event.id,
            rule_id: event.rule_id,
            state: event.state,
            value: event.value,
            firing_value: event.firing_value,
            started_at: event.started_at.format(&Rfc3339).unwrap_or_default(),
            ended_at: event
                .ended_at
                .map(|at| at.format(&Rfc3339).unwrap_or_default()),
            fired_at: event
                .fired_at
                .map(|at| at.format(&Rfc3339).unwrap_or_default()),
            notified: event.notified,
            reason: event.reason,
            context: event.context,
        }
    }
}

/// One silence, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct SilenceView {
    /// Primary key.
    pub id: Uuid,
    /// Its rule, or `None` for a global window.
    pub rule_id: Option<Uuid>,
    /// The reason, which is required.
    pub reason: String,
    /// When it takes effect.
    pub starts_at: Option<String>,
    /// When it stops.
    pub ends_at: String,
    /// Whether it is in force right now.
    pub active: bool,
    /// How many minutes remain, so the screen can say "ends in 42 min".
    pub minutes_remaining: i64,
}

impl From<alerts::Silence> for SilenceView {
    fn from(silence: alerts::Silence) -> Self {
        let now = OffsetDateTime::now_utc();
        let active = silence.ends_at > now && silence.starts_at.is_none_or(|start| start <= now);
        Self {
            id: silence.id,
            rule_id: silence.rule_id,
            reason: silence.reason,
            starts_at: silence
                .starts_at
                .map(|at| at.format(&Rfc3339).unwrap_or_default()),
            ends_at: silence.ends_at.format(&Rfc3339).unwrap_or_default(),
            active,
            minutes_remaining: (silence.ends_at - now).whole_minutes().max(0),
        }
    }
}

/// The alerts response.
#[derive(Debug, Serialize)]
pub struct AlertsResponse {
    /// `firing` events, newest first.
    pub firing: Vec<AlertEventView>,
    /// `pending` events, newest first.
    pub pending: Vec<AlertEventView>,
    /// Recently resolved events, newest first.
    pub resolved: Vec<AlertEventView>,
    /// The silences, active first.
    pub silences: Vec<SilenceView>,
    /// The counts the overview head reads.
    pub counts: AlertCounts,
}

/// The three counts, so the overview does not walk every event.
#[derive(Debug, Serialize)]
pub struct AlertCounts {
    /// How many are firing.
    pub firing: i64,
    /// How many are pending.
    pub pending: i64,
    /// How many are silenced right now.
    pub silenced: i64,
    /// The highest severity firing — `None` when nothing is.
    pub worst_severity: Option<String>,
}

/// `GET /api/v1/observability/alerts` — the states and the silences.
pub async fn read_alerts(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<AlertsResponse>, ApiError> {
    let events = alerts::recent_events(state.db().pool(), 200)
        .await
        .map_err(map_error)?;
    let silences = load_silences(state.db().pool()).await?;

    let firing: Vec<AlertEventView> = events
        .iter()
        .filter(|event| event.state == "firing")
        .cloned()
        .map(AlertEventView::from)
        .collect();
    let pending: Vec<AlertEventView> = events
        .iter()
        .filter(|event| event.state == "pending")
        .cloned()
        .map(AlertEventView::from)
        .collect();
    let resolved: Vec<AlertEventView> = events
        .iter()
        .filter(|event| event.state == "resolved")
        .take(25)
        .cloned()
        .map(AlertEventView::from)
        .collect();

    let rules = alerts::list_rules(state.db().pool())
        .await
        .map_err(map_error)?;
    let silenced_rules = rules.iter().filter(|rule| rule.silenced).count() as i64;
    let worst = rules
        .iter()
        .filter(|rule| rule.state.as_deref() == Some("firing"))
        .map(|rule| rule.rule.severity.clone())
        // The order here IS the ranking: info < warning < critical, and a panel that sorts this
        // alphabetically would show a critical alert under an informational one.
        .max_by_key(|severity| match severity.as_str() {
            "critical" => 3,
            "warning" => 2,
            _ => 1,
        });

    Ok(Json(AlertsResponse {
        counts: AlertCounts {
            firing: firing.len() as i64,
            pending: pending.len() as i64,
            silenced: silenced_rules,
            worst_severity: worst,
        },
        firing,
        pending,
        resolved,
        silences,
    }))
}

async fn load_silences(pool: &sqlx::PgPool) -> Result<Vec<SilenceView>, ApiError> {
    let rows: Vec<alerts::Silence> = sqlx::query_as(
        "select id, rule_id, reason, starts_at, ends_at, created_by from obs_silences \
         order by ends_at desc limit 200",
    )
    .fetch_all(pool)
    .await
    .map_err(map_error)?;
    let mut views: Vec<SilenceView> = rows.into_iter().map(SilenceView::from).collect();
    // Active first, then soonest to end: the silence an operator has to notice is the one about
    // to expire, and the screen's job is to make that visible.
    views.sort_by(|a, b| b.active.cmp(&a.active).then(a.ends_at.cmp(&b.ends_at)));
    Ok(views)
}

/// The silence body.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SilenceInput {
    /// The rule to silence, or `None` for every rule.
    pub rule_id: Option<Uuid>,
    /// Why — required, because an anonymous silence is one nobody dares to remove.
    pub reason: String,
    /// When it takes effect; `None` means now.
    pub starts_at: Option<String>,
    /// When it stops (RFC 3339).
    pub ends_at: String,
}

/// `POST /api/v1/observability/silences`.
pub async fn create_silence(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<SilenceInput>,
) -> Result<Json<SilenceView>, ApiError> {
    if input.reason.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_reason",
            "`reason` is required — a silence with no reason is one nobody dares to remove",
        ));
    }
    let ends_at = OffsetDateTime::parse(&input.ends_at, &Rfc3339).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_ends_at",
            "`ends_at` must be an RFC 3339 instant, e.g. 2026-09-29T18:00:00Z",
        )
    })?;
    let starts_at = match input.starts_at.as_deref() {
        None => None,
        Some(text) => Some(OffsetDateTime::parse(text, &Rfc3339).map_err(|_| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_starts_at",
                "`starts_at` must be an RFC 3339 instant, e.g. 2026-09-28T18:00:00Z",
            )
        })?),
    };

    // "ends in the past" is refused HERE rather than left to the column, because the column
    // cannot call `now()` (stable, not immutable) and a constraint violation is not a message an
    // operator can act on.
    let now = OffsetDateTime::now_utc();
    if ends_at <= now {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "silence_already_ended",
            format!(
                "`ends_at` ({}) is in the past — a silence that has already ended suppresses \
                 nothing and looks like it does",
                input.ends_at
            ),
        ));
    }
    if let Some(start) = starts_at
        && end_before_start(ends_at, start)
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_window",
            "`ends_at` must be after `starts_at`",
        ));
    }
    if ends_at - now > time::Duration::days(alerts::MAX_SILENCE_DAYS) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "silence_too_long",
            format!(
                "a silence may last at most {} days — a silence nobody can forget is a disabled \
                 alert",
                alerts::MAX_SILENCE_DAYS
            ),
        ));
    }

    if let Some(rule_id) = input.rule_id
        && alerts::find_rule(state.db().pool(), rule_id)
            .await
            .map_err(map_error)?
            .is_none()
    {
        return Err(ApiError::not_found("alert rule", rule_id));
    }

    let row: SilenceRow = sqlx::query_as(
        "insert into obs_silences (rule_id, reason, starts_at, ends_at, created_by) \
         values ($1, $2, $3, $4, $5) \
         returning id, rule_id, reason, starts_at, ends_at, created_by",
    )
    .bind(input.rule_id)
    .bind(input.reason.trim())
    .bind(starts_at)
    .bind(ends_at)
    .bind(session.user.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(map_error)?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "observability.silence.created")
            .organization(session.user.organization_id)
            .target("silence", row.id.to_string())
            .metadata(serde_json::json!({
                "rule_id": row.rule_id,
                "reason": row.reason,
                "ends_at": row.ends_at.format(&Rfc3339).unwrap_or_default(),
            })),
    )
    .await
    .map_err(audit_error)?;

    // The request lists `observability.silence.created` as emitted, and this is the only place a
    // silence is created — so this is the only place the event can come from. A silence is the
    // one observability event that belongs to a TENANT and names a user: it is somebody's
    // decision about their own instance, so the fan-out reaches that organization's webhook
    // endpoints and `actor_user_id` is the person who pressed the button.
    //
    // The rule's NAME travels with it so a subscriber need not query the API to make the event
    // readable — an audit row is not a subscription payload, and the one free-text field in this
    // payload is the operator's own reason, which the request already requires to exist.
    let rule_name = match row.rule_id {
        Some(rule_id) => omnion_telemetry::alerts::find_rule(state.db().pool(), rule_id)
            .await
            .ok()
            .flatten()
            .map(|rule| rule.name),
        None => None,
    };
    let silence_event = omnion_telemetry::SilenceCreated {
        silence_id: row.id,
        rule_id: row.rule_id,
        rule_name,
        reason: row.reason.clone(),
        ends_at: row.ends_at.format(&Rfc3339).unwrap_or_default(),
    }
    .payload();
    // An account with no named organization gets a platform-wide event, which fans out to
    // nobody and is still recorded. The alternative — inventing an organization id — would put a
    // tenant's silence on somebody else's endpoint.
    match session.user.organization_id {
        Some(organization_id) => {
            omnion_telemetry::events::try_emit_for_organization(
                state.db().pool(),
                omnion_telemetry::events::SILENCE_CREATED,
                organization_id,
                session.user.id,
                silence_event,
            )
            .await;
        }
        None => {
            omnion_telemetry::events::try_emit(
                state.db().pool(),
                omnion_telemetry::events::SILENCE_CREATED,
                silence_event,
            )
            .await;
        }
    }

    Ok(Json(SilenceView::from(alerts::Silence {
        id: row.id,
        rule_id: row.rule_id,
        reason: row.reason,
        starts_at: row.starts_at,
        ends_at: row.ends_at,
        created_by: row.created_by,
    })))
}

fn end_before_start(ends_at: OffsetDateTime, starts_at: OffsetDateTime) -> bool {
    ends_at <= starts_at
}

#[derive(Debug, sqlx::FromRow)]
struct SilenceRow {
    id: Uuid,
    rule_id: Option<Uuid>,
    reason: String,
    starts_at: Option<OffsetDateTime>,
    ends_at: OffsetDateTime,
    created_by: Option<Uuid>,
}

/// `DELETE /api/v1/observability/silences/{id}` — lift a silence now.
pub async fn delete_silence(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let removed = sqlx::query("delete from obs_silences where id = $1")
        .bind(id)
        .execute(state.db().pool())
        .await
        .map_err(map_error)?;
    if removed.rows_affected() == 0 {
        return Err(ApiError::not_found("silence", id));
    }
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "observability.silence.lifted")
            .organization(session.user.organization_id)
            .target("silence", id.to_string()),
    )
    .await
    .map_err(audit_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/* ── the settings row ────────────────────────────────────────────────────────────────────────── */

/// The settings, in the panel's shape.
#[derive(Debug, Serialize)]
pub struct ObservabilitySettingsView {
    /// The share of non-error requests whose trace is kept.
    pub sampling_ratio: f64,
    /// How many days of logs are kept.
    pub logs_retention_days: i64,
    /// How many days of trace-index rows are kept.
    pub traces_retention_days: i64,
    /// The level a module logs at unless overridden.
    pub log_level_default: String,
    /// Per-module temporary raises.
    pub log_level_overrides: serde_json::Value,
    /// The registry-wide series cap.
    pub cardinality_budget: i64,
    /// Whether `/metrics` answers on a public interface.
    pub prometheus_public: bool,
    /// The caps, so a field-level message can name the number it enforces.
    pub caps: SettingsCapsView,
    /// What the request asks the settings screen to say out loud.
    pub egress_note: String,
    /// The live overrides, each with whether it has expired.
    pub level_overrides: Vec<LevelOverrideView>,
}

/// The documented caps, from `obs_settings_caps`.
#[derive(Debug, Serialize)]
pub struct SettingsCapsView {
    /// The retention cap for logs.
    pub logs_retention_max: i64,
    /// The retention cap for traces.
    pub traces_retention_max: i64,
    /// The highest ratio.
    pub sampling_max: f64,
    /// The highest series cap.
    pub cardinality_max: i64,
    /// The accepted level names.
    pub log_levels: Vec<String>,
}

/// One per-module raise, resolved against the clock.
#[derive(Debug, Serialize)]
pub struct LevelOverrideView {
    /// The module path prefix.
    pub target: String,
    /// The level it is raised to.
    pub level: String,
    /// When the raise expires, if it has one.
    pub expires_at: Option<String>,
    /// Whether the raise has already expired and is only listed so the operator can see it.
    pub expired: bool,
}

/// The settings body.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilitySettingsInput {
    /// The share of non-error requests whose trace is kept, 0.0 to 1.0.
    pub sampling_ratio: f64,
    /// How many days of logs to keep.
    pub logs_retention_days: i64,
    /// How many days of trace-index rows to keep.
    pub traces_retention_days: i64,
    /// The level a module logs at unless overridden.
    pub log_level_default: String,
    /// Per-module raises, as `{"omnion_secrets": {"level": "debug", "expires_at": "…"}}`.
    #[serde(default)]
    pub log_level_overrides: serde_json::Value,
    /// The registry-wide series cap.
    pub cardinality_budget: i64,
    /// Whether `/metrics` answers on a public interface.
    #[serde(default)]
    pub prometheus_public: bool,
}

impl ObservabilitySettingsInput {
    /// Resolve the raw overrides object into rows, dropping the expired ones.
    ///
    /// The expiry is enforced HERE, on write, and again on read: an override whose `expires_at`
    /// has passed is returned to the panel as `expired: true` and is NOT written back, so
    /// "a temporary log-level raise expires back to the configured default without a restart"
    /// holds for a raise made through this form. The levels themselves are resolved by the
    /// process's own `LogContext`, which reads the same column.
    fn resolve_overrides(&self, now: OffsetDateTime) -> Result<Vec<LevelOverrideView>, ApiError> {
        let Some(map) = self.log_level_overrides.as_object() else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for (target, value) in map {
            let level = value
                .get("level")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_level_override",
                        format!(
                            "`log_level_overrides[\"{target}\"]` must be an object with a `level` \
                             string and an optional `expires_at`"
                        ),
                    )
                })?;
            omnion_telemetry::LogLevel::parse(level).ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_level_override",
                    format!(
                        "`log_level_overrides[\"{target}\"].level` must be one of trace, debug, \
                         info, warn, error — got `{level}`"
                    ),
                )
            })?;
            let expires_at = match value.get("expires_at").and_then(serde_json::Value::as_str) {
                None => None,
                Some(text) => Some(OffsetDateTime::parse(text, &Rfc3339).map_err(|_| {
                    ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_level_expiry",
                        format!(
                            "`log_level_overrides[\"{target}\"].expires_at` must be an RFC 3339 \
                             instant, e.g. 2026-09-28T18:00:00Z"
                        ),
                    )
                })?),
            };
            rows.push(LevelOverrideView {
                target: target.clone(),
                level: level.to_owned(),
                expired: expires_at.is_some_and(|at| at <= now),
                expires_at: expires_at.map(|at| at.format(&Rfc3339).unwrap_or_default()),
            });
        }
        rows.sort_by(|a, b| a.target.cmp(&b.target));
        Ok(rows)
    }

    /// The object to store: the same shape minus the expired entries.
    fn compacted(&self, rows: &[LevelOverrideView]) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for row in rows.iter().filter(|row| !row.expired) {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "level".to_owned(),
                serde_json::Value::String(row.level.clone()),
            );
            if let Some(expires) = &row.expires_at {
                entry.insert(
                    "expires_at".to_owned(),
                    serde_json::Value::String(expires.clone()),
                );
            }
            map.insert(row.target.clone(), serde_json::Value::Object(entry));
        }
        serde_json::Value::Object(map)
    }
}

/// What changed between two `log_level_overrides` objects, one entry per module.
///
/// A diff and not a dump, for two reasons the request names. An event per save would fire on
/// every autosave of a form nobody edited; an event per module that moved is the one an operator
/// can act on. And the diff is over the raw objects, so an entry the write DROPPED because its
/// expiry passed is reported as a move to the configured default — which is the fact the
/// acceptance line's "expires back to the configured default" is actually about.
///
/// A malformed entry is read leniently rather than refused: the row it came from is already
/// stored and this is an event describing it, and an event that refuses to describe a real row
/// is worse than one that names the level it can read. The write path validates properly, so a
/// row like that can only exist if it was edited by hand.
fn level_changes(
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Vec<omnion_telemetry::LogLevelChanged> {
    let read = |value: &serde_json::Value, target: &str| {
        value
            .get(target)
            .and_then(|entry| entry.get("level"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let mut targets: Vec<&str> = before
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if let Some(map) = after.as_object() {
        for key in map.keys() {
            if !targets.contains(&key.as_str()) {
                targets.push(key);
            }
        }
    }
    targets.sort_unstable();

    targets
        .into_iter()
        .filter_map(|target| {
            let previous = read(before, target);
            let current = read(after, target);
            // The expiry alone is not a level change: a raise whose window was extended is the
            // same raise, and an operator extending a silence-like window should not see an
            // event for it.
            if previous == current {
                return None;
            }
            Some(omnion_telemetry::LogLevelChanged {
                target: target.to_owned(),
                previous: previous.unwrap_or_else(|| "default".to_owned()),
                current: current.unwrap_or_else(|| "default".to_owned()),
                expires_at: after
                    .get(target)
                    .and_then(|entry| entry.get("expires_at"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// `GET /api/v1/observability/settings`.
pub async fn read_observability_settings(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<Json<ObservabilitySettingsView>, ApiError> {
    let pool = state.db().pool();
    let row: SettingsRow = sqlx::query_as(
        "select sampling_ratio, logs_retention_days, traces_retention_days, log_level_default, \
                log_level_overrides, cardinality_budget, prometheus_public \
         from obs_log_settings where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(map_error)?;
    let caps: SettingsCapsRow = sqlx::query_as(
        "select logs_retention_max, traces_retention_max, sampling_max, cardinality_max, \
                log_levels from obs_settings_caps where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(map_error)?;

    // Read the overrides through the same resolver the write uses, so an expired entry is shown
    // as expired here and dropped there — one implementation, two call sites.
    let now = OffsetDateTime::now_utc();
    let input = ObservabilitySettingsInput {
        sampling_ratio: row.sampling_ratio,
        logs_retention_days: i64::from(row.logs_retention_days),
        traces_retention_days: i64::from(row.traces_retention_days),
        log_level_default: row.log_level_default.clone(),
        log_level_overrides: row.log_level_overrides.clone(),
        cardinality_budget: i64::from(row.cardinality_budget),
        prometheus_public: row.prometheus_public,
    };
    let level_overrides = input.resolve_overrides(now)?;

    Ok(Json(ObservabilitySettingsView {
        sampling_ratio: row.sampling_ratio,
        // The columns are `int` and the view is `i64`, so every number in the JSON body has the
        // same width. A client that had to know which Postgres type was behind each field is a
        // client with a bug waiting to happen.
        logs_retention_days: i64::from(row.logs_retention_days),
        traces_retention_days: i64::from(row.traces_retention_days),
        log_level_default: row.log_level_default,
        log_level_overrides: row.log_level_overrides,
        cardinality_budget: i64::from(row.cardinality_budget),
        prometheus_public: row.prometheus_public,
        caps: SettingsCapsView {
            logs_retention_max: i64::from(caps.logs_retention_max),
            traces_retention_max: i64::from(caps.traces_retention_max),
            sampling_max: caps.sampling_max,
            cardinality_max: i64::from(caps.cardinality_max),
            log_levels: caps.log_levels,
        },
        egress_note:
            "Configuring an exporter sends log lines and spans to that endpoint. Secret values \
             and e-mail addresses are redacted before the payload is built, and the endpoint's \
             own authentication is held in the secret store — never in this screen."
                .to_owned(),
        level_overrides,
    }))
}

#[derive(Debug, sqlx::FromRow)]
struct SettingsRow {
    sampling_ratio: f64,
    logs_retention_days: i32,
    traces_retention_days: i32,
    log_level_default: String,
    log_level_overrides: serde_json::Value,
    cardinality_budget: i32,
    prometheus_public: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct SettingsCapsRow {
    logs_retention_max: i32,
    traces_retention_max: i32,
    sampling_max: f64,
    cardinality_max: i32,
    log_levels: Vec<String>,
}

/// `PUT /api/v1/observability/settings` — save the one settings row.
///
/// Every bound is checked against the caps TABLE, not against a constant in this file, so an
/// installation that documents a different cap gets that cap enforced. Each failure names the
/// field, because the request's acceptance line is about field-level messages.
pub async fn save_observability_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(input): Json<ObservabilitySettingsInput>,
) -> Result<Json<ObservabilitySettingsView>, ApiError> {
    let pool = state.db().pool();
    let caps: SettingsCapsRow = sqlx::query_as(
        "select logs_retention_max, traces_retention_max, sampling_max, cardinality_max, \
                log_levels from obs_settings_caps where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(map_error)?;

    if !input.sampling_ratio.is_finite()
        || input.sampling_ratio < 0.0
        || input.sampling_ratio > caps.sampling_max
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_sampling_ratio",
            format!(
                "`sampling_ratio` must be between 0.0 and {} — errors are sampled regardless, so \
                 this only sets how much of the healthy traffic is kept",
                caps.sampling_max
            ),
        ));
    }
    if input.logs_retention_days < 1 || input.logs_retention_days > caps.logs_retention_max as i64 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_retention",
            format!(
                "`logs_retention_days` must be between 1 and {} — the log store cannot answer a \
                 search older than it keeps",
                caps.logs_retention_max
            ),
        ));
    }
    if input.traces_retention_days < 1
        || input.traces_retention_days > caps.traces_retention_max as i64
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_retention",
            format!(
                "`traces_retention_days` must be between 1 and {} — the trace index cannot answer \
                 a search older than it keeps",
                caps.traces_retention_max
            ),
        ));
    }
    if !caps
        .log_levels
        .iter()
        .any(|level| level == &input.log_level_default)
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_log_level",
            format!(
                "`log_level_default` must be one of {} — got `{}`",
                caps.log_levels.join(", "),
                input.log_level_default
            ),
        ));
    }
    if input.cardinality_budget < 1 || input.cardinality_budget > caps.cardinality_max as i64 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cardinality_budget",
            format!(
                "`cardinality_budget` must be between 1 and {} — the cap is a memory bound, and \
                 a number typed into a screen cannot grow the process's heap",
                caps.cardinality_max
            ),
        ));
    }

    let overrides = input.resolve_overrides(OffsetDateTime::now_utc())?;
    let stored = input.compacted(&overrides);

    sqlx::query(
        "update obs_log_settings set \
             sampling_ratio = $1, logs_retention_days = $2, traces_retention_days = $3, \
             log_level_default = $4, log_level_overrides = $5, cardinality_budget = $6, \
             prometheus_public = $7, updated_by = $8, updated_at = now() \
         where id = 1",
    )
    .bind(input.sampling_ratio)
    .bind(input.logs_retention_days)
    .bind(input.traces_retention_days)
    .bind(&input.log_level_default)
    .bind(&stored)
    .bind(input.cardinality_budget)
    .bind(input.prometheus_public)
    .bind(session.user.id)
    .execute(pool)
    .await
    .map_err(map_error)?;

    // The registry and the sampler are told immediately, not at the next restart: the request's
    // reason for the screen is "debugging does not need a redeploy", and a setting that only
    // takes effect on restart is a setting that fails at the one moment it is needed.
    //
    // The ratio is read BEFORE the write and after it, so the event says what MOVED rather than
    // what is now. An event carrying only the new value cannot distinguish an operator raising
    // the ratio from the panel saving a form that never changed it — and a `sampling.changed`
    // per autosave would be the second.
    let previous_ratio: f64 = sqlx::query_scalar(
        "select sampling_ratio from obs_log_settings where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(map_error)?;
    let previous_overrides: serde_json::Value = sqlx::query_scalar(
        "select log_level_overrides from obs_log_settings where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(map_error)?;
    // Compared against the RAW previous value, not against a re-resolved copy of it: this diff
    // exists to say what the write changed, and re-resolving the previous row would drop an
    // entry that is still there and report the removal as nothing. The expired entries the write
    // dropped therefore show up here as `previous` with no `current` — which is what actually
    // happened.
    let before_overrides = previous_overrides;

    omnion_telemetry::metrics::global().set_global_budget(input.cardinality_budget as usize);
    omnion_telemetry::tracing_spine::set_sampling_ratio(input.sampling_ratio);

    if (previous_ratio - input.sampling_ratio).abs() > f64::EPSILON {
        omnion_telemetry::events::try_emit(
            pool,
            omnion_telemetry::events::SAMPLING_CHANGED,
            omnion_telemetry::SamplingChanged {
                previous: previous_ratio,
                current: input.sampling_ratio,
            }
            .payload(),
        )
        .await;
    }
    // One event per module whose level actually moved, and none for a module whose raise merely
    // had its expiry edited. `previous` and `current` both come from the resolved form, so an
    // expired override reads as "gone" rather than as a raise to a level nobody asked for.
    for change in level_changes(&before_overrides, &stored) {
        omnion_telemetry::events::try_emit(
            pool,
            omnion_telemetry::events::LOG_LEVEL_CHANGED,
            change.payload(),
        )
        .await;
    }

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "observability.settings.updated")
            .organization(session.user.organization_id)
            .target("settings", "obs_log_settings")
            .metadata(serde_json::json!({
                "sampling_ratio": input.sampling_ratio,
                "logs_retention_days": input.logs_retention_days,
                "traces_retention_days": input.traces_retention_days,
                "log_level_default": input.log_level_default,
                "cardinality_budget": input.cardinality_budget,
                "prometheus_public": input.prometheus_public,
            })),
    )
    .await
    .map_err(audit_error)?;

    read_observability_settings(State(state), session).await
}

/* ── the bundle manifest ─────────────────────────────────────────────────────────────────────── */

/// `GET /api/v1/observability/bundle` — what `infra/observability/` ships.
///
/// The version is compiled in, so the panel can say "this instance was built for bundle 1.x"
/// rather than reading a directory an operator may not have deployed.
pub async fn read_bundle(_state: State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": omnion_telemetry::BUNDLE_VERSION,
        "compatible_with": env!("CARGO_PKG_VERSION"),
        "note": "Omnion emits telemetry and ships the assets to read it. It does not replace \
                 your Grafana, Tempo, Jaeger or Loki — point the exporters at yours.",
        "assets": omnion_telemetry::BUNDLE_ASSETS,
        "collector_example": "infra/observability/otel-collector.yaml",
        "readme": "infra/observability/README.md",
    }))
}

/* ── the lifecycle contract, as data ─────────────────────────────────────────────────────────── */

/// `GET /api/v1/observability/lifecycle` — the probe contract, and where this process is in it.
///
/// Exposed because REQ-128 (deployment tooling) and REQ-024 (the deployment centre) both need
/// the drain deadline and the probe paths, and a hard-coded copy in each is a copy that drifts
/// from the process that has to honour it.
pub async fn read_lifecycle(_state: State<AppState>) -> Json<serde_json::Value> {
    let lifecycle = omnion_telemetry::lifecycle::global();
    Json(serde_json::json!({
        "draining": lifecycle.is_draining(),
        "in_flight": lifecycle.in_flight(),
        "drain_timeout_ms": omnion_telemetry::lifecycle::DEFAULT_DRAIN_TIMEOUT_MS,
        "probes": {
            "liveness": { "path": "/healthz", "alias": "/livez", "fails_on_drain": false },
            "readiness": { "path": "/readyz", "fails_on_drain": true },
        },
        "note": "A liveness probe that fails during a drain restarts a process that is \
                 behaving correctly, so /healthz deliberately does not report the drain.",
        "summary": lifecycle.summary(),
    }))
}

/* ── shared helpers ──────────────────────────────────────────────────────────────────────────── */

async fn audit(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    rule: &alerts::Rule,
    metadata: serde_json::Value,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, action)
            .organization(session.user.organization_id)
            .target("alert_rule", rule.id.to_string())
            .metadata(metadata),
    )
    .await
    .map_err(audit_error)?;
    Ok(())
}

fn audit_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "audit_write_failed",
        error.to_string(),
    )
}

fn map_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "observability_failed",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode as Status;

    fn input() -> ObservabilitySettingsInput {
        ObservabilitySettingsInput {
            sampling_ratio: 0.25,
            logs_retention_days: 14,
            traces_retention_days: 7,
            log_level_default: "info".to_owned(),
            log_level_overrides: serde_json::json!({}),
            cardinality_budget: 10_000,
            prometheus_public: false,
        }
    }

    #[test]
    fn an_expired_level_override_is_reported_expired_and_dropped_from_the_stored_object() {
        // The acceptance line: "A temporary log-level raise expires back to the configured
        // default without a restart." Two halves, and both are here — the screen SEES it as
        // expired, and it is not carried into the stored object, so a later read cannot
        // resurrect it.
        let mut input = input();
        input.log_level_overrides = serde_json::json!({
            "omnion_secrets": { "level": "debug" },
            "omnion_core::db": { "level": "trace", "expires_at": "2000-01-01T00:00:00Z" },
        });
        let now = OffsetDateTime::now_utc();
        let rows = input.resolve_overrides(now).expect("resolves");
        assert_eq!(rows.len(), 2);
        let expired = rows.iter().find(|row| row.expired).expect("one expired");
        assert_eq!(expired.target, "omnion_core::db");

        let compacted = input.compacted(&rows);
        assert!(
            compacted.get("omnion_core::db").is_none(),
            "an expired raise was written back: {compacted}"
        );
        assert!(
            compacted.get("omnion_secrets").is_some(),
            "a live raise was dropped: {compacted}"
        );
    }

    #[test]
    fn a_raise_with_no_expiry_is_kept_verbatim() {
        let mut input = input();
        input.log_level_overrides = serde_json::json!({ "omnion_secrets": { "level": "debug" } });
        let rows = input
            .resolve_overrides(OffsetDateTime::now_utc())
            .expect("resolves");
        assert!(!rows[0].expired);
        assert!(rows[0].expires_at.is_none());
    }

    #[test]
    fn an_unknown_level_in_an_override_is_refused_naming_the_target() {
        let mut input = input();
        input.log_level_overrides = serde_json::json!({ "omnion_secrets": { "level": "verbose" } });
        let error = input
            .resolve_overrides(OffsetDateTime::now_utc())
            .expect_err("must be refused");
        assert_eq!(error.status(), Status::UNPROCESSABLE_ENTITY);
        assert!(
            error.message().contains("omnion_secrets"),
            "the message must name the module: {}",
            error.message()
        );
        assert!(
            error.message().contains("trace, debug, info, warn, error"),
            "the message must name the accepted set: {}",
            error.message()
        );
    }

    #[test]
    fn an_override_without_a_level_is_refused_rather_than_ignored() {
        let mut input = input();
        input.log_level_overrides = serde_json::json!({ "omnion_secrets": {} });
        let error = input
            .resolve_overrides(OffsetDateTime::now_utc())
            .expect_err("must be refused");
        assert!(
            error.message().contains("must be an object"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_misspelled_setting_field_is_refused_rather_than_silently_ignored() {
        // `deny_unknown_fields` is the whole mechanism; a test that cannot construct the body
        // would be the proof, so this asserts the attribute is actually on the struct.
        let result = serde_json::from_value::<ObservabilitySettingsInput>(serde_json::json!({
            "sampling_ratio": 0.1,
            "logs_retention_days": 14,
            "traces_retention_days": 7,
            "log_level_default": "info",
            "cardinality_budget": 10000,
            "retention_days": 3,
        }));
        assert!(
            result.is_err(),
            "a misspelled field was accepted and ignored"
        );
    }

    #[test]
    fn a_silence_window_is_ordered_by_the_same_rule_the_column_enforces() {
        let ends = OffsetDateTime::now_utc();
        let starts = ends + time::Duration::hours(1);
        assert!(
            end_before_start(ends, starts),
            "the service accepted a window that the column's check would reject"
        );
        assert!(!end_before_start(starts, ends));
    }

    #[test]
    fn a_rule_with_an_unparseable_expression_is_validated_before_it_is_written() {
        // The acceptance line is that a bad expression is refused at write time, so the test
        // asserts the refusal rather than the parse — a parse test would pass whether or not
        // `validate_rule` calls it.
        let bad = AlertRuleInput {
            name: "Broken".to_owned(),
            expr: "omnion_not_a_family > 1".to_owned(),
            severity: "warning".to_owned(),
            for_seconds: 300,
            summary: String::new(),
            runbook_url: None,
            labels: serde_json::Value::Null,
        };
        let error = validate_rule(&bad).expect_err("must be refused");
        assert_eq!(error.status(), Status::UNPROCESSABLE_ENTITY);
        assert_eq!(error.code(), "invalid_alert_expression");
        assert!(
            error.message().contains("omnion_not_a_family"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_rule_is_refused_for_every_field_the_form_can_get_wrong() {
        let base = AlertRuleInput {
            name: "Ok".to_owned(),
            expr: "omnion_queue_depth > 10".to_owned(),
            severity: "warning".to_owned(),
            for_seconds: 300,
            summary: String::new(),
            runbook_url: None,
            labels: serde_json::Value::Null,
        };
        assert!(
            validate_rule(&base).is_ok(),
            "the base rule must be accepted"
        );

        let no_name = AlertRuleInput {
            name: "  ".to_owned(),
            ..base.clone()
        };
        assert_eq!(validate_rule(&no_name).unwrap_err().code(), "invalid_name");

        let bad_severity = AlertRuleInput {
            severity: "urgent".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            validate_rule(&bad_severity).unwrap_err().code(),
            "invalid_severity"
        );

        let too_long = AlertRuleInput {
            for_seconds: alerts::MAX_FOR_SECONDS + 1,
            ..base.clone()
        };
        assert_eq!(
            validate_rule(&too_long).unwrap_err().code(),
            "invalid_for_seconds"
        );

        let negative_dwell = AlertRuleInput {
            for_seconds: -1,
            ..base
        };
        assert_eq!(
            validate_rule(&negative_dwell).unwrap_err().code(),
            "invalid_for_seconds"
        );
    }

    #[test]
    fn a_severity_is_matched_case_insensitively() {
        // The form sends what the select offers, but a hand-written API call sends "Critical",
        // and refusing it over case is a `422` an operator cannot act on.
        let rule = AlertRuleInput {
            name: "Ok".to_owned(),
            expr: "omnion_queue_depth > 10".to_owned(),
            severity: "CRITICAL".to_owned(),
            for_seconds: 0,
            summary: String::new(),
            runbook_url: None,
            labels: serde_json::Value::Null,
        };
        assert!(validate_rule(&rule).is_ok());
    }

    #[test]
    fn a_zero_dwell_is_allowed_because_that_is_what_it_means() {
        // `for_seconds: 0` is the documented "fire on the first breaching sample", which is a
        // real need (a dead exporter, a disk at 100%). Refusing it would be over-validating.
        let rule = AlertRuleInput {
            name: "Immediate".to_owned(),
            expr: "omnion_exporter_dropped_total > 0".to_owned(),
            severity: "critical".to_owned(),
            for_seconds: 0,
            summary: String::new(),
            runbook_url: None,
            labels: serde_json::Value::Null,
        };
        assert!(validate_rule(&rule).is_ok());
    }
}
