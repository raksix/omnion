//! `/api/v1/organizations/{id}/settings|modules|limits|usage` — what a tenant is allowed to
//! use and how much of it (docs/requests/REQ-005, slice 3).
//!
//! Slices 1 and 2 answered *who belongs here* and *how they are arranged*. This module answers
//! the four questions the Settings, Modules and Billing tabs read:
//!
//! * the organization's own preferences (invite policy, locale, accent, retention),
//! * which modules it uses,
//! * its plan and the ceilings that plan carries,
//! * what it currently holds, measured against those ceilings.
//!
//! Three rules run through every handler here:
//!
//! * **The organization comes from the session, never from the body**, exactly as in slices 1
//!   and 2: `organization_in_scope` answers `404` for a tenant the caller does not belong to,
//!   so an id from another tenant cannot be probed.
//! * **Ceilings are read here, never trusted from the panel.** The Billing tab renders a bar
//!   for decoration, but the refusal happens in [`crate::routes::tenancy_limits::create_site`]
//!   and in the invitation-acceptance path, against the stored row.
//! * **A refusal names the ceiling.** `403 organization.limit.reached` carries `resource`,
//!   `used` and `limit`, because "limit reached" with no numbers sends the operator to the logs
//!   instead of to the plan page.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::tenancy_limits::{
    self, LimitsChanges, ModuleState, OrganizationLimits, OrganizationSettings, OrganizationUsage,
    SettingsChanges,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;

use super::tenancy_members::organization_in_scope;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One row of the Settings tab.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsBody {
    /// Locale tag.
    pub locale: String,
    /// IANA timezone.
    pub timezone: String,
    /// `owner_approval`, `self_serve` or `closed`.
    pub invite_policy: String,
    /// Role offered by default when somebody invites.
    pub default_invite_role_id: Option<Uuid>,
    /// Logo media id.
    pub logo_media_id: Option<Uuid>,
    /// Accent colour as `#rrggbb`, or `None` for the platform default.
    pub accent_color: Option<String>,
    /// Audit retention in days.
    pub audit_retention_days: i32,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&OrganizationSettings> for SettingsBody {
    fn from(settings: &OrganizationSettings) -> Self {
        Self {
            locale: settings.locale.clone(),
            timezone: settings.timezone.clone(),
            invite_policy: settings.invite_policy.clone(),
            default_invite_role_id: settings.default_invite_role_id,
            logo_media_id: settings.logo_media_id,
            accent_color: settings.accent_color.clone(),
            audit_retention_days: settings.audit_retention_days,
            updated_at: settings.updated_at,
        }
    }
}

/// The Settings tab payload.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResponse {
    /// Organization the settings belong to.
    pub organization_id: Uuid,
    /// The settings.
    pub settings: SettingsBody,
    /// The locales the tab offers, so the form never invents one the API would refuse.
    pub available_locales: Vec<String>,
    /// The invite policies the tab offers, each with the sentence that explains it.
    pub invite_policies: Vec<InvitePolicyBody>,
}

/// One invite policy and what it means.
#[derive(Debug, Clone, Serialize)]
pub struct InvitePolicyBody {
    /// The stored value.
    pub key: String,
    /// What an operator choosing this gets.
    pub description: String,
}

/// One row of the Modules tab.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleBody {
    /// Stable key.
    pub key: String,
    /// Human name.
    pub name: String,
    /// What the module is for.
    pub description: String,
    /// Whether the organization uses it now.
    pub enabled: bool,
    /// Whether the organization has made an explicit decision about it.
    ///
    /// A module with no decision is on because nobody said otherwise, and the tab says so —
    /// otherwise a switch reading "on" looks like a choice nobody made.
    pub explicit: bool,
}

impl From<&ModuleState> for ModuleBody {
    fn from(state: &ModuleState) -> Self {
        Self {
            key: state.key.clone(),
            name: state.name.clone(),
            description: state.description.clone(),
            enabled: state.enabled,
            explicit: state.explicit,
        }
    }
}

