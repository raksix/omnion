//! `/api/v1/cdn/rules` — the cache-rule surface (docs/requests/REQ-011, slice 1).
//!
//! Reading rules is `cdn.read`; writing one is `cdn.manage`. Every handler is scoped to
//! the caller's organization the same way the rest of the platform is, and the site is
//! named in the body rather than in the path: the panel is per-site, and a path-scoped
//! route would let an account with two sites' rules manage one of them by guessing an id.
//!
//! Validation lives in `omnion-cdn` (`CacheRule::checked`), not here. The API layer's only
//! extra job is turning a field-level refusal into a `400` that names the field, because a
//! form that only learns "invalid" cannot put the message under the right input.

use axum::Json;
use axum::extract::{Path, State};
use omnion_audit::NewAuditEntry;
use omnion_cdn::store::{self, NewRule, RuleRow};
use omnion_cdn::{Bypass, CacheKey, CacheRule, CdnError, PathPattern};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/cdn/rules` and `PUT /api/v1/cdn/rules/{id}`.
///
/// A create without a priority is appended after the last rule rather than refused: an
/// operator adding a rule to the end is the common case, and making them count the
/// existing rules first is friction for no gain.
#[derive(Debug, Deserialize)]
pub struct RuleInput {
    /// The site the rule belongs to.
    pub site_id: Uuid,
    /// Display name, 1–64 characters, unique per site.
    pub name: String,
    /// Precedence; lower wins. `None` means "after everything".
    pub priority: Option<i32>,
    /// The glob, `*` within a segment and `**` across them.
    pub path_pattern: String,
    /// Methods the rule applies to.
    pub methods: Option<Vec<String>>,
    /// Edge TTL in seconds, 0–31536000.
    pub edge_ttl_seconds: Option<i32>,
    /// Browser TTL in seconds, 0–31536000.
    pub browser_ttl_seconds: Option<i32>,
    /// Stale-while-revalidate window.
    pub swr_seconds: Option<i32>,
    /// Which request parts form the cache key.
    pub cache_key: Option<CacheKey>,
    /// Conditions that force a bypass.
    pub bypass: Option<Bypass>,
    /// Whether the rule is live.
    pub enabled: Option<bool>,
}

impl RuleInput {
    /// Validate the input into a rule, defaulting what the form left out.
    fn into_rule(self, fallback_priority: i32) -> Result<CacheRule, CdnError> {
        // The pattern is compiled here rather than trusted from the body: `checked()` below
        // re-validates it, and compiling twice is cheaper than trusting a client string.
        let pattern = PathPattern::parse(&self.path_pattern)?;
        CacheRule {
            name: self.name,
            priority: self.priority.unwrap_or(fallback_priority),
            pattern,
            methods: self
                .methods
                .unwrap_or_else(|| vec!["GET".into(), "HEAD".into()]),
            edge_ttl_seconds: self.edge_ttl_seconds.unwrap_or(300),
            browser_ttl_seconds: self.browser_ttl_seconds.unwrap_or(60),
            swr_seconds: self.swr_seconds.unwrap_or(0),
            cache_key: self.cache_key.unwrap_or_default(),
            bypass: self.bypass.unwrap_or_default(),
            enabled: self.enabled.unwrap_or(true),
        }
        .checked()
        .map_err(CdnError::from)
    }
}

/// Body of `POST /api/v1/cdn/rules/reorder`.
#[derive(Debug, Deserialize)]
pub struct ReorderInput {
    /// The site whose rules are being renumbered.
    pub site_id: Uuid,
    /// Rule ids in the new precedence order, every rule of the site exactly once.
    pub order: Vec<Uuid>,
}

/// Body of the "duplicate a rule" and "disable" actions.
#[derive(Debug, Deserialize)]
pub struct ToggleInput {
    /// The rule to act on.
    pub site_id: Uuid,
    /// The live flag to set.
    pub enabled: bool,
}

/// A cache rule as the panel sees it.
#[derive(Debug, Serialize)]
pub struct RuleBody {
    /// Primary key.
    pub id: Uuid,
    /// Owning site.
    pub site_id: Uuid,
    /// Display name.
    pub name: String,
    /// Precedence; lower wins.
    pub priority: i32,
    /// The glob as written.
    pub path_pattern: String,
    /// Methods the rule applies to.
    pub methods: Vec<String>,
    /// Edge TTL in seconds.
    pub edge_ttl_seconds: i32,
    /// Browser TTL in seconds.
    pub browser_ttl_seconds: i32,
    /// Stale-while-revalidate window.
    pub swr_seconds: i32,
    /// Cache-key components.
    pub cache_key: CacheKey,
    /// Bypass conditions.
    pub bypass: Bypass,
    /// Whether the rule is live.
    pub enabled: bool,
    /// Author.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl RuleBody {
    /// Project a stored row.
    fn build(row: &RuleRow) -> Self {
        Self {
            id: row.id,
            site_id: row.site_id,
            name: row.name.clone(),
            priority: row.priority,
            path_pattern: row.path_pattern.clone(),
            methods: row.methods.clone(),
            edge_ttl_seconds: row.edge_ttl_seconds,
            browser_ttl_seconds: row.browser_ttl_seconds,
            swr_seconds: row.swr_seconds,
            cache_key: serde_json::from_value(row.cache_key.clone()).unwrap_or_default(),
            bypass: serde_json::from_value(row.bypass.clone()).unwrap_or_default(),
            enabled: row.enabled,
            created_by: row.created_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// Response of `GET /api/v1/cdn/rules`.
#[derive(Debug, Serialize)]
pub struct RulesResponse {
    /// The site's rules in precedence order.
    pub rules: Vec<RuleBody>,
    /// Rules that could not be compiled and therefore cannot match.
    ///
    /// Surfaced rather than hidden: a rule that silently stopped working is the failure an
    /// operator discovers days later from a stale page.
    pub unreadable: Vec<UnreadableRule>,
}

/// A rule that is stored but cannot be matched.
#[derive(Debug, Serialize)]
pub struct UnreadableRule {
    /// The rule's id, so it can be fixed or deleted.
    pub id: Uuid,
    /// What is wrong with it.
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/cdn/rules` — a site's rules, in precedence order.
pub async fn list_rules(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<SiteQuery>,
) -> Result<Json<RulesResponse>, ApiError> {
    let site_id = site_in_scope(&state, &current, query.site_id).await?;
    let rows = store::list_rules(state.db().pool(), site_id).await?;

    // A row is projected into the body either way, but one whose pattern no longer compiles
    // is *also* reported: it can never match, and an operator who cannot see that will
    // debug the wrong rule.
    let mut rules = Vec::with_capacity(rows.len());
    let mut unreadable = Vec::new();
    for row in &rows {
        if let Err(error) = row.to_rule() {
            unreadable.push(UnreadableRule {
                id: row.id,
                reason: error.to_string(),
            });
        }
        rules.push(RuleBody::build(row));
    }

    Ok(Json(RulesResponse { rules, unreadable }))
}

