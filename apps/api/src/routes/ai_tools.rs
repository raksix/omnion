//! `/api/v1/ai/tools` — the tool registry (REQ-100, slice 1).
//!
//! This module is the read/write face of `ai_tools`. It exists as its own file rather than as
//! more handlers in `ai.rs` because the registry has a property nothing else in the AI hub has:
//! **every answer here is a mix of compiled code and operator decisions**, and the routes have to
//! keep those two apart in the response, not just in the database.
//!
//! # Why the response carries both halves
//!
//! A registry row is the code's description of a tool *plus* four numbers an operator owns. A
//! list that showed only the code half would be a read-only description that lies the moment
//! somebody gated a tool; a list that showed only the operator half would be four numbers with no
//! tool attached. [`ToolView`] therefore carries both, plus the two facts the panel cannot compute
//! for itself: [`ToolView::ungated_high_risk`] and [`ToolView::used_by_agents`]. The second one
//! exists because the spec requires the disable confirmation to *name* the agents that would
//! break, and a confirmation dialog that says "this will affect some agents" is the dialog the
//! spec asked to avoid.
//!
//! # The seeding banner
//!
//! `GET /ai/tools` answers `seeded: false` when the table is empty. The spec anticipates this
//! exactly — "the registry is seeded, so instead of an empty state a banner appears if seeding
//! has not run" — so an empty registry is a *diagnosis* with an action, not a dead end. An
//! operator who upgraded from a build without the tool tables gets told to restart the API
//! rather than shown a table with headers and no rows.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;

use omnion_ai_hub::identity::{self, GrantEffect};
use omnion_ai_hub::registry::{self, AgentRef, ToolLimits, ToolRow, ToolUsage};
use omnion_events::NewEvent;
use omnion_events::bus;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::ai_agents::OrgQuery;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// The window the registry's "Calls 30 d" column counts over, in SECONDS.
///
/// A named constant rather than a literal in each handler: the acceptance criterion says the
/// usage counts must equal the aggregation of `ai_tool_calls` "for the window", and a window that
/// differs by a handler is a number the panel and the API disagree about.
///
/// Seconds, not days, because the store takes seconds — `crates/ai-hub` has no `chrono`, and a
/// duration type in the signature would mean a second clock crate in a crate whose timestamps are
/// `time::OffsetDateTime`. The conversion happens exactly here, once.
const USAGE_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;

/// The class metadata the screens filter on: label, order, default risk.
///
/// Served from code, not from the table, because a class with no label is a filter chip that
/// reads `""` — and the *order* is the panel's grouping order (ops last, because an ops tool is
/// the one an operator reads twice before enabling).
#[derive(Debug, Clone, Serialize)]
pub struct ToolClassView {
    pub key: &'static str,
    pub label: &'static str,
    pub order: u8,
    pub default_risk: &'static str,
}

const CLASSES: &[ToolClassView] = &[
    ToolClassView { key: "content", label: "Content", order: 1, default_risk: "medium" },
    ToolClassView { key: "media", label: "Media", order: 2, default_risk: "medium" },
    ToolClassView { key: "users", label: "Users", order: 3, default_risk: "high" },
    ToolClassView { key: "sites", label: "Sites", order: 4, default_risk: "high" },
    ToolClassView { key: "themes", label: "Themes", order: 5, default_risk: "medium" },
    ToolClassView { key: "plugins", label: "Plugins", order: 6, default_risk: "high" },
    ToolClassView { key: "ops", label: "Operations", order: 7, default_risk: "high" },
];

/// One registry row, as the table renders it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolView {
    pub key: String,
    pub class: String,
    pub permission: String,
    pub risk: String,
    pub description: String,
    pub idempotent: bool,
    pub requires_approval: bool,
    pub enabled: bool,
    pub timeout_ms: i32,
    pub max_calls_per_run: i32,
    /// Set once the tool has left the compiled catalogue; the row and its grants are kept.
    pub retired_note: Option<String>,
    /// Enabled, high risk and ungated — the row the panel draws a warning stripe on.
    pub ungated_high_risk: bool,
    /// Calls in the 30-day window, straight from the `ai_tool_calls` aggregation.
    pub calls_30d: i64,
    /// `None` when the tool was never called, which is not the same as 0 %.
    pub error_rate_30d: Option<f64>,
    pub last_used: Option<time::OffsetDateTime>,
    /// The agents whose allow-list names this tool, for the disable confirmation.
    pub used_by_agents: Vec<AgentRef>,
}

