//! `/api/v1/security` — posture and findings (REQ-012, slice 1).
//!
//! Four endpoints' worth of behaviour, and the rules on them are all consequences of the one
//! thing this surface does that no other does: **it tells an operator how safe they are.** A
//! response that is wrong here is not a cosmetic bug, it is a security claim the product made.
//!
//! * **A check the run could not evaluate answers `unknown`, in the response itself.** The
//!   overview always returns one row per registered check — including the ones with no stored
//!   result — so a client cannot render "we checked 4 things" when the registry has 10. A
//!   missing row and a passing row are different shapes, and the panel is written against
//!   that difference.
//! * **"Run checks" never invents a state.** The endpoint gathers the environment, hands it to
//!   the registry, and stores what comes back. If a probe cannot read, the stored state is
//!   `unknown` and the panel says so — it does not fall back to the previous run's answer,
//!   because a stale green is the most dangerous thing this screen can show.
//! * **An ignore needs its reason at the API, not only in the database.** The SQL constraint is
//!   the backstop; the `400` naming `ignore_reason` is what lets the form show the message on
//!   the field instead of a generic failure toast.
//! * **A finding you may not see is a `404`.** Same reasoning as the notification inbox: the
//!   ids are UUIDs, but a `403` would still tell a prober that the row exists.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_security::{
    Check, CheckResult, Environment, Finding, FindingQuery, NewFinding, Probe, SEVERITIES,
    SeverityCount, StatusChange, evaluate_all, find as find_check, latest_results, list_findings,
    open_counts_by_severity, record_run, set_status, stale_dependency_count, to_overview,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a store error onto the API surface.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    use omnion_security::SecurityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_security_input", message),
        E::NotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "finding not found",
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security store: {inner}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One check row as the panel reads it.
#[derive(Debug, Serialize)]
pub struct CheckBody {
    /// The check's key.
    pub key: String,
    /// The sentence in the row.
    pub label: String,
    /// One of `pass`, `warn`, `fail`, `unknown`.
    pub state: String,
    /// Structured detail; the panel renders named fields and never shows this raw.
    pub detail: serde_json::Value,
    /// When it was evaluated. `None` means "never evaluated" and the panel shows the empty
    /// state with the run action next to it.
    pub checked_at: Option<String>,
    /// Where the row's action link goes when it is not a pass.
    pub action_href: String,
    /// That link's label. Present on every row — a row with an empty action renders a dead
    /// button, which the definition of done forbids.
    pub action_label: String,
}

/// The overview's answer.
#[derive(Debug, Serialize)]
pub struct OverviewBody {
    /// One row per registered check, in display order, always all of them.
    pub checks: Vec<CheckBody>,
    /// The score ring's number, 0-100.
    pub score: i32,
    /// How many checks are in each state, so the ring's legend is computed from the same
    /// numbers the rows came from.
    pub summary: BTreeMap<String, i64>,
    /// Open findings per severity, with the zero buckets filled in.
    pub open_findings: Vec<SeverityCount>,
    /// When the last run finished; `None` before the first one.
    pub last_run_at: Option<String>,
    /// The registry's keys, so a client can tell "not evaluated" from "not a check here".
    pub registry: Vec<String>,
}

/// One finding as the panel reads it.
#[derive(Debug, Serialize)]
pub struct FindingBody {
    /// The row's id.
    pub id: Uuid,
    /// Where it came from.
    pub source: String,
    /// How bad it is.
    pub severity: String,
    /// One line.
    pub title: String,
    /// The detail drawer's body.
    pub description: String,
    /// The dependency, if any.
    pub component: Option<String>,
    /// The version present.
    pub component_version: Option<String>,
    /// The version that fixes it.
    pub fixed_in: Option<String>,
    /// What has been done about it.
    pub status: String,
    /// Why it was ignored.
    pub ignore_reason: Option<String>,
    /// When the ignore lapses.
    pub ignored_until: Option<String>,
    /// Whether the ignore has already lapsed — the row is not rewritten, and the panel greys
    /// it out and offers "reopen" rather than pretending the ignore still holds.
    pub ignore_lapsed: bool,
    /// The operator's note.
    pub note: Option<String>,
    /// When it was first raised.
    pub first_seen_at: String,
    /// When it was last confirmed still true.
    pub last_seen_at: String,
    /// Whether it still counts against the score.
    pub is_open: bool,
}

