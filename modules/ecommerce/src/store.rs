//! Storefront settings storage: read a site's configuration, write it back, and the one
//! place the row is created.
//!
//! REQ-118, slice 1a. The validation is [`crate::settings`] and is pure and unit-tested; this
//! file is the SQL that feeds it. The split is the branch's standing pattern: the rules can be
//! proven without a database *and* verified against a real one, and the two are the same
//! function reading the same column.
//!
//! ## Why `load` never says "no settings"
//!
//! The migration seeds one row per *existing* site, and a site created afterwards has none —
//! so the read path has to answer something. It answers [`StorefrontSettings::defaults`]
//! rather than a 404 or an error, and says so in a flag, because a site that renders with
//! platform defaults and no explanation looks exactly like a site whose operator chose those
//! values. `row_exists` is what the panel uses to show "not configured yet" without the
//! client guessing.
//!
//! The alternative — failing the read — would take a shop's entire public site down because
//! one settings row is missing, which is a far worse failure than a default nobody chose.
//!
//! ## A note on row decoding
//!
//! [`SettingsRow`] is a `FromRow` struct rather than a tuple. A tuple type annotation is
//! positional: the ninth element of the type is the ninth element of the `select`, and moving
//! a column in the SQL without moving it in the annotation produces a `query_as` that
//! compiles and then decodes each value into the wrong field. `FromRow` matches by name, so a
//! reordered `select` is a compile error or a correct read rather than a plausible one.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::settings::StorefrontSettings;

/// One row of `storefront_settings`, decoded by column name.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SettingsRow {
    /// The site these settings belong to.
    pub site_id: Uuid,
    /// Whether a visitor may check out without an account.
    pub guest_checkout: bool,
    /// `inclusive` or `exclusive`. Presentation only.
    pub tax_display: String,
    /// The listing layout the theme should render.
    pub listing_variant: String,
    /// How many products one listing page holds.
    pub page_size: i32,
    /// How the listing is cut into pages.
    pub pagination: String,
    /// The largest quantity one order line may carry.
    pub per_order_item_max: i32,
    /// Whether the wishlist is offered at all.
    pub wishlist_enabled: bool,
    /// Stock at or below which a product card grows a low-stock badge.
    pub low_stock_badge_threshold: i32,
    /// How long a cart with items may sit before the sweep may call it abandoned.
    pub abandonment_hours: i32,
    /// The mail template reference for the order confirmation.
    pub confirmation_template: String,
    /// ISO 4217-shaped currency code.
    pub currency: String,
}

impl From<SettingsRow> for StorefrontSettings {
    fn from(row: SettingsRow) -> Self {
        Self {
            site_id: row.site_id,
            guest_checkout: row.guest_checkout,
            tax_display: row.tax_display,
            listing_variant: row.listing_variant,
            page_size: row.page_size,
            pagination: row.pagination,
            per_order_item_max: row.per_order_item_max,
            wishlist_enabled: row.wishlist_enabled,
            low_stock_badge_threshold: row.low_stock_badge_threshold,
            abandonment_hours: row.abandonment_hours,
            confirmation_template: row.confirmation_template,
            currency: row.currency,
        }
    }
}

/// A site's settings together with whether the row actually exists.
///
/// Two fields rather than an `Option<StorefrontSettings>`, because a caller almost never
/// wants to branch: it wants settings to render with, and — only the settings screen — a way
/// to tell "the operator chose this" from "nobody has configured this site yet".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSettings {
    /// The configuration to serve. The defaults when no row exists.
    pub settings: StorefrontSettings,
    /// Whether a row backs these values. `false` means every field is a platform default.
    pub row_exists: bool,
}

impl LoadedSettings {
    /// The platform defaults for a site with no row, marked as unconfigured.
    #[must_use]
    pub fn unconfigured(site_id: Uuid) -> Self {
        Self {
            settings: StorefrontSettings::defaults(site_id),
            row_exists: false,
        }
    }
}

const SETTINGS_COLUMNS: &str = "site_id, guest_checkout, tax_display, listing_variant, \
     page_size, pagination, per_order_item_max, wishlist_enabled, low_stock_badge_threshold, \
     abandonment_hours, confirmation_template, currency";