impl ToolView {
    fn build(row: &ToolRow, usage: Option<&ToolUsage>, used_by: Vec<AgentRef>) -> Self {
        let usage = usage.copied().unwrap_or_default();
        Self {
            key: row.key.clone(),
            class: row.class.clone(),
            permission: row.permission.clone(),
            risk: row.risk.clone(),
            description: row.description.clone(),
            idempotent: row.idempotent,
            requires_approval: row.requires_approval,
            enabled: row.enabled,
            timeout_ms: row.timeout_ms,
            max_calls_per_run: row.max_calls_per_run,
            retired_note: row.retired_note.clone(),
            ungated_high_risk: row.is_ungated_high_risk(),
            calls_30d: usage.calls,
            error_rate_30d: usage.error_rate(),
            last_used: usage.last_used,
            used_by_agents: used_by,
        }
    }
}

/// `GET /api/v1/ai/tools` — the registry with its usage columns.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListToolsQuery {
    /// Free text over key and description.
    pub q: Option<String>,
    pub class: Option<String>,
    pub risk: Option<String>,
    /// Only tools that require approval.
    pub gated: Option<bool>,
    /// Only enabled, or only disabled, tools.
    pub enabled: Option<bool>,
}

/// The list body, with the seed banner flag the spec asks for.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsList {
    pub tools: Vec<ToolView>,
    /// False when the registry has no rows at all — i.e. the seeder has not run.
    pub seeded: bool,
}

pub async fn list_tools_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Query(list): Query<ListToolsQuery>,
) -> Result<Json<ToolsList>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let rows = registry::list_tools(state.db().pool()).await?;
    let seeded = !rows.is_empty();
    let usage =
        registry::usage_over(state.db().pool(), organization, USAGE_WINDOW_SECONDS)
            .await?;

    // The needle is lowercased once, outside the loop, and the search covers the key *and* the
    // description: an operator who remembers "the thing that publishes" is not typing
    // `content.publish`.
    let needle = list
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);

    let mut tools = Vec::new();
    for row in &rows {
        if let Some(class) = &list.class
            && class != "all"
            && row.class != *class
        {
            continue;
        }
        if let Some(risk) = &list.risk
            && risk != "all"
            && row.risk != *risk
        {
            continue;
        }
        if let Some(gated) = list.gated
            && row.requires_approval != gated
        {
            continue;
        }
        if let Some(enabled) = list.enabled
            && row.enabled != enabled
        {
            continue;
        }
        if let Some(needle) = &needle
            && !row.key.to_lowercase().contains(needle)
            && !row.description.to_lowercase().contains(needle)
        {
            continue;
        }
        // Only the tools that survive the filters pay for the agent lookup, so a filtered list
        // is not one query per row of the whole registry.
        let used_by = registry::agents_using(state.db().pool(), &row.key).await?;
        tools.push(ToolView::build(row, usage.get(&row.key), used_by));
    }

    Ok(Json(ToolsList { tools, seeded }))
}

/// `GET /api/v1/ai/tools/classes` — the class metadata the filters and grouping need.
pub async fn tool_classes_route(_current: CurrentSession) -> Result<Json<Vec<ToolClassView>>, ApiError> {
    let mut classes = CLASSES.to_vec();
    classes.sort_by_key(|class| class.order);
    Ok(Json(classes))
}