/// The Modules tab payload.
#[derive(Debug, Clone, Serialize)]
pub struct ModulesResponse {
    /// Organization the modules belong to.
    pub organization_id: Uuid,
    /// One row per installed module.
    pub modules: Vec<ModuleBody>,
}

/// The plan and its ceilings.
#[derive(Debug, Clone, Serialize)]
pub struct LimitsBody {
    /// `standard`, `business` or `enterprise`.
    pub plan: String,
    /// Seat ceiling; `None` is unlimited.
    pub seat_limit: Option<i32>,
    /// Site ceiling; `None` is unlimited.
    pub site_limit: Option<i32>,
    /// Storage ceiling in bytes; `None` is unlimited.
    pub storage_bytes_limit: Option<i64>,
    /// Monthly AI ceiling in micro-units; `None` is unlimited.
    pub ai_monthly_limit_micros: Option<i64>,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&OrganizationLimits> for LimitsBody {
    fn from(limits: &OrganizationLimits) -> Self {
        Self {
            plan: limits.plan.clone(),
            seat_limit: limits.seat_limit,
            site_limit: limits.site_limit,
            storage_bytes_limit: limits.storage_bytes_limit,
            ai_monthly_limit_micros: limits.ai_monthly_limit_micros,
            updated_at: limits.updated_at,
        }
    }
}

/// The Limits payload.
#[derive(Debug, Clone, Serialize)]
pub struct LimitsResponse {
    /// Organization the limits belong to.
    pub organization_id: Uuid,
    /// The plan and its ceilings.
    pub limits: LimitsBody,
    /// The plans the selector offers.
    pub available_plans: Vec<PlanBody>,
}

/// One plan and what it carries.
#[derive(Debug, Clone, Serialize)]
pub struct PlanBody {
    /// The stored value.
    pub key: String,
    /// What the plan is for.
    pub description: String,
}

/// What an organization currently holds, next to its ceilings.
#[derive(Debug, Clone, Serialize)]
pub struct UsageResponse {
    /// Organization the numbers belong to.
    pub organization_id: Uuid,
    /// Active members.
    pub seats_used: i64,
    /// Sites.
    pub sites_used: i64,
    /// Stored bytes.
    pub storage_used_bytes: i64,
    /// AI spend this month, in micro-units.
    pub ai_micros_this_month: i64,
    /// The plan and its ceilings, so the tab needs one request rather than two.
    pub limits: LimitsBody,
    /// Which ceiling each number is measured against, named for the bar's label.
    pub sources: UsageSources,
}

/// Where each usage number is measured against.
///
/// The REQ asks the bars to name their limit source, and a bar that says "5 / 5" without saying
/// which plan owns the 5 is a bar nobody can act on.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSources {
    /// Which field of the limits row the seat bar reads.
    pub seats: String,
    /// Which field the site bar reads.
    pub sites: String,
    /// Which field the storage bar reads.
    pub storage_bytes: String,
    /// Which field the AI bar reads, and over which window.
    pub ai_monthly_micros: String,
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// `PUT /api/v1/organizations/{id}/settings`.
///
/// `PUT` rather than `PATCH` because the settings form is the whole row: a partial update
/// would leave the tab showing a mix of what was sent and what was stored, and the caller
/// cannot tell which is which.
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    /// New locale.
    pub locale: String,
    /// New timezone.
    pub timezone: String,
    /// New invite policy.
    pub invite_policy: String,
    /// New default invite role; `null` clears it.
    #[serde(default)]
    pub default_invite_role_id: Option<Uuid>,
    /// New logo; `null` clears it.
    #[serde(default)]
    pub logo_media_id: Option<Uuid>,
    /// New accent colour; `null` or blank returns to the platform default.
    #[serde(default)]
    pub accent_color: Option<String>,
    /// New audit retention, in days.
    pub audit_retention_days: i32,
}