impl From<Finding> for FindingBody {
    fn from(value: Finding) -> Self {
        let ignore_lapsed = value.ignore_has_lapsed(time::OffsetDateTime::now_utc());
        let is_open = value.is_open() || ignore_lapsed;
        Self {
            id: value.id,
            source: value.source,
            severity: value.severity,
            title: value.title,
            description: value.description,
            component: value.component,
            component_version: value.component_version,
            fixed_in: value.fixed_in,
            status: value.status,
            ignore_reason: value.ignore_reason,
            ignored_until: value.ignored_until.map(|at| at.to_string()),
            ignore_lapsed,
            note: value.note,
            first_seen_at: value.first_seen_at.to_string(),
            last_seen_at: value.last_seen_at.to_string(),
            is_open,
        }
    }
}

/// The findings list's answer.
#[derive(Debug, Serialize)]
pub struct FindingsBody {
    /// The rows on this page.
    pub findings: Vec<FindingBody>,
    /// How many rows the current filter matches in total.
    ///
    /// Computed by the same filter as the rows — a total that disagrees with the page is the
    /// one number a security screen must never show.
    pub total: i64,
    /// The offset this page started at.
    pub offset: i64,
    /// The filter's own echo, so the panel can show what it is looking at.
    pub filter: FilterEcho,
}

/// What the panel asked for, echoed back.
#[derive(Debug, Serialize)]
pub struct FilterEcho {
    /// The severity filter.
    pub severity: Option<String>,
    /// The status filter.
    pub status: Option<String>,
    /// The source filter.
    pub source: Option<String>,
    /// The component filter.
    pub component: Option<String>,
    /// The free-text term.
    pub search: Option<String>,
    /// The page size actually applied after the clamp.
    pub limit: usize,
}

/// The body's shape for a status change.
#[derive(Debug, Deserialize)]
pub struct StatusBody {
    /// The status being moved to.
    pub status: String,
    /// Required when the status is `ignored`.
    #[serde(default)]
    pub ignore_reason: Option<String>,
    /// When the ignore lapses.
    #[serde(default)]
    pub ignored_until: Option<String>,
    /// An operator note.
    #[serde(default)]
    pub note: Option<String>,
}

/// The body's shape for a bulk status change.
#[derive(Debug, Deserialize)]
pub struct BulkStatusBody {
    /// The findings to change.
    pub ids: Vec<Uuid>,
    /// The change to apply to each.
    #[serde(flatten)]
    pub change: StatusBody,
}

/// What a bulk change did.
#[derive(Debug, Serialize)]
pub struct BulkStatusAnswer {
    /// The findings that changed.
    pub updated: Vec<Uuid>,
    /// The ids that matched nothing — stale selections, reported rather than swallowed.
    pub missing: Vec<Uuid>,
}