/// `GET /api/v1/ai/tools/{key}` — one tool with its schema, its limits and who uses it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDetail {
    #[serde(flatten)]
    pub summary: ToolView,
    /// The argument schema, verbatim, so the screen renders it read-only and never re-derives
    /// it from the model — a re-derived schema is a schema that can disagree with the validator.
    pub input_schema: serde_json::Value,
    /// The example payload the screen offers next to the copy button.
    pub example: Option<serde_json::Value>,
    /// Compiled, or a row whose tool has left the catalogue.
    pub compiled: bool,
    /// The last 20 calls. Arguments are never returned — the store returns sizes only, and the
    /// spec's "arguments redacted" is why: a call log that showed raw arguments would accumulate
    /// tenant content on a screen outside every retention path the rest of the platform honours.
    pub recent_calls: Vec<registry::RecentCall>,
}

pub async fn get_tool_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<Json<ToolDetail>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let Some(row) = registry::get_tool(state.db().pool(), &key).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    };
    let usage =
        registry::usage_over(state.db().pool(), organization, USAGE_WINDOW_SECONDS)
            .await?;
    let used_by = registry::agents_using(state.db().pool(), &row.key).await?;
    let recent_calls = registry::recent_calls(state.db().pool(), organization, &row.key, 20).await?;

    Ok(Json(ToolDetail {
        summary: ToolView::build(&row, usage.get(&row.key), used_by),
        input_schema: row.input_schema.clone(),
        example: row.example.clone(),
        compiled: registry::is_compiled(&row.key),
        recent_calls,
    }))
}

/// `GET /api/v1/ai/tools/{key}/usage` — calls, errors and latency for a window.
///
/// The spec's table lists this as its own endpoint, and it is separate from the list because
/// the list's window is fixed at 30 days while this one takes the window as a parameter — a
/// chart needs 7 or 90 days, and hard-coding 30 for both is the kind of compromise that makes a
/// chart's axis disagree with the number in the table above it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageQuery {
    /// Window in days, clamped to 1–365. A year of calls is a screen; a millisecond is a typo.
    pub days: Option<i64>,
}

pub async fn tool_usage_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
    Query(window): Query<UsageQuery>,
) -> Result<Json<ToolUsageView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    if registry::get_tool(state.db().pool(), &key).await?.is_none() {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    }
    // The chart's default window is the registry's own, so a link with no `days` shows the same
    // 30 days the "Calls 30 d" column counts. The query parameter is in days; the store takes
    // seconds, and the conversion is one multiplication at the call below.
    let days = window.days.unwrap_or(USAGE_WINDOW_SECONDS / 86_400).clamp(1, 365);
    let usage = registry::usage_over(state.db().pool(), organization, days * 24 * 60 * 60)
        .await?;
    let series = registry::usage_by_day(state.db().pool(), organization, &key, days).await?;
    let totals = usage.get(&key).copied().unwrap_or_default();
    Ok(Json(ToolUsageView {
        key,
        days,
        calls: totals.calls,
        errors: totals.errors,
        error_rate: totals.error_rate(),
        avg_duration_ms: totals.avg_duration_ms,
        series,
    }))
}

/// The usage body: totals plus the per-day series a chart draws.
#[derive(Debug, Clone, Serialize)]
pub struct ToolUsageView {
    pub key: String,
    pub days: i64,
    pub calls: i64,
    pub errors: i64,
    pub error_rate: Option<f64>,
    pub avg_duration_ms: Option<f64>,
    /// One entry per day in the window, including the days with no calls at all.
    pub series: Vec<registry::UsagePoint>,
}

/// `PATCH /api/v1/ai/tools/{key}` — the four operator-owned decisions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchTool {
    pub enabled: Option<bool>,
    pub timeout_ms: Option<i32>,
    pub max_calls_per_run: Option<i32>,
    pub requires_approval: Option<bool>,
}