/// `GET /api/v1/cdn/rules/{id}` — one rule.
pub async fn get_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(rule_id): Path<Uuid>,
) -> Result<Json<RuleBody>, ApiError> {
    let row = rule_in_scope(&state, &current, rule_id).await?;
    Ok(Json(RuleBody::build(&row)))
}

/// `POST /api/v1/cdn/rules` — create a rule.
pub async fn create_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<RuleInput>,
) -> Result<(axum::http::StatusCode, Json<RuleBody>), ApiError> {
    let site_id = site_in_scope(&state, &current, input.site_id).await?;
    // A new rule lands after the last one, so creating several in a row preserves the
    // order they were created in.
    let fallback = next_priority(state.db().pool(), site_id).await?;
    let rule = input.into_rule(fallback)?;

    let row = store::create_rule(
        state.db().pool(),
        &NewRule {
            site_id,
            rule,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(duplicate_name)?;

    audit(
        &state,
        &current,
        &address,
        "cdn.rule.changed",
        json!({ "action": "created", "rule": row.id, "site": site_id, "name": row.name }),
    )
    .await?;

    Ok((axum::http::StatusCode::CREATED, Json(RuleBody::build(&row))))
}

/// `PUT /api/v1/cdn/rules/{id}` — update a rule.
pub async fn update_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(rule_id): Path<Uuid>,
    Json(input): Json<RuleInput>,
) -> Result<Json<RuleBody>, ApiError> {
    let existing = rule_in_scope(&state, &current, rule_id).await?;
    let site_id = site_in_scope(&state, &current, input.site_id).await?;
    if existing.site_id != site_id {
        return Err(ApiError::forbidden(
            "permission_denied",
            "a cache rule cannot be moved to another site; create a new rule there instead",
        ));
    }
    let rule = input.into_rule(existing.priority)?;

    let row = store::update_rule(state.db().pool(), rule_id, &rule)
        .await
        .map_err(duplicate_name)?;

    audit(
        &state,
        &current,
        &address,
        "cdn.rule.changed",
        json!({ "action": "updated", "rule": rule_id, "site": site_id, "name": row.name }),
    )
    .await?;

    Ok(Json(RuleBody::build(&row)))
}

/// `DELETE /api/v1/cdn/rules/{id}` — remove a rule.
pub async fn delete_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(rule_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    let row = rule_in_scope(&state, &current, rule_id).await?;
    store::delete_rule(state.db().pool(), rule_id)
        .await
        .map_err(ApiError::from)?;

    audit(
        &state,
        &current,
        &address,
        "cdn.rule.changed",
        json!({ "action": "deleted", "rule": rule_id, "site": row.site_id, "name": row.name }),
    )
    .await?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /api/v1/cdn/rules/{id}/toggle` — enable or disable a rule.
///
/// Separate from `PUT` because the panel's switch fires on every toggle and a full-body
/// `PUT` from a stale form would be a lost update the moment two panels are open. The body
/// carries no id on purpose: the path already names the row, so a stale client cannot
/// toggle the rule it meant to toggle last.
pub async fn toggle_rule(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(rule_id): Path<Uuid>,
    Json(input): Json<ToggleInput>,
) -> Result<Json<RuleBody>, ApiError> {
    site_in_scope(&state, &current, input.site_id).await?;
    let row = store::set_rule_enabled(state.db().pool(), rule_id, input.enabled)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "cache_rule_not_found",
                "no cache rule with that id exists",
            )
        })?;
    if row.site_id != input.site_id {
        return Err(ApiError::forbidden(
            "permission_denied",
            "that cache rule belongs to another site",
        ));
    }

    audit(
        &state,
        &current,
        &address,
        "cdn.rule.changed",
        json!({ "action": if input.enabled { "enabled" } else { "disabled" }, "rule": row.id, "site": row.site_id }),
    )
    .await?;

    Ok(Json(RuleBody::build(&row)))
}

/// `POST /api/v1/cdn/rules/reorder` — persist the priority order.
pub async fn reorder_rules(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(input): Json<ReorderInput>,
) -> Result<Json<RulesResponse>, ApiError> {
    let site_id = site_in_scope(&state, &current, input.site_id).await?;
    let moved = store::reorder_rules(state.db().pool(), site_id, &input.order)
        .await
        .map_err(|error| match error {
            CdnError::IncompleteReorder { expected, given } => ApiError::bad_request(
                "incomplete_reorder",
                format!("a reorder must name all {expected} rules of this site exactly once; {given} were named"),
            ),
            other => ApiError::from(other),
        })?;

    audit(
        &state,
        &current,
        &address,
        "cdn.rule.changed",
        json!({ "action": "reordered", "site": site_id, "count": moved }),
    )
    .await?;

    list_rules(
        State(state),
        current,
        axum::extract::Query(SiteQuery { site_id }),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `?site_id=`, the one parameter every list call takes.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// The site to read.
    pub site_id: Uuid,
}

/// The priority a new rule gets when the form sent none.
async fn next_priority(pool: &PgPool, site_id: Uuid) -> Result<i32, ApiError> {
    let highest: Option<i32> =
        sqlx::query_scalar("select max(priority) from cdn_cache_rules where site_id = $1")
            .bind(site_id)
            .fetch_one(pool)
            .await
            .map_err(CdnError::from)
            .map_err(ApiError::from)?;
    // A site with no rules yet starts at 0, and each new rule is appended after the
    // current maximum, so creating rules in sequence preserves the order they were made.
    Ok(highest.map_or(0, |highest| highest + 1))
}

/// Map a unique-constraint violation onto the field message the form shows.
fn duplicate_name(error: CdnError) -> ApiError {
    if let CdnError::Store(sqlx::Error::Database(ref inner)) = error {
        if inner.code().as_deref() == Some("23505") {
            return ApiError::bad_request(
                "duplicate_rule_name",
                "a cache rule with that name already exists for this site",
            )
            .with_details(json!({ "field": "name" }));
        }
    }
    ApiError::from(error)
}

/// Check the site belongs to the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Uuid, ApiError> {
    let organization_id = current.user.organization_id.ok_or_else(|| {
        ApiError::forbidden(
            "permission_denied",
            "a cache rule belongs to an organization; a platform account has no site to write to",
        )
    })?;
    let owner: Option<Uuid> = sqlx::query_scalar("select organization_id from sites where id = $1")
        .bind(site_id)
        .fetch_optional(state.db().pool())
        .await
        .map_err(CdnError::from)
        .map_err(ApiError::from)?;
    match owner {
        Some(owner) if owner == organization_id => Ok(site_id),
        _ => Err(ApiError::forbidden(
            "permission_denied",
            "that site belongs to another organization",
        )),
    }
}

/// Load a rule and check it is the caller's.
async fn rule_in_scope(
    state: &AppState,
    current: &CurrentSession,
    rule_id: Uuid,
) -> Result<RuleRow, ApiError> {
    let row = store::find_rule(state.db().pool(), rule_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "cache_rule_not_found",
                "no cache rule with that id exists",
            )
        })?;
    site_in_scope(state, current, row.site_id).await?;
    Ok(row)
}

/// Write the `cdn.*` audit entry a mutation owes.
///
/// The error is propagated rather than logged-and-ignored, which is the pattern the rest
/// of the API follows: a change that succeeded but left no audit row is a change an
/// operator cannot account for, and a `500` naming the audit table is a better outcome
/// than a silent gap in the log.
async fn audit(
    state: &AppState,
    current: &CurrentSession,
    address: &ClientAddress,
    action: &'static str,
    metadata: serde_json::Value,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, action)
            .target("cdn_rule", "cache-rules")
            .metadata(metadata)
            .ip_address(address.as_text()),
    )
    .await?;
    Ok(())
}

/// Turn a crate error into the API's own, keeping the code and the field detail.
impl From<CdnError> for ApiError {
    fn from(error: CdnError) -> Self {
        let code = error.code();
        let message = error.to_string();
        let api = if error.is_client_error() {
            ApiError::bad_request(code, message)
        } else {
            ApiError::from_core(omnion_core::CoreError::Unavailable {
                dependency: "cdn store".into(),
                message,
            })
        };
        match &error {
            CdnError::Rule(omnion_cdn::RuleError::Pattern(_)) | CdnError::Pattern(_) => {
                api.with_details(json!({ "field": "path_pattern" }))
            }
            CdnError::Rule(omnion_cdn::RuleError::NameEmpty)
            | CdnError::Rule(omnion_cdn::RuleError::NameTooLong)
            | CdnError::Rule(omnion_cdn::RuleError::NameNotPrintable) => {
                api.with_details(json!({ "field": "name" }))
            }
            CdnError::Rule(omnion_cdn::RuleError::NegativeTtl { field })
            | CdnError::Rule(omnion_cdn::RuleError::TtlTooLarge { field }) => {
                api.with_details(json!({
                    "field": match *field {
                        "edge TTL" => "edge_ttl_seconds",
                        "browser TTL" => "browser_ttl_seconds",
                        _ => "swr_seconds",
                    }
                }))
            }
            CdnError::Rule(omnion_cdn::RuleError::NoMethods) => {
                api.with_details(json!({ "field": "methods" }))
            }
            _ => api,
        }
    }
}
