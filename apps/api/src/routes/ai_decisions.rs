//! `/api/v1/ai/logs/decisions` — the route decision log (REQ-098, slice 3).
//!
//! Slices 1 and 2 made the routing *deliberate*; this makes it *explainable*. Three endpoints and
//! one shared filter:
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/logs/decisions` | `ai.usage.read` | The log, filtered and paged |
//! | `GET /ai/logs/decisions/{id}` | `ai.usage.read` | One decision with its full candidate walk |
//! | `GET /ai/logs/decisions.csv` | `ai.usage.read` | The same rows as CSV, matching the table |
//! | `GET /ai/routing/unresolved` | `ai.providers.read` | What cannot resolve, with the reason |
//!
//! # Why `ai.usage.read` and not `ai.providers.read`
//!
//! The decision log is a *usage* record: it is the accounting trail of what the platform asked of
//! its providers. `ai.providers.read` answers "what is connected and what can it do"; a reader
//! of that has no business reading the per-request history of an organization, and giving them
//! the same key would make the two powers indistinguishable. The cost manager (REQ-104) will
//! read the same rows, which is the point of one key rather than two spellings of it.
//!
//! # Why the export is the *same* filter
//!
//! `decisions.csv` re-reads the identical [`DecisionFilter`] the table used. A second set of
//! conditions is how a CSV ends up holding rows the table did not show — an export that
//! disagrees with the screen is worse than no export, because it is what gets pasted into a
//! ticket. The walk that proves it lives in `apps/api/tests/ai_decisions.rs`.

use axum::Json;
use axum::extract::{Path, Query, State};
use omnion_ai_hub::{
    DecisionFilter, DecisionRow, RouteDecision, check_task, last_per_task, list, read_one,
    unknown_task,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------------------------

/// The filters the log screen and its export both accept.
///
/// Every field is optional and the default is "no filter": a log that needs a filter to show
/// anything is a log nobody opens. `organization_id` and `site_id` are **scope**, not filters —
/// the endpoint overwrites them with the caller's own scope, so a query string cannot widen the
/// read. A caller that asks for another organization's id gets its own rows, not an error,
/// because the two fields mean the same thing here and the narrower one is the safe one to
/// honour.
#[derive(Debug, Default, Deserialize)]
pub struct DecisionQuery {
    /// Organization scope (overwritten by the session's organization).
    pub organization_id: Option<Uuid>,
    /// Site scope; only honoured when it belongs to the session's organization.
    pub site_id: Option<Uuid>,
    /// Restrict to one task key.
    pub task: Option<String>,
    /// Restrict to one feature key.
    pub feature: Option<String>,
    /// Restrict to one resolved model id.
    pub model_id: Option<Uuid>,
    /// Only rows where a fallback answered.
    #[serde(default)]
    pub fallback: bool,
    /// Only rows where nothing could answer.
    #[serde(default)]
    pub unresolved: bool,
    /// Start of the window, ISO-8601 inclusive.
    pub from: Option<String>,
    /// End of the window, ISO-8601 exclusive.
    pub to: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------------

/// One log row.
#[derive(Debug, Serialize)]
pub struct DecisionView {
    /// The row's id, which opens the detail.
    pub id: i64,
    /// When it was taken.
    pub created_at: OffsetDateTime,
    /// The task the request was for.
    pub task: Option<String>,
    /// The feature whose pin was considered first.
    pub feature: Option<String>,
    /// What the caller asked for.
    pub requested: Option<String>,
    /// The `provider/model` that answered.
    pub resolved_label: Option<String>,
    /// The model id that answered.
    pub resolved_model_id: Option<Uuid>,
    /// 0-based position; the panel badges anything above 0.
    pub fallback_index: i32,
    /// Whether a fallback answered.
    pub used_fallback: bool,
    /// Whether nothing could answer.
    pub unresolved: bool,
    /// Which rule produced the answer.
    pub rule: String,
    /// Capabilities the request needed.
    pub requirements: Vec<String>,
    /// The one-sentence explanation.
    pub reason: String,
    /// The agent run, when the request came from one.
    pub run_id: Option<Uuid>,
}

impl From<DecisionRow> for DecisionView {
    fn from(row: DecisionRow) -> Self {
        // The predicates are read *before* the moves: a `used_fallback()` call after
        // `row.decision.task` has been moved out is a borrow of a partially moved value, and the
        // compiler says so three lines after the mistake rather than at the move itself.
        let used_fallback = row.decision.used_fallback();
        let unresolved = row.decision.is_unresolved();
        let decision = row.decision;

        Self {
            id: decision.id,
            created_at: decision.created_at,
            task: decision.task,
            feature: decision.feature,
            requested: decision.requested,
            resolved_label: row.resolved_label,
            resolved_model_id: decision.resolved_model_id,
            fallback_index: decision.fallback_index,
            used_fallback,
            unresolved,
            rule: decision.rule,
            requirements: decision.requirements,
            reason: decision.reason,
            run_id: decision.run_id,
        }
    }
}

/// One page of the log.
#[derive(Debug, Serialize)]
pub struct DecisionListResponse {
    /// The rows, newest first.
    pub rows: Vec<DecisionView>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// The offset the page starts at.
    pub offset: i64,
    /// The page size actually applied.
    pub limit: i64,
}

/// One decision with its walk.
#[derive(Debug, Serialize)]
pub struct DecisionDetailResponse {
    /// The row, same shape the table renders.
    #[serde(flatten)]
    pub row: DecisionView,
    /// Every candidate considered, in order, with its outcome and reason.
    pub walk: serde_json::Value,
    /// The scope the request belonged to, spelled out.
    pub scope: String,
    /// The resolution order, so the detail view's legend cannot drift from the resolver.
    pub rules: Vec<String>,
}

impl DecisionDetailResponse {
    fn build(row: DecisionRow) -> Self {
        let scope = match (row.decision.site_id, row.decision.organization_id) {
            (Some(site), _) => format!("site:{site}"),
            (None, Some(organization)) => format!("org:{organization}"),
            (None, None) => "installation".to_owned(),
        };

        Self {
            // The view is built from the *whole* `DecisionRow` first (which consumes the
            // joined label), and the walk is read off the decision afterwards by reference.
            // Building the view from `row.decision` alone would drop `resolved_label` — the
            // `provider/model` the detail header shows — and the two orders differ in exactly
            // that field, which is why the first one compiles and renders a blank header.
            row: row.clone().into(),
            walk: row.decision.walk,
            scope,
            rules: omnion_ai_hub::RULES.iter().map(|rule| (*rule).to_owned()).collect(),
        }
    }
}

/// One task that cannot resolve, with the reason.
#[derive(Debug, Serialize)]
pub struct UnresolvedView {
    /// The task key.
    pub task: String,
    /// Why nothing answered.
    pub reason: String,
    /// How many times it has failed to resolve in the window.
    pub occurrences: i64,
    /// The most recent failure, when there is one.
    pub last_failed_at: Option<OffsetDateTime>,
}

/// The unresolved list, plus the decision it was derived from.
#[derive(Debug, Serialize)]
pub struct UnresolvedResponse {
    /// The scope the read was for.
    pub scope: omnion_ai_hub::Scope,
    /// One entry per task or feature that could not resolve, worst first.
    pub unresolved: Vec<UnresolvedView>,
    /// `true` when nothing is misconfigured — the banner's own predicate.
    pub ok: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/logs/decisions` — the log.