pub async fn patch_tool_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<PatchTool>,
) -> Result<Json<ToolView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    let changes = ToolLimits {
        enabled: body.enabled,
        timeout_ms: body.timeout_ms,
        max_calls_per_run: body.max_calls_per_run,
        requires_approval: body.requires_approval,
    };
    // A PATCH that changes nothing is refused rather than accepted as a silent no-op that bumps
    // `updated_at`: a screen showing "saved" for a request that edited nothing is a screen that
    // teaches an operator to distrust its own confirmation.
    if changes.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "tool.nothing_to_change",
            "name at least one of enabled, timeout_ms, max_calls_per_run or requires_approval",
        ));
    }
    // The tool must exist *before* the patch is validated, so an unknown key answers 404 rather
    // than 400: the caller's mistake is the key, not the limits.
    let Some(before) = registry::get_tool(state.db().pool(), &key).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    };

    let Some(after) = registry::update_tool(state.db().pool(), &key, &changes).await? else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    };

    // The two events the spec names for this path, split by what actually changed. `ai.tool.disabled`
    // carries the agents it would break, because that is the payload an operator needs and a bare
    // "deployment.deploy was disabled" is not actionable.
    if after.enabled == false && before.enabled == true {
        let agents = registry::agents_using(state.db().pool(), &key).await?;
        let names: Vec<String> = agents.iter().map(|agent| agent.name.clone()).collect();
        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.tool.disabled")
                .organization(organization)
                .actor(current.user.id)
                .payload(serde_json::json!({ "tool_key": key, "agents": names })),
        )
        .await?;
    } else {
        let mut changed = serde_json::Map::new();
        for (field, (was, now)) in [
            ("enabled", (before.enabled as i64, after.enabled as i64)),
            ("requires_approval", (before.requires_approval as i64, after.requires_approval as i64)),
            ("timeout_ms", (i64::from(before.timeout_ms), i64::from(after.timeout_ms))),
            (
                "max_calls_per_run",
                (i64::from(before.max_calls_per_run), i64::from(after.max_calls_per_run)),
            ),
        ] {
            if was != now {
                changed.insert(field.to_owned(), serde_json::json!(now));
            }
        }
        if !changed.is_empty() {
            bus::emit(
                state.db().pool(),
                NewEvent::new("ai.tool.updated")
                    .organization(organization)
                    .actor(current.user.id)
                    .payload(json!({ "tool_key": key, "changed": changed })),
            )
            .await?;
        }
    }

    let usage =
        registry::usage_over(state.db().pool(), organization, USAGE_WINDOW_SECONDS)
            .await?;
    let used_by = registry::agents_using(state.db().pool(), &key).await?;
    Ok(Json(ToolView::build(&after, usage.get(&key), used_by)))
}