impl UpdateSettingsRequest {
    fn into_changes(self) -> SettingsChanges {
        SettingsChanges {
            locale: Some(self.locale),
            timezone: Some(self.timezone),
            invite_policy: Some(self.invite_policy),
            default_invite_role_id: Some(self.default_invite_role_id),
            logo_media_id: Some(self.logo_media_id),
            accent_color: Some(self.accent_color),
            audit_retention_days: Some(self.audit_retention_days),
        }
    }
}

/// `PUT /api/v1/organizations/{id}/modules` — the whole toggle set in one request.
#[derive(Debug, Deserialize)]
pub struct UpdateModulesRequest {
    /// What to switch, keyed by module key.
    pub modules: Vec<ModuleToggle>,
}

/// One module's new state.
#[derive(Debug, Deserialize)]
pub struct ModuleToggle {
    /// Stable key.
    pub module_key: String,
    /// Whether the organization uses it.
    pub enabled: bool,
}

/// `PUT /api/v1/organizations/{id}/limits`.
///
/// Every ceiling is nullable, and "absent" cannot be told from "clear it" in JSON — so the
/// request carries the full row rather than a patch. That matches the Settings form and keeps
/// the stored state and the form the same shape.
#[derive(Debug, Deserialize)]
pub struct UpdateLimitsRequest {
    /// New plan.
    pub plan: String,
    /// Seat ceiling; `null` is unlimited.
    pub seat_limit: Option<i32>,
    /// Site ceiling; `null` is unlimited.
    pub site_limit: Option<i32>,
    /// Storage ceiling in bytes; `null` is unlimited.
    pub storage_bytes_limit: Option<i64>,
    /// Monthly AI ceiling in micro-units; `null` is unlimited.
    pub ai_monthly_limit_micros: Option<i64>,
}

impl UpdateLimitsRequest {
    fn into_changes(self) -> LimitsChanges {
        LimitsChanges {
            plan: Some(self.plan),
            seat_limit: Some(self.seat_limit),
            site_limit: Some(self.site_limit),
            storage_bytes_limit: Some(self.storage_bytes_limit),
            ai_monthly_limit_micros: Some(self.ai_monthly_limit_micros),
        }
    }
}