pub async fn list_decisions(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DecisionQuery>,
) -> Result<Json<DecisionListResponse>, ApiError> {
    let filter = build_filter(state.db().pool(), query, current.user.organization_id).await?;
    let page = list(state.db().pool(), &filter).await?;

    Ok(Json(DecisionListResponse {
        offset: filter.offset.max(0),
        limit: filter.limit.clamp(1, 500),
        total: page.total,
        rows: page.rows.into_iter().map(DecisionView::from).collect(),
    }))
}

/// `GET /api/v1/ai/logs/decisions/{id}` — one decision with its walk.
pub async fn get_decision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<i64>,
) -> Result<Json<DecisionDetailResponse>, ApiError> {
    let row = read_one(state.db().pool(), id)
        .await
        .map_err(|error| match error {
            omnion_ai_hub::AiHubError::DecisionNotFound => ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "decision_not_found",
                "no route decision with that id",
            ),
            other => ApiError::from(other),
        })?;

    // The tenancy check happens **after** the read, on the row's own organization — not on a
    // query parameter the caller controls. A decision belonging to another organization is a
    // 404, not a 403: answering "it exists, you may not see it" confirms the id is real, and
    // ids are sequential.
    if row.decision.organization_id != current.user.organization_id {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "decision_not_found",
            "no route decision with that id",
        ));
    }

    Ok(Json(DecisionDetailResponse::build(row)))
}

/// `GET /api/v1/ai/logs/decisions.csv` — the filtered rows as CSV.
///
/// Served as a file rather than as JSON so the panel's "Export" control is a real download. The
/// header is fixed and the escaping is RFC 4180: a reason containing a comma or a quote is
/// normal (model keys carry slashes, provider names carry spaces), and an unescaped row produces
/// a spreadsheet that silently shifts every column after the first one.
pub async fn export_decisions(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DecisionQuery>,
) -> Result<String, ApiError> {
    let filter = build_filter(state.db().pool(), query, current.user.organization_id).await?;
    let rows = omnion_ai_hub::export_rows(state.db().pool(), &filter).await?;

    let mut out = String::from(
        "id,created_at,task,feature,requested,resolved,fallback_index,rule,requirements,reason,run_id\n",
    );
    for row in rows {
        let view = DecisionView::from(row);
        let cells = [
            view.id.to_string(),
            view.created_at.to_string(),
            view.task.unwrap_or_default(),
            view.feature.unwrap_or_default(),
            view.requested.unwrap_or_default(),
            view.resolved_label.unwrap_or_else(|| "unresolved".to_owned()),
            view.fallback_index.to_string(),
            view.rule,
            view.requirements.join("|"),
            view.reason,
            view.run_id.map_or_else(String::new, |id| id.to_string()),
        ];
        let escaped: Vec<String> = cells.iter().map(|cell| csv_cell(cell)).collect();
        out.push_str(&escaped.join(","));
        out.push('\n');
    }

    Ok(out)
}