// -------------------------------------------------------------------------------------------
// Handlers · the grants of one tool
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/ai/tools/{key}/grants` — who has an opinion about this tool.
///
/// **The tool, not the identity, is the axis.** `PUT /ai/identities/{id}/tools` answers "what does
/// this identity think of everything", and the tool detail screen answers the other question —
/// "who decided something about *this* tool" — which is a different query, a different index
/// (`ai_tool_grants (tool_key)`) and a different answer. The tool detail screen is where an
/// operator goes to find out why `deployment.deploy` is refused, so this is what it renders.
pub async fn get_tool_grants_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
) -> Result<Json<ToolGrantView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    if registry::get_tool(state.db().pool(), &key).await?.is_none() {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    }
    let identities = identity::identities_granting(state.db().pool(), &key).await?;
    Ok(Json(grant_view(key, &identities)))
}

/// `PUT /api/v1/ai/tools/{key}/grants` — replace the per-identity grants for one tool.
///
/// The spec's table names it "replace the per-agent grants for one tool", and there is no per-agent
/// grant row: `ai_tool_grants` is keyed on `(identity_id, tool_key)`. An **agent's** opinion about a
/// tool is its own `tools` / `approvals` list, replaced through `PUT /ai/agents/{id}/tools`. This
/// route is the identity axis, scoped to one tool, because that is the shape the store has and the
/// only one that can express a deny — a deny is a row, and a row needs an identity.
///
/// The body is the same `{ identity_id: allow | deny | inherit }` map the identity screen sends,
/// so a client can round-trip one tool's row out of the matrix and back in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolGrantReplace {
    /// `identity_id → allow | deny | inherit`. An id that is absent from the map is **not**
    /// touched: the axis is "this tool", and the client is replacing what it can see, not clearing
    /// the table. Clearing happens by sending `inherit` for the id, which deletes the row.
    pub grants: BTreeMap<uuid::Uuid, GrantEffect>,
}

/// The response both grant routes answer with.
#[derive(Debug, Clone, Serialize)]
pub struct ToolGrantView {
    pub tool_key: String,
    /// Only the decided rows. A grant absent from this list is inherit, which is the same rule
    /// `identity::grants_of` uses — stated once here so a client does not invent a second one.
    pub grants: Vec<ToolGrant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolGrant {
    pub identity_id: uuid::Uuid,
    pub identity_key: String,
    pub identity_name: String,
    /// A platform-level identity is visible to every tenant and not editable by one of them.
    pub platform_level: bool,
}

impl ToolGrant {
    /// One identity's row on this tool.
    ///
    /// **By reference, not by value**, and that is not a style choice: `is_platform_level` reads
    /// `self`, so a `into_iter().map(|i| … { name: i.name, platform_level: i.is_platform_level() })`
    /// moves `name` out of `i` and then borrows it — `E0382`, twice, once per route. One
    /// constructor called from both places is also the reason the two responses cannot drift.
    fn from_identity(identity: &omnion_ai_hub::identity::AiIdentity) -> Self {
        Self {
            identity_id: identity.id,
            identity_key: identity.key.clone(),
            identity_name: identity.name.clone(),
            platform_level: identity.is_platform_level(),
        }
    }
}

/// The body both grant routes answer with, built from the rows that survived.
///
/// Shared so `GET` and `PUT` cannot answer different shapes for the same state — a client that
/// reads then writes would otherwise have to handle a shape change it did not make.
fn grant_view(tool_key: String, identities: &[omnion_ai_hub::identity::AiIdentity]) -> ToolGrantView {
    ToolGrantView {
        tool_key,
        grants: identities.iter().map(ToolGrant::from_identity).collect(),
    }
}

pub async fn put_tool_grants_route(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(scope_query): Query<OrgQuery>,
    Path(key): Path<String>,
    Json(body): Json<ToolGrantReplace>,
) -> Result<Json<ToolGrantView>, ApiError> {
    let organization = resolve_organization(&current, scope_query.organization_id)?;
    // The tool is checked before the body is walked, so a typo in the path answers 404 rather
    // than 400 and the caller's mistake is named as the key.
    if registry::get_tool(state.db().pool(), &key).await?.is_none() {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "tool.not_found",
            format!("no tool `{key}` in the registry"),
        ));
    }

    let mut changed = Vec::new();
    for (identity_id, effect) in &body.grants {
        // **Read first, then authorize.** A cross-tenant identity is a 404, not a 403: the status
        // code must not be an existence oracle, and a 403 here would tell an operator of tenant A
        // that tenant B has an identity with that id. This is the same order every other route in
        // this area uses, and getting it backwards is the leak.
        let Some(row) =
            identity::get_identity(state.db().pool(), organization, *identity_id).await?
        else {
            return Err(ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "identity.not_found",
                format!("no identity `{identity_id}` in this organization"),
            ));
        };
        if row.is_platform_level() {
            // Readable by everyone, writable by nobody outside the platform. The same rule the
            // identity routes apply, and it is a 403 with the key because the operator CAN see
            // this row — the refusal is about the right to change it, not about its existence.
            return Err(ApiError::forbidden(
                "identity.platform_level",
                format!(
                    "`{}` is a platform-level identity and cannot be changed from an organization",
                    row.key
                ),
            ));
        }
        identity::set_grant(state.db().pool(), *identity_id, &key, *effect, Some(current.user.id))
            .await?;
        if *effect != GrantEffect::Inherit {
            changed.push(json!({ "identity": row.key, "effect": effect.wire() }));
        }
    }

    // One event for the whole save, like the identity-level route: twenty toggles are one
    // decision, and the audit trail's value is in "this changed", not in counting the toggles.
    if !changed.is_empty() {
        bus::emit(
            state.db().pool(),
            NewEvent::new("ai.tool.grant_changed")
                .organization(organization)
                .actor(current.user.id)
                .payload(json!({ "tool_key": key, "grants": changed })),
        )
        .await?;
    }

    let identities = identity::identities_granting(state.db().pool(), &key).await?;
    Ok(Json(grant_view(key, &identities)))
}
