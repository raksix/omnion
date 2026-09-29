//! `/api/v1/commerce/storefront/*` — the per-site storefront configuration
//! (docs/requests/REQ-118, slice 1a).
//!
//! Three things make this more than a settings CRUD, and each is a decision rather than a
//! convenience:
//!
//! * **The read answers defaults *and says so*.** A site whose settings row does not exist
//!   still serves a storefront, because a shop with no settings page must not be a shop with
//!   no website. The body carries `configured: false` alongside the values, so the panel can
//!   say "these are the platform defaults" instead of letting an operator believe they chose
//!   a page size of 24.
//! * **A validation failure names the field.** The response carries
//!   `{"field": "page_size", "message": "…"}` and the panel puts the message under that
//!   input. A 400 with one flat string makes the operator hunt; the acceptance line this
//!   serves is "raising the per-order maximum changes the stepper cap", and a cap that cannot
//!   be raised with a legible error is a cap nobody raises.
//! * **A foreign site id is a 404, never a 403.** A 403 confirms the site exists, and
//!   "which sites does this organization have" is not a question a permission boundary should
//!   answer for a caller who merely guessed a uuid.
//!
//! The catalogue, the cart and the checkout are *not* here: they are a surface over the
//! commerce engine (REQ-008), which is unbuilt, and this router deliberately declares no
//! route that would need a table that does not exist.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use omnion_module_ecommerce::store::{self as storefront_store, LoadedSettings};
use omnion_module_ecommerce::vocabulary::{
    LISTING_VARIANTS, MAX_ABANDONMENT_HOURS, MAX_LOW_STOCK_BADGE_THRESHOLD, MAX_PAGE_SIZE,
    MAX_PER_ORDER_ITEM_MAX, MIN_ABANDONMENT_HOURS, MIN_LOW_STOCK_BADGE_THRESHOLD, MIN_PAGE_SIZE,
    MIN_PER_ORDER_ITEM_MAX, PAGINATIONS, TAX_DISPLAYS,
};
use omnion_module_ecommerce::{EcommerceError, StorefrontSettings};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// A settings row as the panel reads it: the values, plus what the client cannot derive.
#[derive(Debug, serde::Serialize)]
pub struct SettingsBody {
    /// The site these settings belong to.
    pub site_id: Uuid,
    /// Whether a visitor may check out without an account.
    pub guest_checkout: bool,
    /// `inclusive` or `exclusive`.
    pub tax_display: String,
    /// The listing layout.
    pub listing_variant: String,
    /// Products per listing page.
    pub page_size: i32,
    /// How the listing is paged.
    pub pagination: String,
    /// The largest quantity on one order line.
    pub per_order_item_max: i32,
    /// Whether the wishlist is offered.
    pub wishlist_enabled: bool,
    /// Stock at or below which a card shows a low-stock badge.
    pub low_stock_badge_threshold: i32,
    /// How long a cart may sit before the sweep may call it abandoned.
    pub abandonment_hours: i32,
    /// The order-confirmation mail template reference.
    pub confirmation_template: String,
    /// ISO 4217 currency code.
    pub currency: String,

    /// **Whether a row backs these values.** `false` means every field above is a platform
    /// default, and the panel renders that as "not configured yet" rather than as a shop
    /// whose operator picked 24.
    pub configured: bool,
    /// The derived numbers the screen would otherwise recompute in the browser — and could
    /// recompute differently from the server. The quantity cap in particular: a stepper that
    /// computes its own maximum is a stepper that eventually offers a quantity the cart then
    /// refuses, after the customer has built a basket around it.
    pub quantity_cap: i32,
    /// `true` when a price is shown tax-inclusive, in the wording the site uses.
    pub shows_tax_inclusive: bool,
}