/// `GET /api/v1/ai/routing/unresolved` — what cannot resolve, with the reason.
///
/// Derived from the **decision log**, not from a second read of the maps. A task that was never
/// requested has no decision and is therefore absent from this list — which is correct: nothing
/// has failed yet, and the routing screen's own "no candidates" badge already covers the empty
/// case. What lands here is the task that was asked for and could not be answered, which is the
/// half that needs an operator.
pub async fn get_unresolved(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DecisionQuery>,
) -> Result<Json<UnresolvedResponse>, ApiError> {
    let filter = build_filter(state.db().pool(), query, current.user.organization_id).await?;

    let scope = match (filter.site_id, filter.organization_id) {
        (Some(site), _) => omnion_ai_hub::Scope::Site(site),
        (None, Some(organization)) => omnion_ai_hub::Scope::Organization(organization),
        (None, None) => omnion_ai_hub::Scope::Installation,
    };

    let mut unresolved_filter = filter.clone();
    unresolved_filter.unresolved_only = true;
    unresolved_filter.limit = 500;
    unresolved_filter.offset = 0;

    let page = list(state.db().pool(), &unresolved_filter).await?;

    // Grouped by task in Rust rather than by a `group by` query: the page is already bounded at
    // 500 and the grouping needs the task name, the newest failure and the count together,
    // which is three columns a grouped query would have to aggregate into a json blob.
    let mut grouped: std::collections::BTreeMap<String, UnresolvedView> = std::collections::BTreeMap::new();
    for row in page.rows {
        let Some(task) = row.decision.task.clone().or_else(|| row.decision.feature.clone()) else {
            continue;
        };
        let entry = grouped.entry(task.clone()).or_insert_with(|| UnresolvedView {
            task,
            reason: row.decision.reason.clone(),
            occurrences: 0,
            last_failed_at: None,
        });
        entry.occurrences += 1;
        entry.last_failed_at = Some(match entry.last_failed_at {
            Some(previous) if previous > row.decision.created_at => previous,
            _ => row.decision.created_at,
        });
    }

    let unresolved: Vec<UnresolvedView> = grouped.into_values().collect();
    Ok(Json(UnresolvedResponse {
        scope,
        ok: unresolved.is_empty(),
        unresolved,
    }))
}

/// The "Last resolved" column of the routing screen, for one scope.
///
/// Read through the same store filter as the log, which is what makes the column and the log
/// agree: two endpoints with two hand-built filters is how a task shows "never resolved" while
/// the log has forty rows for it.
pub async fn last_resolved(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<DecisionQuery>,
) -> Result<Json<std::collections::BTreeMap<String, RouteDecision>>, ApiError> {
    let filter = build_filter(state.db().pool(), query, current.user.organization_id).await?;
    Ok(Json(last_per_task(state.db().pool(), &filter).await?))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Build the store filter, resolving the site against the caller's organization.
async fn build_filter(
    pool: &sqlx::PgPool,
    query: DecisionQuery,
    session_organization: Option<Uuid>,
) -> Result<DecisionFilter, ApiError> {
    // The site is resolved *before* the filter is built so the filter carries a site that is
    // known to belong to the organization. A site id passed without its organization would
    // otherwise be a scope the tenancy check cannot see.
    let site_id = match (query.site_id, session_organization) {
        (Some(site), Some(organization)) => {
            let owner = omnion_ai_hub::organization_of_site(pool, site).await?;
            if owner == Some(organization) { Some(site) } else { None }
        }
        (Some(site), None) => Some(site),
        (None, _) => None,
    };

    if let Some(task) = query.task.as_deref() {
        check_task(task).map_err(|_| unknown_task(task))?;
    }

    Ok(DecisionFilter {
        organization_id: session_organization,
        site_id,
        task: query.task,
        feature: query.feature,
        model_id: query.model_id,
        fallback_only: query.fallback,
        unresolved_only: query.unresolved,
        from: parse_time(query.from.as_deref(), "from")?,
        to: parse_time(query.to.as_deref(), "to")?,
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
    })
}

/// Parse an ISO-8601 timestamp, refusing rather than defaulting.
///
/// A filter that silently ignored an unparseable date would show the operator a *different*
/// window than they asked for and label it as the one they chose — the worst possible failure
/// for a range control, and invisible without reading the query.
fn parse_time(value: Option<&str>, field: &str) -> Result<Option<OffsetDateTime>, ApiError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(raw) => OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
            .map(Some)
            .map_err(|_| {
                ApiError::bad_request(
                    "invalid_filter",
                    format!("{field} is not an RFC 3339 timestamp: {raw}"),
                )
            }),
    }
}

/// One CSV field, quoted when it has to be.
fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