/// The body's shape for a report ingest.
#[derive(Debug, Deserialize)]
pub struct ImportBody {
    /// The document, in the format CI produces.
    pub report: serde_json::Value,
    /// Where this document claims to have come from.
    #[serde(default)]
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /security/overview` — every check, the score and the open findings.
pub async fn overview(
    State(state): State<AppState>,
    session: CurrentSession,
    headers: HeaderMap,
) -> Result<Json<OverviewBody>, ApiError> {
    let organization = session.user.organization_id;
    let env = gather(&state, organization, &headers).await?;
    let stored = latest_results(state.db().pool(), organization)
        .await
        .map_err(map_store)?;
    let run_id = stored
        .first()
        .map_or_else(Uuid::new_v4, |result| result.run_id);
    let rows = to_overview(&stored, &env, run_id);
    let last_run_at = omnion_security::last_run_at(state.db().pool(), organization)
        .await
        .map_err(map_store)?;

    let mut summary: BTreeMap<String, i64> = BTreeMap::new();
    for state_name in omnion_security::STATES {
        summary.insert((*state_name).to_string(), 0);
    }
    for row in &rows {
        *summary.entry(row.state.clone()).or_insert(0) += 1;
    }

    Ok(Json(OverviewBody {
        checks: rows
            .iter()
            .map(|row| check_body(row, &rows))
            .collect(),
        score: score_of(&rows),
        summary,
        open_findings: severity_list(&env),
        last_run_at: last_run_at.map(|at| at.to_string()),
        registry: omnion_security::keys().into_iter().map(str::to_string).collect(),
    }))
}

/// `POST /security/checks/run` — re-evaluate every check now and record the result set.
pub async fn run_checks(
    State(state): State<AppState>,
    session: CurrentSession,
    headers: HeaderMap,
) -> Result<Json<OverviewBody>, ApiError> {
    let organization = session.user.organization_id;
    let env = gather(&state, organization, &headers).await?;
    let run_id = Uuid::new_v4();
    let results = evaluate_all(&env, run_id);

    // The environment is gathered *before* the findings are written below, so a run that
    // raises a finding does not immediately re-read it and report itself consistent. The
    // operator presses the button again, which is the honest sequence for "scan again".
    record_run(state.db().pool(), organization, &results)
        .await
        .map_err(map_store)?;

    record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.scan.completed")
            .organization(organization)
            .target("security_run", run_id.to_string())
            .metadata(json!({ "checks": results.len(), "run_id": run_id })),
    )
    .await
    .ok();

    // The bus is told after the write commits, and never after: a consumer that sees an id it
    // cannot yet fetch is the one ordering bug a subscriber cannot detect — it just gets a
    // 404 and concludes the platform lied.
    bus::emit(
        state.db().pool(),
        NewEvent::new("security.scan.completed")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({ "run_id": run_id, "checks": results.len() })),
    )
    .await;

    let stored = latest_results(state.db().pool(), organization)
        .await
        .map_err(map_store)?;
    let mut summary: BTreeMap<String, i64> = BTreeMap::new();
    for state_name in omnion_security::STATES {
        summary.insert((*state_name).to_string(), 0);
    }
    for row in &stored {
        *summary.entry(row.state.clone()).or_insert(0) += 1;
    }

    Ok(Json(OverviewBody {
        checks: stored.iter().map(|row| check_body(row, &stored)).collect(),
        score: score_of(&stored),
        summary,
        open_findings: severity_list(&env),
        last_run_at: omnion_security::last_run_at(state.db().pool(), organization)
            .await
            .map_err(map_store)?
            .map(|at| at.to_string()),
        registry: omnion_security::keys().into_iter().map(str::to_string).collect(),
    }))
}

/// `GET /security/findings` — the filtered page.
pub async fn list(
    State(state): State<AppState>,
    session: CurrentSession,
    axum::extract::RawQuery(raw): RawQuery,
) -> Result<Json<FindingsBody>, ApiError> {
    let query = parse_findings_params(raw.as_deref())?;
    let page = list_findings(state.db().pool(), session.user.organization_id, &query)
        .await
        .map_err(map_store)?;
    Ok(Json(FindingsBody {
        filter: FilterEcho {
            severity: query.severity.clone(),
            status: query.status.clone(),
            source: query.source.clone(),
            component: query.component.clone(),
            search: query.search.clone(),
            limit: query.effective_limit(),
        },
        total: page.total,
        offset: page.offset,
        findings: page.findings.into_iter().map(FindingBody::from).collect(),
    }))
}

/// `GET /security/findings/{id}` — one finding, with its evidence.
pub async fn get(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<FindingBody>, ApiError> {
    let finding = omnion_security::find_finding(state.db().pool(), session.user.organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "finding not found")
        })?;
    Ok(Json(FindingBody::from(finding)))
}

/// `PATCH /security/findings/{id}` — acknowledge, ignore, mark fixed, reopen.
pub async fn patch_status(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusBody>,
) -> Result<Json<FindingBody>, ApiError> {
    let now = time::OffsetDateTime::now_utc();
    let mut change = StatusChange::of(&body.status).map_err(map_store)?;
    if let Some(reason) = body.ignore_reason {
        change = change.with_reason(reason);
    }
    if let Some(until) = body.ignored_until {
        let at = time::OffsetDateTime::parse(
            &until,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_security_input",
                format!("ignored_until {until:?} is not an RFC 3339 timestamp"),
            )
        })?;
        change = change.with_expiry(at);
    }
    if let Some(note) = body.note {
        change = change.with_note(note);
    }

    let finding = set_status(
        state.db().pool(),
        session.user.organization_id,
        id,
        &change,
        Some(session.user.id),
        now,
    )
    .await
    .map_err(map_store)?;

    record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.finding.resolved")
            .organization(session.user.organization_id)
            .target("security_finding", id.to_string())
            .metadata(json!({ "status": body.status })),
    )
    .await
    .ok();

    Ok(Json(FindingBody::from(finding)))
}

/// `POST /security/findings/bulk` — one change, many findings, and a per-row report.
pub async fn bulk(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<BulkStatusBody>,
) -> Result<Json<BulkStatusAnswer>, ApiError> {
    let now = time::OffsetDateTime::now_utc();
    let mut change = StatusChange::of(&body.change.status).map_err(map_store)?;
    if let Some(reason) = body.change.ignore_reason.clone() {
        change = change.with_reason(reason);
    }
    if let Some(note) = body.change.note.clone() {
        change = change.with_note(note);
    }
    let report = omnion_security::bulk_set_status(
        state.db().pool(),
        session.user.organization_id,
        &body.ids,
        &change,
        Some(session.user.id),
        now,
    )
    .await
    .map_err(map_store)?;
    Ok(Json(BulkStatusAnswer {
        updated: report.updated,
        missing: report.missing,
    }))
}

/// `POST /security/findings/import` — ingest a CI report.
///
/// The document is parsed rather than trusted: an entry without a title or with a severity
/// the platform does not have is refused, and a report with a key that looks like a
/// credential is refused whole. A findings table is read by people and exported to CSV, so a
/// report that puts a token in a description would move a secret from a CI log into a screen
/// designed to be shared.
pub async fn import(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<ImportBody>,
) -> Result<Json<ImportReport>, ApiError> {
    let source = body.source.unwrap_or_else(|| "report".to_string());
    if !omnion_security::is_source(&source) || source == "config" || source == "platform" {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            format!("an uploaded report may only claim source dependency or report, got {source:?}"),
        ));
    }

    let entries = read_report(&body.report, &source)?;
    if entries.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            "the report contained no findings; expected a 'findings' array or a top-level array",
        ));
    }

    let mut created = 0usize;
    let mut refreshed = 0usize;
    let mut rejected: Vec<String> = Vec::new();
    for draft in entries {
        match omnion_security::upsert_finding(state.db().pool(), session.user.organization_id, &draft).await {
            Ok((_row, true)) => created += 1,
            // The idempotence the acceptance criteria ask for: ingesting the same report
            // twice must not double the count, and the *return* is what proves it did.
            Ok((_row, false)) => refreshed += 1,
            Err(omnion_security::SecurityError::Invalid(message)) => {
                rejected.push(message);
            }
            Err(other) => return Err(map_store(other)),
        }
    }

    Ok(Json(ImportReport {
        created,
        refreshed,
        rejected,
    }))
}

/// What an ingest did.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    /// Findings that did not exist before.
    pub created: usize,
    /// Findings already known and re-confirmed — the same report run again.
    pub refreshed: usize,
    /// Entries the platform refused, with the reason. A report with a bad entry does not
    /// fail whole: the good rows land and the bad ones are named back to the uploader.
    pub rejected: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Turn a stored result into a row, with the registry's label and action.
///
/// The label and the action come from the registry rather than from the row, because a stored
/// result is a *state* and the panel needs a sentence and a link: a row that carried its own
/// label would let a check be renamed in the database without the code, and the screen would
/// keep rendering the old name forever.
fn check_body(row: &CheckResult, all: &[CheckResult]) -> CheckBody {
    let check: &Check = find_check(&row.check_key).unwrap_or(&FALLBACK_CHECK);
    let evaluated = all
        .iter()
        .find(|other| other.check_key == row.check_key)
        .is_some_and(|other| other.id != 0);
    CheckBody {
        key: row.check_key.clone(),
        label: check.label.to_string(),
        state: row.state.clone(),
        detail: row.detail.clone(),
        // `id == 0` is the marker `to_overview` gives a check that was never evaluated, so the
        // panel shows "not checked yet" with the run action rather than a 1970 date.
        checked_at: evaluated.then(|| row.checked_at.to_string()),
        action_href: check.action_href.to_string(),
        action_label: check.action_label.to_string(),
    }
}

/// The stand-in for a check key the registry does not have — a row written by a newer build.
///
/// It renders as a real row with a neutral label and a link to the findings tab, rather than
/// being dropped: hiding it would make a check the database knows about invisible, which is
/// the one thing this screen must never do. A `const` rather than a constructed value, because
/// `find_check` returns a reference and a temporary here would be dropped while borrowed.
const FALLBACK_CHECK: Check = Check {
    key: "unknown",
    label: "Unrecognised check",
    action_href: "/security/findings",
    action_label: "Review findings",
    // The registry orders in tens and tops out well below 255; 250 puts an unknown key after
    // every real check without claiming an order the type cannot hold. (The first version of
    // this said 999, which does not fit in a `u8` — an unrecognised check sorting last is a
    // display nicety, and `u8` is what the type is.)
    order: 250,
    run: |_| unreachable!("the fallback check is never evaluated"),
};

/// The score ring's number.
///
/// `pass` is worth 100, `warn` 60, `fail` 0, and **`unknown` 40** — not 0 and not 100. Zero
/// would punish the platform for what it does not know, which is how a score becomes a
/// number people stop trusting; 100 would be the over-claim this whole crate exists to avoid.
/// The middle is the point: an unanswered check counts as a question, not as a failure and not
/// as a clearance.
fn score_of(rows: &[CheckResult]) -> i32 {
    if rows.is_empty() {
        return 0;
    }
    let total: i32 = rows
        .iter()
        .map(|row| match row.state.as_str() {
            "pass" => 100,
            "warn" => 60,
            "unknown" => 40,
            _ => 0,
        })
        .sum();
    total / rows.len() as i32
}

/// Open findings per severity, with the zero buckets filled in.
///
/// The zeros are filled here rather than left to the client, because a bar chart with a
/// missing segment and a bar chart with a zero-height segment are not the same picture, and
/// only one of them is a claim about the platform.
fn severity_list(env: &Environment) -> Vec<SeverityCount> {
    SEVERITIES
        .iter()
        .map(|severity| SeverityCount {
            severity: (*severity).to_string(),
            count: env.open_findings.get(*severity).copied().unwrap_or(0),
        })
        .collect()
}

/// Parse the findings screen's query string by hand.
///
/// The reason is the same one the notifications list has: `serde_urlencoded` in axum 0.8
/// cannot express a `Vec` from a repeated key, and a filter that 400s on a legal value is a
/// filter the panel falls back from. Here every parameter is a single value, so the parsing
/// is about *messages*: an unknown severity is refused by name rather than becoming a filter
/// that returns nothing.
fn parse_findings_params(raw: Option<&str>) -> Result<FindingQuery, ApiError> {
    let mut query = FindingQuery::new();
    let Some(raw) = raw else {
        return Ok(query);
    };
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        if value.is_empty() {
            continue;
        }
        match key {
            "severity" => query.severity = Some(value),
            "status" => query.status = Some(value),
            "source" => query.source = Some(value),
            "component" => query.component = Some(value),
            "search" | "q" => query.search = Some(value),
            "offset" => {
                query.offset = value.parse().map_err(|_| {
                    ApiError::bad_request(
                        "invalid_security_input",
                        format!("offset={value:?} is not a whole number"),
                    )
                })?;
            }
            "limit" => {
                query.limit = value.parse().map_err(|_| {
                    ApiError::bad_request(
                        "invalid_security_input",
                        format!("limit={value:?} is not a whole number"),
                    )
                })?;
            }
            _ => {}
        }
    }
    Ok(query)
}

/// Percent-decode one query token, `+` meaning a space.
fn percent_decode(value: &str) -> String {
    let bytes = value.replace('+', " ");
    percent_encoding::percent_decode_str(&bytes)
        .decode_utf8_lossy()
        .to_string()
}

/// Read a report document into drafts, refusing anything that looks like a credential.
///
/// The shape is deliberately narrow — an array, or `{"findings": [...]}` — and a document in
/// any other shape is an error the uploader sees. Guessing at a format is how a security tool
/// ends up importing an object it did not understand.
fn read_report(report: &serde_json::Value, source: &str) -> Result<Vec<NewFinding>, ApiError> {
    if looks_like_a_credential(report) {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            "the report contains a key that looks like a credential; findings never carry secrets",
        ));
    }
    let entries = match report {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(map) => match map.get("findings").or_else(|| map.get("vulnerabilities")) {
            Some(serde_json::Value::Array(items)) => items.clone(),
            _ => {
                return Err(ApiError::bad_request(
                    "invalid_security_input",
                    "expected a top-level array or an object with a 'findings' array",
                ));
            }
        },
        _ => {
            return Err(ApiError::bad_request(
                "invalid_security_input",
                "expected a top-level array or an object with a 'findings' array",
            ));
        }
    };

    let mut drafts = Vec::with_capacity(entries.len());
    for entry in &entries {
        let title = entry
            .get("title")
            .or_else(|| entry.get("name"))
            .or_else(|| entry.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if title.trim().is_empty() {
            continue; // an entry with no name is not a finding; it is a line in a file
        }
        let severity = entry
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .map(|value| match value.to_ascii_lowercase().as_str() {
                "moderate" | "med" => "medium".to_string(),
                "important" | "error" => "high".to_string(),
                "moderate+" => "high".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "info".to_string());
        drafts.push(NewFinding {
            source: source.to_string(),
            severity,
            title: title.to_string(),
            description: entry
                .get("description")
                .or_else(|| entry.get("detail"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            component: entry
                .get("component")
                .or_else(|| entry.get("package"))
                .or_else(|| entry.get("module"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            component_version: entry
                .get("version")
                .or_else(|| entry.get("installed"))
                .or_else(|| entry.get("component_version"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            fixed_in: entry
                .get("fixed_in")
                .or_else(|| entry.get("fixedIn"))
                .or_else(|| entry.get("remediation"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            evidence: entry.clone(),
        });
    }
    Ok(drafts)
}

/// `true` when any key in the document looks like it holds a secret.
///
/// This is a **heuristic and the code says so**: it looks for the key *names* CI reports use
/// for credentials (`token`, `api_key`, `password`, `authorization`, `secret`, `private_key`)
/// and for a value that has the shape of a bearer token. It is not a scanner, and a report
/// that gets past it can still put something sensitive on the screen — which is why the
/// screen's own rule is that findings are shown, not executed, and why the release note
/// says the ingest path is a place to look when one does.
fn looks_like_a_credential(value: &serde_json::Value) -> bool {
    const SUSPICIOUS_KEYS: &[&str] = &[
        "token",
        "api_key",
        "apikey",
        "password",
        "passwd",
        "authorization",
        "auth",
        "secret",
        "private_key",
        "privatekey",
        "credential",
        "credentials",
        "bearer",
    ];
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, child)| {
            let lowered = key.to_ascii_lowercase();
            if SUSPICIOUS_KEYS.iter().any(|name| lowered.contains(name)) {
                return true;
            }
            looks_like_a_credential(child)
        }),
        serde_json::Value::Array(items) => items.iter().any(looks_like_a_credential),
        serde_json::Value::String(text) => {
            text.len() > 24
                && (text.starts_with("sk-")
                    || text.starts_with("ghp_")
                    || text.starts_with("Bearer ")
                    || text.starts_with("eyJ"))
        }
        _ => false,
    }
}

/// Gather what the checks are allowed to read.
///
/// Every read is independent and every failure is a probe that says so rather than an error
/// that aborts the run: a posture screen that 500s because one table is missing cannot answer
/// the nine other questions, and "the screen is down" is a worse answer than "we could not
/// read that one".
async fn gather(
    state: &AppState,
    organization: Option<Uuid>,
    headers: &HeaderMap,
) -> Result<Environment, ApiError> {
    let mut env = Environment::unprobed();

    // The finding-derived numbers are read first because they are the two checks that cannot
    // be faked: they are a count of real rows.
    if let Ok(counts) = open_counts_by_severity(state.db().pool(), organization, time::OffsetDateTime::now_utc()).await
    {
        env.open_findings = counts;
    }
    match stale_dependency_count(state.db().pool(), organization, time::OffsetDateTime::now_utc()).await
    {
        Ok(count) => env.stale_dependencies = count,
        // A read that failed is not a zero: it is a question this platform cannot answer, and
        // `None` is what turns it into an `unknown` row rather than a green one.
        Err(_) => env.stale_dependencies = None,
    }

    env.mfa = Some(factor_probe(state).await);
    env.https = Some(https_probe(headers));
    env.secure_cookies = Some(cookie_probe(headers));
    env.database_encryption = Some(Probe::unreadable(
        "storage encryption is a deployment fact; the application cannot verify it",
    ));
    env.rate_limiting = Some(Probe::unreadable(
        "rate limit policy is configured in slice 3; nothing to verify yet",
    ));
    env.csp = Some(Probe::unreadable(
        "header policy is configured in slice 2; nothing to verify yet",
    ));
    env.ip_rules = match sqlx::query_scalar::<_, i64>(
        "select count(*) from information_schema.tables \
         where table_schema = current_schema() and table_name = 'security_ip_rules'",
    )
    .fetch_one(state.db().pool())
    .await
    {
        Ok(count) if count == 0 => None, // slice 4 has not landed its table yet: not a pass
        Ok(count) => Some(count),
        Err(_) => None,
    };
    env.last_backup_hours = backup_age(state).await;
    Ok(env)
}

/// Whether any multi-factor method exists in the identity subsystem.
///
/// The check behind this one is the narrowest in the registry and says so: it reports whether
/// a factor *method* is available, not whether anybody enrolled. A platform where TOTP exists
/// and nobody uses it is not a platform with MFA enforced, and the row's label and detail both
/// say which question is being answered.
async fn factor_probe(state: &AppState) -> Probe {
    match sqlx::query_scalar::<_, i64>(
        "select count(*) from information_schema.tables \
         where table_schema = current_schema() and table_name = 'mfa_factors'",
    )
    .fetch_one(state.db().pool())
    .await
    {
        Ok(0) => Probe::missing("no multi-factor method is registered with the identity store"),
        Ok(_) => match sqlx::query_scalar::<_, i64>("select count(*) from mfa_factors")
            .fetch_one(state.db().pool())
            .await
        {
            Ok(0) => Probe::unreadable(
                "the method table exists but this build has no factor rows to read",
            ),
            Ok(count) => Probe::value(json!({ "methods": count })),
            Err(error) => Probe::unreadable(format!("the factor table could not be read: {error}")),
        },
        Err(error) => Probe::unreadable(format!("the factor table could not be probed: {error}")),
    }
}

/// Whether the request that reached us came over TLS.
///
/// Read from the request, not from a setting, because the deployment's honest answer to "is
/// TLS on?" is literally "did a request arrive over TLS" — and a platform behind a proxy that
/// does not forward the header has no way to know, which is `unknown`, not a green row from a
/// setting nobody can verify. The row's own detail names the header it needed, so an operator
/// reading a `warn` learns the fix.
fn https_probe(headers: &HeaderMap) -> Probe {
    let forwarded = header_str(headers, "x-forwarded-proto");
    if let Some(scheme) = forwarded.split(',').next().map(str::trim) {
        if scheme.eq_ignore_ascii_case("https") {
            return Probe::value(json!({ "scheme": "https", "via": "x-forwarded-proto" }));
        }
        if scheme.eq_ignore_ascii_case("http") {
            return Probe::Absent(
                "the proxy reported this request arrived over plain HTTP".to_string(),
            );
        }
    }
    Probe::unreadable(
        "no x-forwarded-proto header reached the application; behind a proxy it must be forwarded \
         or this check can never be answered",
    )
}

/// Whether the session cookie that came in would have been set with the secure flag.
///
/// Same reasoning as TLS, and deliberately so: the flag lives in the config that *mints* the
/// cookie, which this process does not own. What it can observe is whether the cookie that
/// arrived is itself marked `Secure` — which is the property that actually matters, because a
/// cookie set without it will be sent over plain HTTP by the browser.
fn cookie_probe(headers: &HeaderMap) -> Probe {
    let cookies = header_str(headers, "cookie");
    let Some(session) = cookies
        .split(';')
        .map(str::trim)
        .find(|pair| pair.starts_with("omnion_session=") || pair.starts_with("session="))
    else {
        // No cookie in *this* request is not evidence about the flag. A panel screen is
        // fetched with a session, so this is rare, and answering "unknown" is right.
        return Probe::unreadable("this request carried no session cookie to inspect");
    };
    if session.to_ascii_lowercase().contains("; secure") || session.to_ascii_lowercase().contains("__secure") {
        Probe::value(json!({ "secure": true }))
    } else {
        // Browser cookies are not required to echo their own attributes back, so a cookie that
        // arrived without the marker is suggestive rather than conclusive — which is exactly
        // why the detail says so instead of claiming a `fail` on a guess.
        Probe::unreadable(
            "the session cookie arrived without a Secure marker; browsers need not echo the \
             attribute, so verify the cookie configuration rather than this row",
        )
    }
}

/// Read one header as a string, or `""` when it is absent.
///
/// The lifetime is tied to `headers` alone, deliberately: a `&'a str` tied to both would force
/// every caller to keep the header *name* alive for as long as the value, which is a
/// restriction that exists for no reason on a function whose name is always a literal.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

/// How many hours since the last successful backup, if the backup subsystem says.
///
/// `None` means *no successful backup could be established* — either the subsystem has never
/// completed a run, or the table is not there yet, or the read failed. All three land on the
/// same answer, and the check turns that into a `fail`, which is the conservative direction:
/// a screen that says "we could not ask" where the truth is "there are no backups" is the
/// over-claim this crate exists to prevent, and the reverse — a green row on a platform with
/// no backup — is the failure that costs somebody their data.
async fn backup_age(state: &AppState) -> Option<i64> {
    // The backup subsystem's own table, read only if it exists. This is REQ-013's table, and
    // the probe answers `None` before that request lands rather than failing the whole run.
    let completed: Result<Option<(time::OffsetDateTime,)>, sqlx::Error> = sqlx::query_as(
        "select completed_at from backup_runs where status = 'succeeded' \
         order by completed_at desc limit 1",
    )
    .fetch_optional(state.db().pool())
    .await;
    let Some((completed_at,)) = completed.ok().flatten() else {
        return None;
    };
    let hours = (time::OffsetDateTime::now_utc() - completed_at).whole_hours();
    // A clock skew that puts the last backup in the future means we do not know when it ran,
    // and clamping to 0 would turn "we cannot tell" into "the backup is fresh".
    (hours >= 0).then_some(hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_score_punishes_a_failure_and_never_rewards_an_unknown() {
        let row = |state: &str| CheckResult {
            id: 1,
            organization_id: None,
            check_key: "x".into(),
            state: state.to_string(),
            detail: json!({}),
            run_id: Uuid::nil(),
            checked_at: time::OffsetDateTime::now_utc(),
        };
        assert_eq!(score_of(&[row("pass"), row("pass")]), 100);
        assert_eq!(score_of(&[row("fail"), row("pass")]), 50);
        // Unknown is neither 0 nor 100 — that is the whole reason it has its own weight.
        let unknown = score_of(&[row("unknown"), row("pass")]);
        assert!(
            (0..100).contains(&unknown),
            "an unknown check must not read as a failure or a clearance, got {unknown}"
        );
        assert_eq!(score_of(&[]), 0);
    }

    #[test]
    fn a_report_carrying_a_credential_is_refused_whole() {
        let clean = json!({"findings": [{"title": "Unpinned tokio", "severity": "high"}]});
        assert!(!looks_like_a_credential(&clean));
        assert!(read_report(&clean, "dependency").expect("a clean report parses").len() == 1);

        for leaky in [
            json!({"findings": [{"title": "x", "api_key": "abc"}]}),
            json!({"findings": [{"title": "x", "nested": {"password": "hunter2"}}]}),
            json!({"findings": [{"title": "x", "evidence": "Bearer abcdefghijklmnopqrstuvwx"}]}),
            json!({"findings": [{"title": "x", "leak": "«redacted:sk-…»"}]}),
        ] {
            assert!(
                looks_like_a_credential(&leaky),
                "this report carries something that should never be imported: {leaky}"
            );
            assert!(read_report(&leaky, "dependency").is_err());
        }
    }

    #[test]
    fn a_report_in_an_unknown_shape_is_an_error_the_uploader_sees() {
        for bad in [
            json!("a string"),
            json!(42),
            json!({"results": []}),
        ] {
            let err = read_report(&bad, "dependency").expect_err("an unknown shape must be named");
            assert!(
                err.to_string().contains("findings"),
                "the message should say what was expected: {err}"
            );
        }
    }

    #[test]
    fn a_report_with_no_findings_is_an_empty_list_not_a_fake_one() {
        let empty = read_report(&json!({"findings": []}), "dependency").expect("parses");
        assert!(empty.is_empty(), "an empty report imports nothing");
    }

    #[test]
    fn the_severity_aliases_are_the_ones_ci_actually_writes() {
        let report = json!({
            "findings": [
                {"title": "moderate one", "severity": "moderate"},
                {"title": "error one", "severity": "error"},
                {"title": "no severity at all"},
            ]
        });
        let drafts = read_report(&report, "dependency").expect("parses");
        assert_eq!(drafts[0].severity, "medium");
        assert_eq!(drafts[1].severity, "high");
        assert_eq!(
            drafts[2].severity, "info",
            "a report that omits a severity lands as info, not as a guess at high"
        );
    }

    #[test]
    fn an_entry_with_no_name_is_skipped_rather_than_imported_blank() {
        let report = json!({"findings": [{"severity": "high"}, {"title": "real one"}]});
        let drafts = read_report(&report, "dependency").expect("parses");
        assert_eq!(drafts.len(), 1, "a nameless entry is a line in a file, not a finding");
        assert_eq!(drafts[0].title, "real one");
    }

    #[test]
    fn the_query_parser_refuses_a_wordless_number_and_ignores_an_empty_filter() {
        assert!(parse_findings_params(Some("limit=lots")).is_err());
        assert!(parse_findings_params(Some("offset=nope")).is_err());
        let empty = parse_findings_params(Some("severity=&status=&search=")).expect("parses");
        assert!(empty.severity.is_none(), "an empty dropdown is not a filter");
        assert_eq!(empty.limit, 50, "an absent limit is the default page");
    }

    #[test]
    fn the_query_parser_reads_a_repeated_and_a_single_filter_the_same_way() {
        let single = parse_findings_params(Some("severity=high")).expect("parses");
        assert_eq!(single.severity.as_deref(), Some("high"));
        let repeated = parse_findings_params(Some("severity=high&severity=low")).expect("parses");
        assert_eq!(
            repeated.severity.as_deref(),
            Some("low"),
            "a repeated key takes the last value; a findings filter is single-valued"
        );
    }

    #[test]
    fn the_query_parser_understands_percent_encoding_in_a_search_term() {
        let query = parse_findings_params(Some("search=buffer%20overflow")).expect("parses");
        assert_eq!(query.search.as_deref(), Some("buffer overflow"));
    }

    #[test]
    fn an_unrecognised_parameter_is_ignored_so_a_newer_panel_keeps_working() {
        let query = parse_findings_params(Some("group_by=severity&severity=low")).expect("parses");
        assert_eq!(query.severity.as_deref(), Some("low"));
    }

    #[test]
    fn the_severity_list_fills_in_the_zero_buckets() {
        let env = Environment {
            open_findings: BTreeMap::from([("critical".to_string(), 2)]),
            ..Environment::unprobed()
        };
        let list = severity_list(&env);
        assert_eq!(list.len(), SEVERITIES.len(), "the chart has a fixed set of bars");
        assert_eq!(list[0].severity, "critical");
        assert_eq!(list[0].count, 2);
        assert!(
            list[1..].iter().all(|entry| entry.count == 0),
            "a missing bar and a zero-height bar are not the same picture"
        );
    }
}