/// Read a site's settings, falling back to the platform defaults.
///
/// `organization_id` is part of the read, not a filter applied afterwards: it is what makes a
/// foreign `site_id` answer "defaults" rather than another organization's configuration, so a
/// caller who guessed a site id gets a working shop rather than a copy of somebody else's
/// settings. Callers that need to *tell* the difference read [`LoadedSettings::row_exists`].
pub async fn load(pool: &PgPool, organization_id: Uuid, site_id: Uuid) -> Result<LoadedSettings> {
    let row: Option<SettingsRow> = sqlx::query_as(&format!(
        "select {SETTINGS_COLUMNS} from storefront_settings \
         where site_id = $1 and site_id in \
             (select id from sites where organization_id = $2)"
    ))
    .bind(site_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some(row) => LoadedSettings {
            settings: row.into(),
            row_exists: true,
        },
        None => LoadedSettings::unconfigured(site_id),
    })
}

/// Whether a settings row exists for this site inside this organization.
pub async fn row_exists(pool: &PgPool, organization_id: Uuid, site_id: Uuid) -> Result<bool> {
    let found: Option<(bool,)> = sqlx::query_as(
        "select true from storefront_settings s \
         join sites t on t.id = s.site_id \
         where s.site_id = $1 and t.organization_id = $2",
    )
    .bind(site_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Write a site's settings, creating the row when it is absent.
///
/// The caller validates first ([`StorefrontSettings::normalize`]), and the `check`
/// constraints in the migration are the backstop rather than the only guard — a constraint
/// violation arrives as a `Database` error with no field name in it, which is exactly the
/// message a settings form cannot show under an input.
///
/// The write is an `insert … on conflict (site_id) do update` rather than an update that has
/// to check first: a read-then-write here is two round trips and a race two admins saving the
/// settings screen at once can lose.
///
/// **The `where exists` guard is not decoration.** `on conflict do update` would otherwise
/// create a row for a site this organization does not own, because the conflict target is the
/// site itself and a foreign id simply does not conflict. The guard turns a guessed site id
/// into a write of zero rows — and [`rows_affected`] below is what turns *that* into a
/// refusal rather than a silent success.
pub async fn save(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &StorefrontSettings,
    updated_by: Option<Uuid>,
) -> Result<bool> {
    let result = sqlx::query(
        "insert into storefront_settings \
             (site_id, guest_checkout, tax_display, listing_variant, page_size, pagination, \
              per_order_item_max, wishlist_enabled, low_stock_badge_threshold, \
              abandonment_hours, confirmation_template, currency, updated_by) \
         select $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13 \
         where exists (select 1 from sites where id = $1 and organization_id = $14) \
         on conflict (site_id) do update set \
             guest_checkout = excluded.guest_checkout, \
             tax_display = excluded.tax_display, \
             listing_variant = excluded.listing_variant, \
             page_size = excluded.page_size, \
             pagination = excluded.pagination, \
             per_order_item_max = excluded.per_order_item_max, \
             wishlist_enabled = excluded.wishlist_enabled, \
             low_stock_badge_threshold = excluded.low_stock_badge_threshold, \
             abandonment_hours = excluded.abandonment_hours, \
             confirmation_template = excluded.confirmation_template, \
             currency = excluded.currency, \
             updated_by = excluded.updated_by, \
             updated_at = now()",
    )
    .bind(settings.site_id)
    .bind(settings.guest_checkout)
    .bind(&settings.tax_display)
    .bind(&settings.listing_variant)
    .bind(settings.page_size)
    .bind(&settings.pagination)
    .bind(settings.per_order_item_max)
    .bind(settings.wishlist_enabled)
    .bind(settings.low_stock_badge_threshold)
    .bind(settings.abandonment_hours)
    .bind(&settings.confirmation_template)
    .bind(&settings.currency)
    .bind(updated_by)
    .bind(organization_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// A site and its settings, from the `left join` that includes unconfigured sites.
///
/// Every settings column is `Option` because a site with no row produces NULL for all of
/// them, and a NOT NULL field here would make the one query that is supposed to enumerate
/// *all* shops fail on the shops nobody has configured yet. `site_id` is not optional: it
/// comes from `sites`, which is the side of the join that always has a row.
#[derive(Debug, Clone, sqlx::FromRow)]
struct SiteWithSettings {
    site_id: Uuid,
    guest_checkout: Option<bool>,
    tax_display: Option<String>,
    listing_variant: Option<String>,
    page_size: Option<i32>,
    pagination: Option<String>,
    per_order_item_max: Option<i32>,
    wishlist_enabled: Option<bool>,
    low_stock_badge_threshold: Option<i32>,
    abandonment_hours: Option<i32>,
    confirmation_template: Option<String>,
    currency: Option<String>,
}

impl SiteWithSettings {
    /// Whether a settings row backed this site.
    ///
    /// Read from `tax_display` rather than assumed: it is `not null` in the table, so it is
    /// `Some` exactly when the row exists. Testing "was the row found" by asking whether the
    /// *last* selected column is present is the kind of inference that breaks when somebody
    /// adds a nullable column to the end of the list.
    fn is_configured(&self) -> bool {
        self.tax_display.is_some()
    }

    /// The settings to serve, defaults applied when unconfigured.
    ///
    /// Every field falls back independently rather than the row being taken or discarded as a
    /// whole. A partially-null row cannot occur through [`save`] (it writes every column), but
    /// it *can* occur through a hand-edited row or an older migration, and a row that is
    /// missing one column should not take a shop's tax wording down with it.
    fn into_loaded_settings(self) -> LoadedSettings {
        let site_id = self.site_id;
        let configured = self.is_configured();
        let defaults = StorefrontSettings::defaults(site_id);
        LoadedSettings {
            settings: StorefrontSettings {
                site_id,
                guest_checkout: self.guest_checkout.unwrap_or(defaults.guest_checkout),
                tax_display: self.tax_display.unwrap_or(defaults.tax_display),
                listing_variant: self.listing_variant.unwrap_or(defaults.listing_variant),
                page_size: self.page_size.unwrap_or(defaults.page_size),
                pagination: self.pagination.unwrap_or(defaults.pagination),
                per_order_item_max: self
                    .per_order_item_max
                    .unwrap_or(defaults.per_order_item_max),
                wishlist_enabled: self.wishlist_enabled.unwrap_or(defaults.wishlist_enabled),
                low_stock_badge_threshold: self
                    .low_stock_badge_threshold
                    .unwrap_or(defaults.low_stock_badge_threshold),
                abandonment_hours: self.abandonment_hours.unwrap_or(defaults.abandonment_hours),
                confirmation_template: self
                    .confirmation_template
                    .unwrap_or(defaults.confirmation_template),
                currency: self.currency.unwrap_or(defaults.currency),
            },
            row_exists: configured,
        }
    }
}

/// Every site in an organization with the settings the storefront should serve.
///
/// The abandoned-cart sweep and the catalogue's per-site cache warm-up both need "all the
/// shops", and both need the defaults applied for a site nobody has configured — so this
/// returns [`LoadedSettings`] rather than rows, and a caller cannot accidentally serve an
/// empty site.
///
/// **The `left join` is load-bearing.** An `inner join` returns only the configured sites, and
/// "all the shops" would quietly mean "all the shops somebody has opened the settings screen
/// for" — the shape of bug that makes a freshly created site the one site a deployment sweep
/// skips. There is a test for exactly this.
pub async fn load_all(pool: &PgPool, organization_id: Uuid) -> Result<Vec<LoadedSettings>> {
    let rows: Vec<SiteWithSettings> = sqlx::query_as(
        "select t.id as site_id, s.guest_checkout, s.tax_display, s.listing_variant, \
                s.page_size, s.pagination, s.per_order_item_max, s.wishlist_enabled, \
                s.low_stock_badge_threshold, s.abandonment_hours, \
                s.confirmation_template, s.currency \
         from sites t \
         left join storefront_settings s on s.site_id = t.id \
         where t.organization_id = $1 \
         order by t.key",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(SiteWithSettings::into_loaded_settings)
        .collect())
}