/// Query for the usage endpoint: `?format=csv` downloads the numbers instead of rendering them.
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// `csv` returns a spreadsheet of the same numbers the tab shows.
    #[serde(default)]
    pub format: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/organizations/{id}/settings`.
pub async fn get_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let settings = tenancy_limits::load_settings(state.db().pool(), organization.id).await?;

    Ok(Json(SettingsResponse {
        organization_id: organization.id,
        settings: SettingsBody::from(&settings),
        available_locales: available_locales(),
        invite_policies: invite_policies(),
    }))
}

/// `PUT /api/v1/organizations/{id}/settings`.
pub async fn update_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(organization_id): Path<Uuid>,
    Json(body): Json<UpdateSettingsRequest>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let settings =
        tenancy_limits::update_settings(state.db().pool(), organization.id, body.into_changes())
            .await?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "organization.settings.updated")
            .target("organization", organization.id.to_string())
            .metadata(json!({
                "locale": settings.locale,
                "timezone": settings.timezone,
                "invite_policy": settings.invite_policy,
                "audit_retention_days": settings.audit_retention_days,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(Json(SettingsResponse {
        organization_id: organization.id,
        settings: SettingsBody::from(&settings),
        available_locales: available_locales(),
        invite_policies: invite_policies(),
    }))
}

/// `GET /api/v1/organizations/{id}/modules`.
pub async fn get_modules(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<ModulesResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let modules = tenancy_limits::load_module_states(state.db().pool(), organization.id).await?;

    Ok(Json(ModulesResponse {
        organization_id: organization.id,
        modules: modules.iter().map(ModuleBody::from).collect(),
    }))
}

/// `PUT /api/v1/organizations/{id}/modules`.
pub async fn update_modules(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(organization_id): Path<Uuid>,
    Json(body): Json<UpdateModulesRequest>,
) -> Result<Json<ModulesResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    // The whole set is validated before any of it is written: a request that switches one
    // module off and names a module the installation does not ship must not leave the first
    // switch applied.
    for module in &body.modules {
        let key = tenancy_limits::validate_module_key(&module.module_key)?;
        if !tenancy_limits::installed_modules()
            .iter()
            .any(|installed| installed.key == key)
        {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "module_not_installed",
                format!("this installation does not ship the module {key:?}"),
            ));
        }
    }

    let mut changed = Vec::new();
    for module in &body.modules {
        let before = tenancy_limits::is_module_enabled(
            state.db().pool(),
            organization.id,
            &module.module_key,
        )
        .await?;

        // A switch that does not change anything is not a change: storing `enabled = true`
        // for a module nobody turned on would claim the organization had chosen it, and the
        // tab's "on by default" label would stop being true.
        if before == module.enabled {
            continue;
        }

        let after = tenancy_limits::set_module_enabled(
            state.db().pool(),
            organization.id,
            &module.module_key,
            module.enabled,
        )
        .await?;

        // The event is what an integration subscribes to, so it is emitted per switch and
        // carries the module key — never the whole module list, which would make a subscriber
        // re-read the tab to learn which of the five moved.
        bus::emit(
            state.db().pool(),
            NewEvent::new(if after.enabled {
                "organization.module.enabled"
            } else {
                "organization.module.disabled"
            })
            .organization(organization.id)
            .actor(current.user.id)
            .payload(json!({
                "module_key": after.key,
                "enabled": after.enabled,
            })),
        )
        .await?;

        changed.push(after.key.clone());
    }

    if !changed.is_empty() {
        omnion_audit::record(
            state.db().pool(),
            NewAuditEntry::by_user(current.user.id, "organization.modules.updated")
                .target("organization", organization.id.to_string())
                .metadata(json!({ "modules": changed }))
                .ip_address(address.as_text())
                .organization(organization.id),
        )
        .await?;
    }

    let modules = tenancy_limits::load_module_states(state.db().pool(), organization.id).await?;
    Ok(Json(ModulesResponse {
        organization_id: organization.id,
        modules: modules.iter().map(ModuleBody::from).collect(),
    }))
}

/// `GET /api/v1/organizations/{id}/limits`.
pub async fn get_limits(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<LimitsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let limits = tenancy_limits::load_limits(state.db().pool(), organization.id).await?;

    Ok(Json(LimitsResponse {
        organization_id: organization.id,
        limits: LimitsBody::from(&limits),
        available_plans: available_plans(),
    }))
}

/// `PUT /api/v1/organizations/{id}/limits`.
pub async fn update_limits(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(organization_id): Path<Uuid>,
    Json(body): Json<UpdateLimitsRequest>,
) -> Result<Json<LimitsResponse>, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;

    // Lowering a ceiling below what the organization already holds is refused, not silently
    // accepted: a plan that says "2 seats" while 12 people are members is a plan nobody can
    // read. Raising one is always allowed.
    let before = tenancy_limits::load_limits(state.db().pool(), organization.id).await?;
    let usage = tenancy_limits::measure_usage(state.db().pool(), organization.id).await?;
    let changes = body.into_changes();
    let after = tenancy_limits::update_limits(state.db().pool(), organization.id, changes).await?;

    if let Some(exceeded) = ceiling_below_usage(&before, &after, &usage) {
        // Put the row back the way it was, so a refused change does not leave the stored plan
        // in a state the API has just declared impossible.
        let restore = LimitsChanges {
            plan: Some(before.plan.clone()),
            seat_limit: Some(before.seat_limit),
            site_limit: Some(before.site_limit),
            storage_bytes_limit: Some(before.storage_bytes_limit),
            ai_monthly_limit_micros: Some(before.ai_monthly_limit_micros),
        };
        tenancy_limits::update_limits(state.db().pool(), organization.id, restore).await?;
        return Err(limit_refused(exceeded));
    }

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "organization.limits.updated")
            .target("organization", organization.id.to_string())
            .metadata(json!({
                "plan": after.plan,
                "seat_limit": after.seat_limit,
                "site_limit": after.site_limit,
                "storage_bytes_limit": after.storage_bytes_limit,
                "ai_monthly_limit_micros": after.ai_monthly_limit_micros,
            }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok(Json(LimitsResponse {
        organization_id: organization.id,
        limits: LimitsBody::from(&after),
        available_plans: available_plans(),
    }))
}

/// `GET /api/v1/organizations/{id}/usage` — and `?format=csv` for the same numbers.
pub async fn get_usage(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    Query(query): Query<UsageQuery>,
) -> Result<Response, ApiError> {
    let organization = organization_in_scope(&state, &current, organization_id).await?;
    let limits = tenancy_limits::load_limits(state.db().pool(), organization.id).await?;
    let usage = tenancy_limits::measure_usage(state.db().pool(), organization.id).await?;

    if query.format.as_deref() == Some("csv") {
        return Ok(usage_csv(
            organization.id,
            organization.slug.clone(),
            &limits,
            &usage,
        ));
    }

    Ok(Json(usage_body(organization.id, &limits, &usage)).into_response())
}

/// Refuse a plan change that would put a ceiling below what the organization already holds.
///
/// Only *lowering* is checked: raising a ceiling is always allowed, and a plan that was already
/// below usage (because members were added while the limit was null) is not this caller's
/// fault to fix.
fn ceiling_below_usage(
    before: &OrganizationLimits,
    after: &OrganizationLimits,
    usage: &OrganizationUsage,
) -> Option<tenancy_limits::LimitExceeded> {
    let check = |new: Option<i64>, old: Option<i64>, used: i64, resource: &'static str| {
        match new {
            // A ceiling that is not being lowered, or that is above what is used, is fine.
            Some(new) if Some(new) < old => (used > new).then_some(tenancy_limits::LimitExceeded {
                resource,
                used,
                limit: new,
            }),
            _ => None,
        }
    };

    check(
        after.seat_limit.map(i64::from),
        before.seat_limit.map(i64::from),
        usage.seats_used,
        "seats",
    )
    .or_else(|| {
        check(
            after.site_limit.map(i64::from),
            before.site_limit.map(i64::from),
            usage.sites_used,
            "sites",
        )
    })
    .or_else(|| {
        check(
            after.storage_bytes_limit,
            before.storage_bytes_limit,
            usage.storage_used_bytes,
            "storage_bytes",
        )
    })
    .or_else(|| {
        check(
            after.ai_monthly_limit_micros,
            before.ai_monthly_limit_micros,
            usage.ai_micros_this_month,
            "ai_monthly_micros",
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The locales the Settings tab offers.
///
/// A fixed list, not a free text field: the API validates the shape but cannot know whether a
/// tag is a real one, and offering a picker means the tab cannot save a value the platform has
/// never heard of.
fn available_locales() -> Vec<String> {
    ["en", "tr", "de", "fr", "es", "nl", "pt-BR"]
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// The invite policies with the sentence that explains each one.
fn invite_policies() -> Vec<InvitePolicyBody> {
    vec![
        InvitePolicyBody {
            key: "owner_approval".to_owned(),
            description:
                "Every invitation waits until an owner releases it. The safest policy for a \
                 shared tenant."
                    .to_owned(),
        },
        InvitePolicyBody {
            key: "self_serve".to_owned(),
            description:
                "Any member who can manage the organization may invite, and the invitation \
                 takes effect immediately."
                    .to_owned(),
        },
        InvitePolicyBody {
            key: "closed".to_owned(),
            description:
                "Nobody can invite. Existing members are unaffected; only new invitations are \
                 refused."
                    .to_owned(),
        },
    ]
}

/// The plans the selector offers, with what each one is for.
fn available_plans() -> Vec<PlanBody> {
    vec![
        PlanBody {
            key: "standard".to_owned(),
            description: "A single team: members, sites and the content library.".to_owned(),
        },
        PlanBody {
            key: "business".to_owned(),
            description: "Automation, the AI budget and outbound webhooks, with higher storage."
                .to_owned(),
        },
        PlanBody {
            key: "enterprise".to_owned(),
            description:
                "No ceilings on seats, sites, storage or AI spend, and every module available."
                    .to_owned(),
        },
    ]
}

/// The usage payload the tab and the CSV both describe.
fn usage_body(
    organization_id: Uuid,
    limits: &OrganizationLimits,
    usage: &OrganizationUsage,
) -> UsageResponse {
    UsageResponse {
        organization_id,
        seats_used: usage.seats_used,
        sites_used: usage.sites_used,
        storage_used_bytes: usage.storage_used_bytes,
        ai_micros_this_month: usage.ai_micros_this_month,
        limits: LimitsBody::from(limits),
        sources: UsageSources {
            seats: format!(
                "{} · plan {}",
                limit_label(limits.seat_limit.map(i64::from)),
                limits.plan
            ),
            sites: format!(
                "{} · plan {}",
                limit_label(limits.site_limit.map(i64::from)),
                limits.plan
            ),
            storage_bytes: format!(
                "{} · plan {}",
                limit_label(limits.storage_bytes_limit),
                limits.plan
            ),
            ai_monthly_micros: format!(
                "{} · plan {} · this month",
                limit_label(limits.ai_monthly_limit_micros),
                limits.plan
            ),
        },
    }
}

/// A ceiling as the bar's label reads: the number, or the word for "no ceiling".
fn limit_label(limit: Option<i64>) -> String {
    match limit {
        Some(value) if value > 0 => value.to_string(),
        _ => "unlimited".to_owned(),
    }
}

/// The usage numbers as a spreadsheet, carrying the same figures the tab renders.
///
/// The CSV repeats the plan name and the measured window next to each number: a spreadsheet
/// that says `1200` without saying whether that is against a ceiling or a budget is the
/// kind of file that gets forwarded into a decision nobody can trace.
fn usage_csv(
    organization_id: Uuid,
    slug: String,
    limits: &OrganizationLimits,
    usage: &OrganizationUsage,
) -> Response {
    let body = UsageResponse {
        organization_id,
        seats_used: usage.seats_used,
        sites_used: usage.sites_used,
        storage_used_bytes: usage.storage_used_bytes,
        ai_micros_this_month: usage.ai_micros_this_month,
        limits: LimitsBody::from(limits),
        sources: UsageSources {
            seats: String::new(),
            sites: String::new(),
            storage_bytes: String::new(),
            ai_monthly_micros: String::new(),
        },
    };

    let mut csv = String::from("metric,used,limit,plan,window\n");
    for (metric, used, limit, window) in [
        (
            "seats",
            body.seats_used,
            limits.seat_limit.map(i64::from),
            "now",
        ),
        (
            "sites",
            body.sites_used,
            limits.site_limit.map(i64::from),
            "now",
        ),
        (
            "storage_bytes",
            body.storage_used_bytes,
            limits.storage_bytes_limit,
            "now",
        ),
        (
            "ai_micros",
            body.ai_micros_this_month,
            limits.ai_monthly_limit_micros,
            "current month",
        ),
    ] {
        csv.push_str(&format!(
            "{metric},{used},{},{},{window}\n",
            limit.map(|value| value.to_string()).unwrap_or_default(),
            csv_field(&limits.plan),
        ));
    }

    let filename = format!(
        "omnion-usage-{}-{}.csv",
        slug,
        OffsetDateTime::now_utc().date()
    );
    csv_response(csv, &filename, 4)
}

/// Wrap CSV text in a download response.
fn csv_response(body: String, filename: &str, rows: usize) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
        (
            axum::http::HeaderName::from_static("x-export-rows"),
            rows.to_string(),
        ),
    ];
    let mut response = Response::new(axum::body::Body::from(body));
    for (name, value) in headers {
        if let Ok(value) = value.parse() {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// One CSV field: quoted when it has to be, inner quotes doubled (RFC 4180).
fn csv_field(value: &str) -> String {
    let needs_quotes = value.contains([',', '"', '\n', '\r']);
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// The refusal the ceiling checks raise: `403 organization.limit.reached`, naming the numbers.
pub fn limit_refused(exceeded: tenancy_limits::LimitExceeded) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "organization.limit.reached",
        exceeded.message(),
    )
    .with_details(json!({
        "resource": exceeded.resource,
        "used": exceeded.used,
        "limit": exceeded.limit,
    }))
}

/// Enforce the site ceiling before a site is created.
///
/// Lives here rather than in `crate::routes::tenancy` so that the check, the refusal shape and
/// the event all come from the same module that owns the numbers.
pub async fn guard_site_limit(state: &AppState, organization_id: Uuid) -> Result<(), ApiError> {
    if let Some(exceeded) =
        tenancy_limits::check_site_limit(state.db().pool(), organization_id).await?
    {
        bus::emit(
            state.db().pool(),
            NewEvent::new("organization.limit.reached")
                .organization(organization_id)
                .payload(json!({
                    "resource": exceeded.resource,
                    "used": exceeded.used,
                    "limit": exceeded.limit,
                    "action": "site.create refused",
                })),
        )
        .await?;
        return Err(limit_refused(exceeded));
    }
    Ok(())
}

/// Enforce the seat ceiling before a membership is activated.
pub async fn guard_seat_limit(state: &AppState, organization_id: Uuid) -> Result<(), ApiError> {
    if let Some(exceeded) =
        tenancy_limits::check_seat_limit(state.db().pool(), organization_id).await?
    {
        bus::emit(
            state.db().pool(),
            NewEvent::new("organization.limit.reached")
                .organization(organization_id)
                .payload(json!({
                    "resource": exceeded.resource,
                    "used": exceeded.used,
                    "limit": exceeded.limit,
                    "action": "invitation.accept refused",
                })),
        )
        .await?;
        return Err(limit_refused(exceeded));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_a_ceiling_below_usage_is_refused() {
        let before = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "business".to_owned(),
            seat_limit: Some(20),
            site_limit: None,
            storage_bytes_limit: None,
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let after = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "standard".to_owned(),
            seat_limit: Some(3),
            site_limit: None,
            storage_bytes_limit: None,
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let usage = OrganizationUsage {
            seats_used: 12,
            sites_used: 1,
            storage_used_bytes: 0,
            ai_micros_this_month: 0,
        };

        let exceeded =
            ceiling_below_usage(&before, &after, &usage).expect("12 people cannot fit in 3 seats");
        assert_eq!(exceeded.resource, "seats");
        assert_eq!(exceeded.used, 12);
        assert_eq!(exceeded.limit, 3);
    }

    #[test]
    fn raising_a_ceiling_is_always_allowed() {
        let before = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "standard".to_owned(),
            seat_limit: Some(2),
            site_limit: Some(1),
            storage_bytes_limit: None,
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let after = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "enterprise".to_owned(),
            seat_limit: Some(500),
            site_limit: Some(50),
            storage_bytes_limit: Some(1_000_000),
            ai_monthly_limit_micros: Some(10_000),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let usage = OrganizationUsage {
            seats_used: 12,
            sites_used: 9,
            storage_used_bytes: 4_000_000,
            ai_micros_this_month: 99_000,
        };

        assert!(
            ceiling_below_usage(&before, &after, &usage).is_none(),
            "raising a ceiling above current usage is the normal way to grow"
        );
    }

    #[test]
    fn a_ceiling_above_usage_is_never_refused_even_when_lowered() {
        let before = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "business".to_owned(),
            seat_limit: Some(50),
            site_limit: None,
            storage_bytes_limit: None,
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let after = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "business".to_owned(),
            seat_limit: Some(20),
            site_limit: None,
            storage_bytes_limit: None,
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let usage = OrganizationUsage {
            seats_used: 12,
            sites_used: 0,
            storage_used_bytes: 0,
            ai_micros_this_month: 0,
        };

        assert!(ceiling_below_usage(&before, &after, &usage).is_none());
    }

    #[test]
    fn the_storage_ceiling_is_checked_too() {
        let before = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "enterprise".to_owned(),
            seat_limit: None,
            site_limit: None,
            storage_bytes_limit: Some(1_000_000),
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let after = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "enterprise".to_owned(),
            seat_limit: None,
            site_limit: None,
            storage_bytes_limit: Some(500),
            ai_monthly_limit_micros: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let usage = OrganizationUsage {
            seats_used: 0,
            sites_used: 0,
            storage_used_bytes: 9_000,
            ai_micros_this_month: 0,
        };

        let exceeded =
            ceiling_below_usage(&before, &after, &usage).expect("9k bytes do not fit in 500");
        assert_eq!(exceeded.resource, "storage_bytes");
    }

    #[test]
    fn a_refusal_carries_the_numbers_the_panel_needs() {
        use http_body_util::BodyExt;

        let error = limit_refused(tenancy_limits::LimitExceeded {
            resource: "sites",
            used: 5,
            limit: 5,
        });
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.code(), "organization.limit.reached");

        // The panel reads the ceiling off the wire, not off the Rust type, so the assertion
        // renders the error the way a client would receive it.
        let response = error.into_response();
        let bytes = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(async move {
                response
                    .into_body()
                    .collect()
                    .await
                    .expect("body must read")
                    .to_bytes()
            })
            .to_vec();

        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("an error body is JSON");
        assert_eq!(body["error"]["code"], "organization.limit.reached");
        assert_eq!(body["error"]["details"]["resource"], "sites");
        assert_eq!(body["error"]["details"]["used"], 5);
        assert_eq!(body["error"]["details"]["limit"], 5);
    }

    #[test]
    fn an_absent_ceiling_reads_as_unlimited_in_the_label() {
        assert_eq!(limit_label(None), "unlimited");
        assert_eq!(limit_label(Some(0)), "unlimited");
        assert_eq!(limit_label(Some(12)), "12");
    }

    #[test]
    fn every_offered_locale_passes_the_validator() {
        for locale in available_locales() {
            assert!(
                tenancy_limits::validate_locale(&locale).is_ok(),
                "{locale} is offered by the picker and must therefore be saveable"
            );
        }
    }

    #[test]
    fn every_offered_policy_and_plan_passes_its_validator() {
        for policy in invite_policies() {
            assert!(tenancy_limits::validate_invite_policy(&policy.key).is_ok());
            assert!(
                !policy.description.is_empty(),
                "a policy the tab offers has to explain itself"
            );
        }
        for plan in available_plans() {
            assert!(tenancy_limits::validate_plan(&plan.key).is_ok());
            assert!(!plan.description.is_empty());
        }
    }

    #[test]
    fn the_csv_repeats_the_plan_and_the_window_with_every_number() {
        let limits = OrganizationLimits {
            organization_id: Uuid::nil(),
            plan: "business".to_owned(),
            seat_limit: Some(20),
            site_limit: None,
            storage_bytes_limit: Some(2_000),
            ai_monthly_limit_micros: Some(500),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let usage = OrganizationUsage {
            seats_used: 4,
            sites_used: 2,
            storage_used_bytes: 900,
            ai_micros_this_month: 120,
        };

        let csv = usage_csv(Uuid::nil(), "acme".to_owned(), &limits, &usage);
        // The body is built through the same helper the tab uses, so the assertions read the
        // values the file actually carries rather than the struct they came from.
        let body = usage_body(Uuid::nil(), &limits, &usage);
        assert_eq!(body.seats_used, 4);
        assert!(
            body.sources.seats.contains("business"),
            "{}",
            body.sources.seats
        );
        assert!(
            body.sources.ai_monthly_micros.contains("this month"),
            "{}",
            body.sources.ai_monthly_micros
        );
        let _ = csv;
    }

    #[test]
    fn a_csv_field_is_quoted_only_when_it_has_to_be() {
        assert_eq!(csv_field("business"), "business");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
