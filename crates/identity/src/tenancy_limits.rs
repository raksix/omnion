//! Organization settings, module enablement and plan ceilings (docs/requests/REQ-005, slice 3).
//!
//! Slices 1 and 2 answered *who belongs to this tenant* and *how they are arranged*. This
//! module answers the three questions that make a tenant configurable and bounded:
//!
//! * [`organization_settings`] — the organization's own preferences: invite policy, default
//!   invite role, locale, timezone, accent colour, logo, audit retention.
//! * [`organization_modules`] — which parts of the platform this organization uses.
//! * [`organization_limits`] — the plan and its ceilings, and the usage that is measured
//!   against them.
//!
//! Three decisions are worth naming, because each one is easy to get subtly wrong:
//!
//! * **A missing module row means *enabled*, not disabled.** `organization_modules` records a
//!   *decision*; a fresh tenant has made none. Reading "absent" as "off" would ship every new
//!   organization with the platform switched off, and a tenant that never touched a toggle
//!   would lose half its navigation. So [`is_module_enabled`] is `true` unless a row says
//!   otherwise, and [`load_module_states`] fills in that default for every installed module.
//! * **Usage is computed, never denormalised.** A seat count kept in a counter drifts the
//!   moment a membership is added by any path that forgot to bump it, and a Billing tab that
//!   disagrees with the Members tab is worse than no tab. [`measure_usage`] runs the
//!   aggregates at read time; the request explicitly asks for this ("drift in a seat count is
//!   worse than a cheap `count(*)`").
//! * **A limit is `null` or strictly positive.** `0` is refused by the schema and by
//!   [`validate_limit`] alike, because a ceiling of zero is indistinguishable in a UI from
//!   "unlimited" (both render as an em dash) while meaning the exact opposite.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// The invite policies the settings row may carry.
pub const INVITE_POLICIES: [&str; 3] = ["owner_approval", "self_serve", "closed"];

/// The plans a limits row may carry.
pub const PLANS: [&str; 3] = ["standard", "business", "enterprise"];

/// Shortest accepted audit retention, in days (a month of operational history).
pub const MIN_AUDIT_RETENTION_DAYS: i32 = 30;

/// Longest accepted audit retention, in days (ten years).
pub const MAX_AUDIT_RETENTION_DAYS: i32 = 3650;

/// A megabyte in bytes — the unit the storage ceiling is expressed in at the API edge.
pub const BYTES_PER_MEGABYTE: i64 = 1024 * 1024;

/// Column list for every `OrganizationSettings` query.
const SETTINGS_COLUMNS: &str = "organization_id, locale, timezone, invite_policy, \
     default_invite_role_id, logo_media_id, accent_color, audit_retention_days, updated_at";

/// Column list for every `OrganizationLimits` query.
const LIMITS_COLUMNS: &str = "organization_id, plan, seat_limit, site_limit, storage_bytes_limit, \
     ai_monthly_limit_micros, updated_at";

/// An organization's own preferences.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OrganizationSettings {
    /// Organization these settings belong to.
    pub organization_id: Uuid,
    /// Interface language tag.
    pub locale: String,
    /// IANA timezone name.
    pub timezone: String,
    /// `owner_approval`, `self_serve` or `closed`.
    pub invite_policy: String,
    /// Role offered by default when somebody invites.
    pub default_invite_role_id: Option<Uuid>,
    /// Logo shown in the switcher and the sign-in surface.
    pub logo_media_id: Option<Uuid>,
    /// `#rrggbb` accent colour, or `None` to use the platform default.
    pub accent_color: Option<String>,
    /// How long audit rows are kept, in days.
    pub audit_retention_days: i32,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// Fields [`update_settings`] may change. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsChanges {
    /// New locale.
    pub locale: Option<String>,
    /// New timezone.
    pub timezone: Option<String>,
    /// New invite policy.
    pub invite_policy: Option<String>,
    /// New default invite role. `Some(None)` clears it.
    pub default_invite_role_id: Option<Option<Uuid>>,
    /// New logo. `Some(None)` clears it.
    pub logo_media_id: Option<Option<Uuid>>,
    /// New accent colour. `Some(None)` clears it back to the platform default.
    pub accent_color: Option<Option<String>>,
    /// New audit retention, in days.
    pub audit_retention_days: Option<i32>,
}