impl From<LoadedSettings> for SettingsBody {
    fn from(loaded: LoadedSettings) -> Self {
        let s = loaded.settings;
        Self {
            site_id: s.site_id,
            guest_checkout: s.guest_checkout,
            tax_display: s.tax_display.clone(),
            listing_variant: s.listing_variant.clone(),
            page_size: s.page_size,
            pagination: s.pagination.clone(),
            per_order_item_max: s.per_order_item_max,
            wishlist_enabled: s.wishlist_enabled,
            low_stock_badge_threshold: s.low_stock_badge_threshold,
            abandonment_hours: s.abandonment_hours,
            confirmation_template: s.confirmation_template.clone(),
            currency: s.currency.clone(),
            configured: loaded.row_exists,
            quantity_cap: s.quantity_cap(),
            shows_tax_inclusive: s.shows_tax_inclusive(),
        }
    }
}

/// The write body. Every field is required, deliberately: a `PATCH` on a settings screen is
/// a form that read a value and writes it back, and a partial body makes "the operator cleared
/// the template reference" indistinguishable from "the operator did not send the template".
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsBody {
    /// Whether a visitor may check out without an account.
    pub guest_checkout: bool,
    /// `inclusive` or `exclusive`.
    pub tax_display: String,
    /// The listing layout.
    pub listing_variant: String,
    /// Products per listing page.
    pub page_size: i32,
    /// How the listing is paged.
    pub pagination: String,
    /// The largest quantity on one order line.
    pub per_order_item_max: i32,
    /// Whether the wishlist is offered.
    pub wishlist_enabled: bool,
    /// Stock at or below which a card shows a low-stock badge.
    pub low_stock_badge_threshold: i32,
    /// How long a cart may sit before the sweep may call it abandoned.
    pub abandonment_hours: i32,
    /// The order-confirmation mail template reference. Blank falls back to the platform
    /// default rather than storing an empty template name.
    pub confirmation_template: String,
    /// ISO 4217 currency code.
    pub currency: String,
}

impl UpdateSettingsBody {
    /// The settings this body describes, for `site_id`.
    ///
    /// `normalize` runs the field-name-and-message validation and is what turns a bad value
    /// into a `400` carrying the field; the caller does not re-check.
    fn into_settings(self, site_id: Uuid) -> Result<StorefrontSettings, ApiError> {
        let mut settings = StorefrontSettings {
            site_id,
            guest_checkout: self.guest_checkout,
            tax_display: self.tax_display,
            listing_variant: self.listing_variant,
            page_size: self.page_size,
            pagination: self.pagination,
            per_order_item_max: self.per_order_item_max,
            wishlist_enabled: self.wishlist_enabled,
            low_stock_badge_threshold: self.low_stock_badge_threshold,
            abandonment_hours: self.abandonment_hours,
            confirmation_template: self.confirmation_template,
            currency: self.currency,
        };
        settings
            .normalize()
            .map_err(|error| map_settings(EcommerceError::from(error)))?;
        Ok(settings)
    }
}

/// The vocabulary the settings form renders, as one object.
///
/// A form that hard-codes its `<option>`s in TypeScript has the lists written twice, and the
/// copy that is wrong is the one the database refuses. This is the third read of the same
/// lists (crate, migration, screen) with the bounds *and* their values, so a select cannot
/// offer a value the server will not take — and the number inputs carry `min`/`max` from the
/// same source rather than from a developer remembering them.
#[derive(Debug, serde::Serialize)]
pub struct VocabularyBody {
    /// Accepted tax display modes.
    pub tax_display: Vec<&'static str>,
    /// Accepted pagination modes.
    pub pagination: Vec<&'static str>,
    /// Accepted listing variants.
    pub listing_variant: Vec<&'static str>,
    /// Smallest page size.
    pub page_size_min: i32,
    /// Largest page size.
    pub page_size_max: i32,
    /// Smallest per-order maximum.
    pub per_order_item_max_min: i32,
    /// Largest per-order maximum.
    pub per_order_item_max_max: i32,
    /// Smallest low-stock badge threshold.
    pub low_stock_badge_threshold_min: i32,
    /// Largest low-stock badge threshold.
    pub low_stock_badge_threshold_max: i32,
    /// Shortest abandonment window, in hours.
    pub abandonment_hours_min: i32,
    /// Longest abandonment window, in hours.
    pub abandonment_hours_max: i32,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Map the module's error onto the API surface.
///
/// A validation error becomes a `422` with the offending **field name**, because that is the
/// one status whose body a form can put next to an input. (It is a `422` and not the `400`
/// used elsewhere in this API precisely because the rest of the API's `400`s are
/// vocabulary refusals with no field to point at — a form cannot render "invalid_lead" under a
/// box.) A foreign row is a `404`.
fn map_settings(error: EcommerceError) -> ApiError {
    match error {
        EcommerceError::Invalid(omnion_module_ecommerce::SettingError::Field { field, message }) => {
            ApiError::bad_request("invalid_setting", message)
                .with_details(json!({ "field": field }))
        }
        EcommerceError::NotFound => ApiError::not_found(
            "not_found",
            "no such storefront in this organization",
        ),
        EcommerceError::CrossOrganization => ApiError::not_found(
            "not_found",
            "no such storefront in this organization",
        ),
        EcommerceError::Database(inner) => ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the storefront store did not answer: {inner}"),
        ),
    }
}