impl SettingsChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locale.is_none()
            && self.timezone.is_none()
            && self.invite_policy.is_none()
            && self.default_invite_role_id.is_none()
            && self.logo_media_id.is_none()
            && self.accent_color.is_none()
            && self.audit_retention_days.is_none()
    }
}

/// An organization's plan and its ceilings.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OrganizationLimits {
    /// Organization these limits belong to.
    pub organization_id: Uuid,
    /// `standard`, `business` or `enterprise`.
    pub plan: String,
    /// Maximum active members; `None` is unlimited.
    pub seat_limit: Option<i32>,
    /// Maximum sites; `None` is unlimited.
    pub site_limit: Option<i32>,
    /// Maximum stored bytes; `None` is unlimited.
    pub storage_bytes_limit: Option<i64>,
    /// Maximum AI spend per month, in micro-units; `None` is unlimited.
    pub ai_monthly_limit_micros: Option<i64>,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// Fields [`update_limits`] may change. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitsChanges {
    /// New plan.
    pub plan: Option<String>,
    /// New seat ceiling. `Some(None)` means unlimited.
    pub seat_limit: Option<Option<i32>>,
    /// New site ceiling. `Some(None)` means unlimited.
    pub site_limit: Option<Option<i32>>,
    /// New storage ceiling in bytes. `Some(None)` means unlimited.
    pub storage_bytes_limit: Option<Option<i64>>,
    /// New monthly AI ceiling in micro-units. `Some(None)` means unlimited.
    pub ai_monthly_limit_micros: Option<Option<i64>>,
}

impl LimitsChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plan.is_none()
            && self.seat_limit.is_none()
            && self.site_limit.is_none()
            && self.storage_bytes_limit.is_none()
            && self.ai_monthly_limit_micros.is_none()
    }
}

/// One module as the Modules tab shows it: the platform's description joined with what *this*
/// organization decided about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleState {
    /// Stable key, e.g. `ai-hub`.
    pub key: String,
    /// Human name for the tab.
    pub name: String,
    /// What the module is for, in product language.
    pub description: String,
    /// Whether the organization currently uses it.
    pub enabled: bool,
    /// Whether an administrator has made an explicit decision about it.
    ///
    /// This is the difference between "on because nobody said otherwise" and "on because
    /// somebody turned it on", and the toggle needs it: a switch that only reads
    /// `enabled` cannot tell the operator whether they are about to switch something off.
    pub explicit: bool,
}

/// A module the platform ships, independent of any organization.
///
/// The installation owns the list; an organization may enable or disable a member of it. A
/// module that is not installed cannot be enabled at all — the row would then describe a
/// feature nobody can reach.
pub fn installed_modules() -> Vec<InstalledModule> {
    vec![
        InstalledModule {
            key: "ai-hub",
            name: "AI Hub",
            description: "AI providers, models and the agent runtime.",
        },
        InstalledModule {
            key: "automation",
            name: "Automation",
            description: "Workflows, triggers and scheduled runs.",
        },
        InstalledModule {
            key: "media",
            name: "Media library",
            description: "Uploads, folders and the asset library.",
        },
        InstalledModule {
            key: "analytics",
            name: "Analytics",
            description: "Page views, referrers and the content report.",
        },
        InstalledModule {
            key: "webhooks",
            name: "Webhooks",
            description: "Outbound event deliveries to your own endpoints.",
        },
    ]
}

/// A module the installation ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstalledModule {
    /// Stable key.
    pub key: &'static str,
    /// Human name.
    pub name: &'static str,
    /// What the module is for.
    pub description: &'static str,
}

/// Usage measured against the ceilings, computed at read time.
///
/// Every field is a `count`/`sum` at the moment of the read, never a stored counter, so a
/// membership added through any path shows up here immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct OrganizationUsage {
    /// Members with an `active` membership.
    pub seats_used: i64,
    /// Sites the organization owns.
    pub sites_used: i64,
    /// Bytes stored across every site of the organization.
    pub storage_used_bytes: i64,
    /// AI spend since the first day of the current month, in micro-units.
    pub ai_micros_this_month: i64,
}

/// Which limit stopped an action, and what the numbers were.
///
/// The REQ is explicit that the refusal has to *name the ceiling*, so this carries both the
/// current count and the limit: `403 organization.limit.reached` with `used`, `limit` and
/// `resource` is actionable, while a bare "limit reached" sends the operator to the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Which ceiling: `seats`, `sites`, `storage_bytes` or `ai_monthly_micros`.
    pub resource: &'static str,
    /// What the organization currently holds.
    pub used: i64,
    /// The ceiling it may not pass.
    pub limit: i64,
}

impl LimitExceeded {
    /// A sentence for the panel and for an event payload, naming the numbers.
    #[must_use]
    pub fn message(&self) -> String {
        match self.resource {
            "seats" => format!(
                "this organization already holds its {} allowed members ({used} in use)",
                self.limit,
                used = self.used
            ),
            "sites" => format!(
                "this organization already holds its {} allowed sites ({used} in use)",
                self.limit,
                used = self.used
            ),
            "storage_bytes" => format!(
                "this organization already stores {} bytes, its storage limit",
                self.limit
            ),
            _ => format!(
                "this organization has spent its monthly AI budget of {} micro-units",
                self.limit
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Validate a locale tag: non-blank and shaped like `en`, `en-GB` or `tr-TR`.
pub fn validate_locale(locale: &str) -> Result<String> {
    let locale = locale.trim().to_owned();
    let shaped = !locale.is_empty()
        && locale.len() <= 35
        && locale
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && locale.starts_with(|c: char| c.is_ascii_alphabetic());
    if shaped {
        return Ok(locale);
    }
    Err(IdentityError::InvalidSettings(
        "locale must look like `en`, `en-GB` or `tr-TR`".to_owned(),
    ))
}

/// Validate a timezone name: non-blank and free of the characters a zone name never has.
pub fn validate_timezone(timezone: &str) -> Result<String> {
    let timezone = timezone.trim().to_owned();
    if timezone.is_empty() || timezone.chars().count() > 64 {
        return Err(IdentityError::InvalidSettings(
            "timezone must be 1 to 64 characters".to_owned(),
        ));
    }
    if !timezone
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '+')
    {
        return Err(IdentityError::InvalidSettings(
            "timezone must be an IANA name such as `Europe/Istanbul`".to_owned(),
        ));
    }
    Ok(timezone)
}

/// Validate an invite policy against the three the REQ names.
pub fn validate_invite_policy(policy: &str) -> Result<String> {
    let policy = policy.trim().to_lowercase();
    if INVITE_POLICIES.contains(&policy.as_str()) {
        return Ok(policy);
    }
    Err(IdentityError::InvalidSettings(format!(
        "invite policy {policy:?} must be one of {}",
        INVITE_POLICIES.join(", ")
    )))
}

/// Validate an accent colour: exactly `#rrggbb`, or `None` for the platform default.
///
/// The hex digits are accepted in either case and stored lowercased, so the stored value
/// compares equal to what a `#RRGGBB` picker produces.
pub fn validate_accent_color(color: Option<&str>) -> Result<Option<String>> {
    let Some(color) = color else {
        return Ok(None);
    };
    let color = color.trim();
    if color.is_empty() {
        return Ok(None);
    }
    let digits = color.strip_prefix('#').unwrap_or(color);
    let shaped = digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit());
    if shaped {
        return Ok(Some(format!("#{digits}").to_lowercase()));
    }
    Err(IdentityError::InvalidSettings(
        "accent color must be a hex colour such as #2f6f4f".to_owned(),
    ))
}

/// Validate an audit retention window against the REQ's 30–3650 day range.
pub fn validate_audit_retention(days: i32) -> Result<i32> {
    if (MIN_AUDIT_RETENTION_DAYS..=MAX_AUDIT_RETENTION_DAYS).contains(&days) {
        return Ok(days);
    }
    Err(IdentityError::InvalidSettings(format!(
        "audit retention must be between {MIN_AUDIT_RETENTION_DAYS} and \
         {MAX_AUDIT_RETENTION_DAYS} days"
    )))
}

/// Validate a plan name.
pub fn validate_plan(plan: &str) -> Result<String> {
    let plan = plan.trim().to_lowercase();
    if PLANS.contains(&plan.as_str()) {
        return Ok(plan);
    }
    Err(IdentityError::InvalidLimits(format!(
        "plan {plan:?} must be one of {}",
        PLANS.join(", ")
    )))
}

/// Validate one ceiling: absent (unlimited) or strictly positive.
fn validate_limit<T: PartialOrd + Default + std::fmt::Display>(
    value: Option<T>,
    field: &str,
) -> Result<Option<T>> {
    match value {
        None => Ok(None),
        Some(inner) if inner > T::default() => Ok(Some(inner)),
        Some(_) => Err(IdentityError::InvalidLimits(format!(
            "{field} must be a positive number, or null for unlimited (0 is not a limit)"
        ))),
    }
}

/// Validate a module key with the same shape a site key has.
pub fn validate_module_key(key: &str) -> Result<String> {
    let key = key.trim().to_lowercase();
    let shaped = key.len() >= 2
        && key.len() <= 64
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key.ends_with(|c: char| c.is_ascii_alphanumeric())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(key);
    }
    Err(IdentityError::InvalidModule(format!(
        "module key {key:?} must be lowercase letters, digits and dashes (2-64 characters)"
    )))
}

/// Validate a whole set of ceiling changes before any of them is written.
pub fn validate_limits_changes(changes: &LimitsChanges) -> Result<()> {
    if let Some(plan) = &changes.plan {
        validate_plan(plan)?;
    }
    if let Some(seat_limit) = changes.seat_limit {
        validate_limit(seat_limit, "seat_limit")?;
    }
    if let Some(site_limit) = changes.site_limit {
        validate_limit(site_limit, "site_limit")?;
    }
    if let Some(storage) = changes.storage_bytes_limit {
        validate_limit(storage, "storage_bytes_limit")?;
    }
    if let Some(ai) = changes.ai_monthly_limit_micros {
        validate_limit(ai, "ai_monthly_limit_micros")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// Read an organization's settings, creating the row with defaults when it is missing.
///
/// The row is written by the migration's backfill, so `None` should not happen on an
/// installation that has been migrated; it *can* happen on an organization created between the
/// backfill and now, or on a database where the backfill was skipped. Upserting the defaults
/// is cheaper and safer than returning a hollow struct the caller might mistake for a real one.
pub async fn load_settings(pool: &PgPool, organization_id: Uuid) -> Result<OrganizationSettings> {
    let sql = format!(
        "insert into organization_settings (organization_id) values ($1) \
         on conflict (organization_id) do update set organization_id = excluded.organization_id \
         returning {SETTINGS_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OrganizationSettings>(&sql)
        .bind(organization_id)
        .fetch_one(pool)
        .await?)
}

/// Apply a settings change.
///
/// Every field is validated *before* the statement runs, so a request carrying one bad value
/// leaves the stored settings untouched rather than half-updated.
pub async fn update_settings(
    pool: &PgPool,
    organization_id: Uuid,
    changes: SettingsChanges,
) -> Result<OrganizationSettings> {
    let locale = changes.locale.as_deref().map(validate_locale).transpose()?;
    let timezone = changes
        .timezone
        .as_deref()
        .map(validate_timezone)
        .transpose()?;
    let invite_policy = changes
        .invite_policy
        .as_deref()
        .map(validate_invite_policy)
        .transpose()?;
    let accent_color = match &changes.accent_color {
        Some(color) => validate_accent_color(color.as_deref())?,
        None => None,
    };
    let retention = changes
        .audit_retention_days
        .map(validate_audit_retention)
        .transpose()?;

    // The defaults keep an untouched field at its stored value: this is a partial update, and
    // reading them back first is what makes `coalesce($n, stored)` unnecessary.
    let current = load_settings(pool, organization_id).await?;

    let sql = format!(
        "update organization_settings set locale = $2, timezone = $3, invite_policy = $4, \
             default_invite_role_id = $5, logo_media_id = $6, accent_color = $7, \
             audit_retention_days = $8, updated_at = now() \
         where organization_id = $1 returning {SETTINGS_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OrganizationSettings>(&sql)
        .bind(organization_id)
        .bind(locale.as_deref().unwrap_or(&current.locale))
        .bind(timezone.as_deref().unwrap_or(&current.timezone))
        .bind(invite_policy.as_deref().unwrap_or(&current.invite_policy))
        .bind(
            changes
                .default_invite_role_id
                .unwrap_or(current.default_invite_role_id),
        )
        .bind(changes.logo_media_id.unwrap_or(current.logo_media_id))
        .bind(accent_color.or(current.accent_color).as_deref())
        .bind(retention.unwrap_or(current.audit_retention_days))
        .fetch_one(pool)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------------------------

/// Is `module_key` enabled for this organization?
///
/// An absent row reads as **enabled**: `organization_modules` records decisions, and a tenant
/// that never opened the Modules tab has made none. Reading it as disabled would switch the
/// platform off for every organization created since the migration.
pub async fn is_module_enabled(
    pool: &PgPool,
    organization_id: Uuid,
    module_key: &str,
) -> Result<bool> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "select enabled from organization_modules \
          where organization_id = $1 and module_key = $2",
    )
    .bind(organization_id)
    .bind(module_key)
    .fetch_optional(pool)
    .await?;
    Ok(enabled.unwrap_or(true))
}

/// Every installed module joined with this organization's decision about it.
///
/// A module key the organization has a row for but the installation does not list is dropped:
/// it describes a feature this platform build cannot serve, and showing a switch for it would
/// be a dead control.
pub async fn load_module_states(pool: &PgPool, organization_id: Uuid) -> Result<Vec<ModuleState>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "select module_key, enabled from organization_modules where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(installed_modules()
        .into_iter()
        .map(
            |module| match rows.iter().find(|(key, _)| key == module.key) {
                Some((_, enabled)) => ModuleState {
                    key: module.key.to_owned(),
                    name: module.name.to_owned(),
                    description: module.description.to_owned(),
                    enabled: *enabled,
                    explicit: true,
                },
                None => ModuleState {
                    key: module.key.to_owned(),
                    name: module.name.to_owned(),
                    description: module.description.to_owned(),
                    enabled: true,
                    explicit: false,
                },
            },
        )
        .collect())
}

/// Record an organization's decision about a module.
///
/// The key is validated first, and then checked against the installation: enabling a module the
/// platform does not ship would store a row describing nothing.
pub async fn set_module_enabled(
    pool: &PgPool,
    organization_id: Uuid,
    module_key: &str,
    enabled: bool,
) -> Result<ModuleState> {
    let key = validate_module_key(module_key)?;
    let Some(module) = installed_modules().into_iter().find(|m| m.key == key) else {
        return Err(IdentityError::ModuleNotInstalled(key));
    };

    sqlx::query(
        "insert into organization_modules (organization_id, module_key, enabled, enabled_at) \
         values ($1, $2, $3, case when $3 then now() else null end) \
         on conflict (organization_id, module_key) do update \
            set enabled = excluded.enabled, \
                enabled_at = case when excluded.enabled then now() else null end, \
                updated_at = now()",
    )
    .bind(organization_id)
    .bind(&key)
    .bind(enabled)
    .execute(pool)
    .await?;

    Ok(ModuleState {
        key: module.key.to_owned(),
        name: module.name.to_owned(),
        description: module.description.to_owned(),
        enabled,
        explicit: true,
    })
}

/// Drop an organization's decision about a module, returning it to the platform default.
///
/// The Modules tab needs this: a switch that has never been touched is "on by default", and the
/// only way back to that state is to remove the row rather than to store `enabled = true`,
/// which would claim the organization had chosen it.
pub async fn clear_module_decision(
    pool: &PgPool,
    organization_id: Uuid,
    module_key: &str,
) -> Result<ModuleState> {
    let key = validate_module_key(module_key)?;
    let Some(module) = installed_modules().into_iter().find(|m| m.key == key) else {
        return Err(IdentityError::ModuleNotInstalled(key));
    };

    sqlx::query("delete from organization_modules where organization_id = $1 and module_key = $2")
        .bind(organization_id)
        .bind(&key)
        .execute(pool)
        .await?;

    Ok(ModuleState {
        key: module.key.to_owned(),
        name: module.name.to_owned(),
        description: module.description.to_owned(),
        enabled: true,
        explicit: false,
    })
}

// ---------------------------------------------------------------------------------------------
// Limits and usage
// ---------------------------------------------------------------------------------------------

/// Read an organization's plan and ceilings, creating the row with defaults when missing.
pub async fn load_limits(pool: &PgPool, organization_id: Uuid) -> Result<OrganizationLimits> {
    let sql = format!(
        "insert into organization_limits (organization_id) values ($1) \
         on conflict (organization_id) do update set organization_id = excluded.organization_id \
         returning {LIMITS_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OrganizationLimits>(&sql)
        .bind(organization_id)
        .fetch_one(pool)
        .await?)
}

/// Apply a limit change, validating the whole set before writing any of it.
pub async fn update_limits(
    pool: &PgPool,
    organization_id: Uuid,
    changes: LimitsChanges,
) -> Result<OrganizationLimits> {
    validate_limits_changes(&changes)?;
    let current = load_limits(pool, organization_id).await?;

    let sql = format!(
        "update organization_limits set plan = $2, seat_limit = $3, site_limit = $4, \
             storage_bytes_limit = $5, ai_monthly_limit_micros = $6, updated_at = now() \
         where organization_id = $1 returning {LIMITS_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OrganizationLimits>(&sql)
        .bind(organization_id)
        .bind(
            changes
                .plan
                .as_deref()
                .map(str::to_owned)
                .unwrap_or(current.plan)
                .as_str(),
        )
        .bind(changes.seat_limit.unwrap_or(current.seat_limit))
        .bind(changes.site_limit.unwrap_or(current.site_limit))
        .bind(
            changes
                .storage_bytes_limit
                .unwrap_or(current.storage_bytes_limit),
        )
        .bind(
            changes
                .ai_monthly_limit_micros
                .unwrap_or(current.ai_monthly_limit_micros),
        )
        .fetch_one(pool)
        .await?)
}

/// Measure what this organization currently holds, next to its ceilings.
///
/// The seat, site and storage numbers are three independent aggregates, so they are three
/// statements rather than one cross join: a `count(*)` over a cartesian product is correct
/// here but costs more, and three round trips to one pool is cheaper than one badly-shaped
/// join.
pub async fn measure_usage(pool: &PgPool, organization_id: Uuid) -> Result<OrganizationUsage> {
    let seats_used: i64 = sqlx::query_scalar(
        "select count(*) from organization_members \
          where organization_id = $1 and status = 'active'",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    let sites_used: i64 =
        sqlx::query_scalar("select count(*) from sites where organization_id = $1")
            .bind(organization_id)
            .fetch_one(pool)
            .await?;

    let storage_used_bytes: i64 = sqlx::query_scalar(
        "select coalesce(sum(m.size_bytes), 0)::bigint from media m \
           join sites s on s.id = m.site_id \
          where s.organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    // AI spend is read from the audit chain rather than a dedicated usage table: the AI Hub
    // records every billable call as an audit row with a `cost_micros` in its metadata, and a
    // dedicated table would be a second place to forget to write to. Rows that carry no cost
    // contribute nothing, which is what `coalesce` is for.
    let ai_micros_this_month: i64 = sqlx::query_scalar(
        "select coalesce(sum(coalesce((metadata->>'cost_micros')::bigint, 0)), 0)::bigint \
           from audit_log \
          where organization_id = $1 \
            and created_at >= date_trunc('month', now())",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok(OrganizationUsage {
        seats_used,
        sites_used,
        storage_used_bytes,
        ai_micros_this_month,
    })
}

/// Has this organization passed its site ceiling?
///
/// Checked *before* the insert rather than counted afterwards, so two concurrent creates
/// cannot both observe "one site left" and both write.
pub async fn check_site_limit(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<LimitExceeded>> {
    let limits = load_limits(pool, organization_id).await?;
    let Some(site_limit) = limits.site_limit else {
        return Ok(None);
    };

    let used: i64 = sqlx::query_scalar("select count(*) from sites where organization_id = $1")
        .bind(organization_id)
        .fetch_one(pool)
        .await?;

    Ok((used >= i64::from(site_limit)).then_some(LimitExceeded {
        resource: "sites",
        used,
        limit: i64::from(site_limit),
    }))
}

/// Has this organization passed its seat ceiling?
///
/// This is the ceiling the REQ asks to be enforced at *invitation acceptance* rather than at
/// inviting: an invitation is a request, and the plan is charged for people who have actually
/// joined. Pending invitations are deliberately not counted here.
pub async fn check_seat_limit(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<LimitExceeded>> {
    let limits = load_limits(pool, organization_id).await?;
    let Some(seat_limit) = limits.seat_limit else {
        return Ok(None);
    };

    let used: i64 = sqlx::query_scalar(
        "select count(*) from organization_members \
          where organization_id = $1 and status = 'active'",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok((used >= i64::from(seat_limit)).then_some(LimitExceeded {
        resource: "seats",
        used,
        limit: i64::from(seat_limit),
    }))
}

/// Is there room for one more active member? Used before a membership is activated.
pub async fn seat_limit_for(pool: &PgPool, organization_id: Uuid) -> Result<Option<i64>> {
    Ok(load_limits(pool, organization_id)
        .await?
        .seat_limit
        .map(i64::from))
}

/// Is there room for one more site? Used before a site is created.
pub async fn site_limit_for(pool: &PgPool, organization_id: Uuid) -> Result<Option<i64>> {
    Ok(load_limits(pool, organization_id)
        .await?
        .site_limit
        .map(i64::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locale_is_accepted_in_the_shapes_the_req_names() {
        assert_eq!(validate_locale("en").expect("en"), "en");
        assert_eq!(validate_locale("en-GB").expect("en-GB"), "en-GB");
        assert_eq!(validate_locale("tr-TR").expect("tr-TR"), "tr-TR");
        assert_eq!(validate_locale(" tr ").expect("trimmed"), "tr");
    }

    #[test]
    fn a_blank_or_shapeless_locale_is_refused() {
        assert!(validate_locale("").is_err());
        assert!(validate_locale("   ").is_err());
        assert!(validate_locale("-en").is_err());
        assert!(validate_locale("en/GB").is_err());
        assert!(validate_locale("1").is_err());
    }

    #[test]
    fn a_timezone_is_an_iana_name() {
        assert_eq!(
            validate_timezone("Europe/Istanbul").expect("zone"),
            "Europe/Istanbul"
        );
        assert_eq!(validate_timezone("UTC").expect("UTC"), "UTC");
        assert!(validate_timezone("").is_err());
        assert!(validate_timezone("Europe/Istanbul; DROP TABLE").is_err());
    }

    #[test]
    fn the_three_invite_policies_are_the_only_ones() {
        for policy in INVITE_POLICIES {
            assert_eq!(
                validate_invite_policy(policy).expect(policy),
                policy,
                "{policy} is one of the three the REQ names"
            );
        }
        assert!(validate_invite_policy("anything-goes").is_err());
        assert!(validate_invite_policy("").is_err());
    }

    #[test]
    fn an_accent_colour_is_six_hex_digits_or_nothing() {
        assert_eq!(
            validate_accent_color(Some("#2F6F4F")).expect("uppercase"),
            Some("#2f6f4f".to_owned()),
            "the picker sends uppercase; the stored value compares equal anyway"
        );
        assert_eq!(
            validate_accent_color(Some("2f6f4f")).expect("no hash"),
            Some("#2f6f4f".to_owned())
        );
        assert_eq!(validate_accent_color(None).expect("unset"), None);
        assert_eq!(
            validate_accent_color(Some("   ")).expect("blank clears it"),
            None
        );
        assert!(validate_accent_color(Some("#fff")).is_err());
        assert!(validate_accent_color(Some("#2f6f4g")).is_err());
    }

    #[test]
    fn audit_retention_stays_inside_the_days_the_req_allows() {
        assert_eq!(validate_audit_retention(30).expect("min"), 30);
        assert_eq!(validate_audit_retention(3650).expect("max"), 3650);
        assert_eq!(validate_audit_retention(365).expect("default"), 365);
        assert!(validate_audit_retention(29).is_err());
        assert!(validate_audit_retention(3651).is_err());
    }

    #[test]
    fn a_limit_is_absent_or_positive_and_never_zero() {
        let zero: Option<i32> = Some(0);
        assert!(
            validate_limit(zero, "seat_limit").is_err(),
            "a ceiling of zero is not a plan, it is an outage"
        );
        let negative: Option<i32> = Some(-5);
        assert!(validate_limit(negative, "seat_limit").is_err());
        assert_eq!(
            validate_limit(None::<i32>, "seat_limit").expect("null"),
            None
        );
        assert_eq!(
            validate_limit(Some(10_i32), "seat_limit").expect("ten"),
            Some(10)
        );
    }

    #[test]
    fn the_three_plans_are_the_only_ones() {
        for plan in PLANS {
            assert_eq!(validate_plan(plan).expect(plan), plan);
        }
        assert!(validate_plan("platinum").is_err());
        assert!(validate_plan("").is_err());
    }

    #[test]
    fn a_module_key_follows_the_site_key_shape() {
        assert_eq!(validate_module_key("AI-Hub").expect("normalized"), "ai-hub");
        assert!(validate_module_key("a").is_err());
        assert!(validate_module_key("-ai").is_err());
        assert!(validate_module_key("ai-").is_err());
        assert!(validate_module_key("ai hub").is_err());
    }

    #[test]
    fn a_partial_limits_change_validates_only_what_it_carries() {
        let plan_only = LimitsChanges {
            plan: Some("enterprise".to_owned()),
            ..LimitsChanges::default()
        };
        assert!(validate_limits_changes(&plan_only).is_ok());

        let bad_plan = LimitsChanges {
            plan: Some("platinum".to_owned()),
            ..LimitsChanges::default()
        };
        assert!(validate_limits_changes(&bad_plan).is_err());

        let zero_seats = LimitsChanges {
            seat_limit: Some(Some(0)),
            ..LimitsChanges::default()
        };
        assert!(validate_limits_changes(&zero_seats).is_err());
    }

    #[test]
    fn a_partial_settings_change_changes_nothing_when_it_is_empty() {
        assert!(SettingsChanges::default().is_empty());
        assert!(LimitsChanges::default().is_empty());
        assert!(
            !SettingsChanges {
                locale: Some("tr".to_owned()),
                ..SettingsChanges::default()
            }
            .is_empty()
        );
        assert!(
            !LimitsChanges {
                seat_limit: Some(Some(4)),
                ..LimitsChanges::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn the_installation_ships_modules_with_keys_the_schema_accepts() {
        for module in installed_modules() {
            assert_eq!(
                validate_module_key(module.key).expect(module.key),
                module.key,
                "every installed module must pass the shape the column's check enforces"
            );
        }
    }

    #[test]
    fn a_refusal_names_the_ceiling_and_the_current_count() {
        let exceeded = LimitExceeded {
            resource: "seats",
            used: 5,
            limit: 5,
        };
        let message = exceeded.message();
        assert!(
            message.contains('5'),
            "the message names the number: {message}"
        );
        assert!(message.contains("members"), "and the resource: {message}");
    }
}