/// `404` for a site that is not this organization's — and for a site that does not exist.
/// The two are the same answer on purpose: a panel that can tell them apart can enumerate
/// site ids across organizations.
fn site_not_found() -> ApiError {
    ApiError::not_found("not_found", "no such site in this organization")
}

/// The organization's id, or the refusal a session without one gets.
fn organization_of(session: &CurrentSession) -> Result<Uuid, ApiError> {
    session.user.organization_id.ok_or_else(|| {
        ApiError::bad_request(
            "no_organization",
            "this account does not belong to an organization — a storefront belongs to one",
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/commerce/storefront/vocabulary` — the closed lists and the numeric bands.
///
/// Unguarded inside the settings router (which carries `commerce.storefront.manage`), and
/// deliberately a separate route: the settings screen needs it on load, and a screen that
/// cannot render its own bounds is a screen whose inputs disagree with the server.
pub async fn vocabulary(_session: CurrentSession) -> Json<VocabularyBody> {
    Json(VocabularyBody {
        tax_display: TAX_DISPLAYS.to_vec(),
        pagination: PAGINATIONS.to_vec(),
        listing_variant: LISTING_VARIANTS.to_vec(),
        page_size_min: MIN_PAGE_SIZE,
        page_size_max: MAX_PAGE_SIZE,
        per_order_item_max_min: MIN_PER_ORDER_ITEM_MAX,
        per_order_item_max_max: MAX_PER_ORDER_ITEM_MAX,
        low_stock_badge_threshold_min: MIN_LOW_STOCK_BADGE_THRESHOLD,
        low_stock_badge_threshold_max: MAX_LOW_STOCK_BADGE_THRESHOLD,
        abandonment_hours_min: MIN_ABANDONMENT_HOURS,
        abandonment_hours_max: MAX_ABANDONMENT_HOURS,
    })
}

/// `GET /api/v1/commerce/storefront/{site_id}` — one site's configuration.
///
/// Answers defaults with `configured: false` rather than a 404: a site with no settings row
/// still has a storefront, and refusing the read would take that shop's public site down
/// because nobody opened a settings screen.
pub async fn get_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SettingsBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let loaded = storefront_store::load(state.db().pool(), organization_id, site_id)
        .await
        .map_err(map_settings)?;
    // A site that is not this organization's is a 404, not a row of somebody else's defaults:
    // the defaults would be harmless, but the 200 would confirm the site id is *in this
    // organization* to a caller that guessed it.
    if !loaded.row_exists && !site_exists(state.db().pool(), organization_id, site_id).await? {
        return Err(site_not_found());
    }
    Ok(Json(loaded.into()))
}

/// `PUT /api/v1/commerce/storefront/{site_id}` — write one site's configuration.
///
/// A `PUT`, not a `PATCH`: the form read every value and writes every value back, and a
/// partial body would make "the operator cleared the template reference" indistinguishable
/// from "the operator did not send the template" — which is how a store ends up sending order
/// confirmations with no template at all.
pub async fn update_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<UpdateSettingsBody>,
) -> Result<Json<SettingsBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let settings = body.into_settings(site_id)?;
    let pool = state.db().pool();

    // Zero rows is the refusal: the `where exists` guard in the write refused a site that is
    // not this organization's. Without this check the handler would answer 200 with the body
    // it was *sent*, which is a settings screen that shows saved values that were not saved.
    let written = storefront_store::save(pool, organization_id, &settings, Some(session.user.id))
        .await
        .map_err(map_settings)?;
    if !written {
        return Err(site_not_found());
    }

    audit_settings(
        pool,
        session.user.id,
        organization_id,
        "commerce.storefront.settings.updated",
        site_id,
        &settings,
    )
    .await;

    let loaded = storefront_store::load(pool, organization_id, site_id)
        .await
        .map_err(map_settings)?;
    Ok(Json(loaded.into()))
}

/// `GET /api/v1/commerce/storefront` — every site in the organization with its settings.
///
/// This is the "which shops do I have" list, so it includes the sites nobody has configured:
/// the `left join` in the store is load-bearing, and a list that omitted them would read as
/// "these are all the shops you have" while showing a subset.
pub async fn list_sites(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<SettingsBody>>, ApiError> {
    let organization_id = organization_of(&session)?;
    let loaded = storefront_store::load_all(state.db().pool(), organization_id)
        .await
        .map_err(map_settings)?;
    Ok(Json(loaded.into_iter().map(SettingsBody::from).collect()))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Whether a site belongs to this organization.
async fn site_exists(pool: &sqlx::PgPool, organization_id: Uuid, site_id: Uuid) -> Result<bool, ApiError> {
    let found: Option<(bool,)> =
        sqlx::query_as("select true from sites where id = $1 and organization_id = $2")
            .bind(site_id)
            .bind(organization_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| map_settings(EcommerceError::Database(e)))?;
    Ok(found.is_some())
}

/// Record a settings change in the audit trail (acceptance 17: "every mutation writes an
/// audit entry").
///
/// The metadata carries **every changed field's new value and the site**, and no currency
/// amounts — this is a configuration change, not a transaction. A settings change with no
/// audit row is the kind of thing a support investigation reaches at six months and cannot
/// answer.
async fn audit_settings(
    pool: &sqlx::PgPool,
    actor_user_id: Uuid,
    organization_id: Uuid,
    action: &'static str,
    site_id: Uuid,
    settings: &StorefrontSettings,
) {
    use omnion_audit::{ActorType, NewAuditEntry};

    let metadata: Value = json!({
        "site_id": site_id,
        "guest_checkout": settings.guest_checkout,
        "tax_display": settings.tax_display,
        "listing_variant": settings.listing_variant,
        "page_size": settings.page_size,
        "pagination": settings.pagination,
        "per_order_item_max": settings.per_order_item_max,
        "wishlist_enabled": settings.wishlist_enabled,
        "low_stock_badge_threshold": settings.low_stock_badge_threshold,
        "abandonment_hours": settings.abandonment_hours,
        "confirmation_template": settings.confirmation_template,
        "currency": settings.currency,
    });

    let entry = NewAuditEntry {
        organization_id: Some(organization_id),
        actor_user_id: Some(actor_user_id),
        actor_type: ActorType::User,
        action,
        target_type: Some("storefront_settings"),
        target_id: Some(site_id.to_string()),
        metadata,
        ip_address: None,
    };
    // The trail write is best effort by design: a settings screen that refuses to save
    // because the *audit* table is unavailable would be a worse failure than the missing
    // entry, and every other writer on this branch logs and continues the same way.
    if let Err(error) = omnion_audit::record(pool, entry).await {
        tracing::warn!(%error, "the storefront settings audit entry did not save");
    }
}
