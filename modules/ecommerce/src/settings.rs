//! The per-site storefront settings: the model, its validation and the error the panel turns
//! into a message under a field.
//!
//! REQ-118, slice 1a. Twelve of this request's acceptance lines end in a number a customer
//! sees, and every one of them is a setting on this struct.
//!
//! ## Why the validation returns a *field*
//!
//! `validate` returns [`SettingError::Field { field, message }`] rather than `Result<(), String>`,
//! and the reason is the acceptance line it serves: *"disabling guest checkout forces the
//! sign-in step, switching tax display changes the labels … and raising the per-order maximum
//! changes the stepper cap."* A setting screen that answers a bad value with `"invalid input"`
//! makes the operator hunt for the field; one that answers `"per_order_item_max: must be
//! between 1 and 100"` puts the message under the input that caused it. The name is carried
//! as a constant rather than typed at the call site, so a rename of a column cannot leave a
//! message pointing at an input that no longer has that name.

use serde::{Deserialize, Serialize};

use crate::vocabulary::{
    DEFAULT_CONFIRMATION_TEMPLATE, MAX_ABANDONMENT_HOURS, MAX_LOW_STOCK_BADGE_THRESHOLD,
    MAX_PAGE_SIZE, MAX_PER_ORDER_ITEM_MAX, MIN_ABANDONMENT_HOURS, MIN_LOW_STOCK_BADGE_THRESHOLD,
    MIN_PAGE_SIZE, MIN_PER_ORDER_ITEM_MAX, is_abandonment_hours, is_currency_code,
    is_listing_variant, is_low_stock_badge_threshold, is_page_size, is_pagination,
    is_per_order_item_max, is_tax_display, normalize_template,
};

/// The column names, as the panel's form field ids. A validation message names one of these,
/// so they live in one place next to the struct.
pub mod fields {
    /// Guest checkout checkbox.
    pub const GUEST_CHECKOUT: &str = "guest_checkout";
    /// Tax display radio.
    pub const TAX_DISPLAY: &str = "tax_display";
    /// Listing variant select.
    pub const LISTING_VARIANT: &str = "listing_variant";
    /// Page size number input.
    pub const PAGE_SIZE: &str = "page_size";
    /// Pagination select.
    pub const PAGINATION: &str = "pagination";
    /// Per-order maximum number input.
    pub const PER_ORDER_ITEM_MAX: &str = "per_order_item_max";
    /// Wishlist toggle.
    pub const WISHLIST_ENABLED: &str = "wishlist_enabled";
    /// Low-stock badge threshold number input.
    pub const LOW_STOCK_BADGE_THRESHOLD: &str = "low_stock_badge_threshold";
    /// Abandonment window number input.
    pub const ABANDONMENT_HOURS: &str = "abandonment_hours";
    /// Confirmation template text input.
    pub const CONFIRMATION_TEMPLATE: &str = "confirmation_template";
    /// Currency select.
    pub const CURRENCY: &str = "currency";
}

/// One site's storefront configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorefrontSettings {
    /// The site these settings belong to.
    pub site_id: uuid::Uuid,
    /// Whether a visitor may check out without an account.
    pub guest_checkout: bool,
    /// `inclusive` or `exclusive`. Presentation only — the stored amounts never change.
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

impl StorefrontSettings {
    /// The platform defaults, for a site whose row has not been created yet.
    ///
    /// These are the same numbers the migration's column defaults carry, written a third
    /// time on purpose: the read path calls this when a site predates the migration and
    /// would otherwise have to answer "no settings" to a public route. The migration-agreement
    /// test in [`crate::vocabulary`] is what keeps the third copy honest.
    #[must_use]
    pub fn defaults(site_id: uuid::Uuid) -> Self {
        Self {
            site_id,
            guest_checkout: true,
            tax_display: "inclusive".to_string(),
            listing_variant: "grid".to_string(),
            page_size: 24,
            pagination: "pagination".to_string(),
            per_order_item_max: 20,
            wishlist_enabled: true,
            low_stock_badge_threshold: 5,
            abandonment_hours: 24,
            confirmation_template: DEFAULT_CONFIRMATION_TEMPLATE.to_string(),
            currency: "EUR".to_string(),
        }
    }

    /// What a quantity stepper will offer, in the wording the storefront uses.
    ///
    /// This exists so the *cap* is one value read from one place. A stepper that recomputes
    /// its maximum in the browser, from a setting it fetched separately, is a stepper that
    /// eventually offers 500 of an item the server then refuses — and the customer is told
    /// the maximum by the server, after they have built a basket around it.
    #[must_use]
    pub fn quantity_cap(&self) -> i32 {
        self.per_order_item_max
    }

    /// Whether a stock count should grow a low-stock badge.
    ///
    /// "At or below", not "below": a threshold of five that badges six and not five is a
    /// number the operator has to learn rather than a rule.
    #[must_use]
    pub fn is_low_stock(&self, available: i32) -> bool {
        available <= self.low_stock_badge_threshold
    }

    /// Whether the price shown to a customer is tax-inclusive.
    #[must_use]
    pub fn shows_tax_inclusive(&self) -> bool {
        self.tax_display == "inclusive"
    }

    /// Whether a cart whose last activity was `last_activity_at` is abandoned as of `now`.
    ///
    /// This is the function the sweep asks, and the reason it takes *both* instants is the
    /// case a cutoff alone cannot see: a `last_activity_at` in the **future** means this
    /// process's clock is behind the one that wrote the row. "Abandoned" is then not a
    /// question anybody can answer, and answering it "yes" drops a basket somebody is in the
    /// middle of filling — so it answers "no" and leaves the row alone until the clocks agree.
    /// The alternative (a cutoff, compared against the row) turns that same skew into a
    /// silently-true comparison, because every cutoff is trivially "before" a future row.
    #[must_use]
    pub fn is_abandoned(
        &self,
        last_activity_at: time::OffsetDateTime,
        now: time::OffsetDateTime,
    ) -> bool {
        match self.abandonment_cutoff(now) {
            // A window that cannot be measured (skew) is a window that has not elapsed.
            None => false,
            Some(cutoff) => last_activity_at <= cutoff,
        }
    }

    /// The instant before which an active cart is abandoned.
    ///
    /// `None` only when `now` is early enough that subtracting the window underflows, which
    /// in practice means a clock set near the epoch. Callers that are asking the real question
    /// want [`StorefrontSettings::is_abandoned`], which also refuses a skewed `last_activity_at`.
    #[must_use]
    pub fn abandonment_cutoff(&self, now: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
        let hours = i64::from(self.abandonment_hours);
        now.checked_sub(time::Duration::hours(hours))
    }

    /// Check every field, naming the first one that is wrong.
    ///
    /// The order is deliberate: closed lists first (a select that sent an unknown value), then
    /// the numeric bounds, then the shape checks. An operator fixing a form fixes the visible
    /// complaint, and returning the *first* field in form order means the message appears
    /// under the control the operator is already looking at.
    pub fn validate(&self) -> Result<(), SettingError> {
        if !is_tax_display(&self.tax_display) {
            return Err(SettingError::Field {
                field: fields::TAX_DISPLAY,
                message: format!(
                    "tax display must be one of {}",
                    crate::vocabulary::TAX_DISPLAYS.join(", ")
                ),
            });
        }
        if !is_listing_variant(&self.listing_variant) {
            return Err(SettingError::Field {
                field: fields::LISTING_VARIANT,
                message: format!(
                    "listing variant must be one of {}",
                    crate::vocabulary::LISTING_VARIANTS.join(", ")
                ),
            });
        }
        if !is_pagination(&self.pagination) {
            return Err(SettingError::Field {
                field: fields::PAGINATION,
                message: format!(
                    "pagination must be one of {}",
                    crate::vocabulary::PAGINATIONS.join(", ")
                ),
            });
        }
        if !is_page_size(self.page_size) {
            return Err(SettingError::Field {
                field: fields::PAGE_SIZE,
                message: format!("page size must be between {MIN_PAGE_SIZE} and {MAX_PAGE_SIZE}"),
            });
        }
        if !is_per_order_item_max(self.per_order_item_max) {
            return Err(SettingError::Field {
                field: fields::PER_ORDER_ITEM_MAX,
                message: format!(
                    "per-order maximum must be between {MIN_PER_ORDER_ITEM_MAX} and \
                     {MAX_PER_ORDER_ITEM_MAX}"
                ),
            });
        }
        if !is_low_stock_badge_threshold(self.low_stock_badge_threshold) {
            return Err(SettingError::Field {
                field: fields::LOW_STOCK_BADGE_THRESHOLD,
                message: format!(
                    "low-stock threshold must be between {MIN_LOW_STOCK_BADGE_THRESHOLD} and \
                     {MAX_LOW_STOCK_BADGE_THRESHOLD}"
                ),
            });
        }
        if !is_abandonment_hours(self.abandonment_hours) {
            return Err(SettingError::Field {
                field: fields::ABANDONMENT_HOURS,
                message: format!(
                    "abandonment window must be between {MIN_ABANDONMENT_HOURS} and \
                     {MAX_ABANDONMENT_HOURS} hours"
                ),
            });
        }
        if !is_currency_code(&self.currency) {
            return Err(SettingError::Field {
                field: fields::CURRENCY,
                message: "currency must be three upper-case letters (ISO 4217)".to_string(),
            });
        }
        Ok(())
    }

    /// Normalize the free-text fields, then validate.
    ///
    /// The template is trimmed and a blank one falls back to the platform default. A stored
    /// empty string here is not a harmless value: it names a template that does not exist, and
    /// the order confirmation is then sent with no body — a failure that shows up days later
    /// as a support ticket, from the one code path nobody tests because the mail is real.
    pub fn normalize(&mut self) -> Result<(), SettingError> {
        self.confirmation_template = normalize_template(Some(&self.confirmation_template));
        self.tax_display = self.tax_display.trim().to_ascii_lowercase();
        self.listing_variant = self.listing_variant.trim().to_ascii_lowercase();
        self.pagination = self.pagination.trim().to_ascii_lowercase();
        self.currency = self.currency.trim().to_ascii_uppercase();
        self.validate()
    }
}

/// Why a settings write was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingError {
    /// A field is not acceptable. `field` is one of [`fields`], so the panel can put the
    /// message under the control that caused it instead of at the top of the form.
    #[error("{message} ({field})")]
    Field {
        /// The setting's column name, matching the panel's form field id.
        field: &'static str,
        /// A sentence an operator can act on, naming the accepted range or list.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> StorefrontSettings {
        StorefrontSettings::defaults(uuid::Uuid::new_v4())
    }

    #[test]
    fn the_defaults_validate() {
        // A default that fails its own validation is a settings screen that cannot be saved.
        assert!(settings().validate().is_ok());
    }

    #[test]
    fn an_unknown_tax_display_names_the_field_and_the_list() {
        let mut s = settings();
        s.tax_display = "excluded".to_string();
        let err = s.validate().expect_err("an unknown tax display is refused");
        assert_eq!(
            err,
            SettingError::Field {
                field: fields::TAX_DISPLAY,
                message: "tax display must be one of inclusive, exclusive".to_string(),
            }
        );
        // And the message a human reads does not lead with a Rust variant name.
        assert!(err.to_string().contains("inclusive, exclusive"));
    }

    #[test]
    fn a_page_size_outside_the_band_names_its_own_bounds() {
        let mut s = settings();
        s.page_size = 3;
        let err = s.validate().expect_err("3 is below the band");
        assert!(err.to_string().contains("between 4 and 96"), "{err}");

        s.page_size = 97;
        let err = s.validate().expect_err("97 is above the band");
        assert!(err.to_string().contains("between 4 and 96"), "{err}");
    }

    #[test]
    fn a_zero_abandonment_window_names_the_field() {
        let mut s = settings();
        s.abandonment_hours = 0;
        let err = s
            .validate()
            .expect_err("a cart abandoned at birth is refused");
        assert_eq!(
            err,
            SettingError::Field {
                field: fields::ABANDONMENT_HOURS,
                message: "abandonment window must be between 1 and 720 hours".to_string(),
            }
        );
    }

    #[test]
    fn a_blank_template_reference_is_normalized_before_it_is_stored() {
        let mut s = settings();
        s.confirmation_template = "   ".to_string();
        s.normalize()
            .expect("a blank reference falls back rather than failing");
        assert_eq!(s.confirmation_template, DEFAULT_CONFIRMATION_TEMPLATE);
    }

    #[test]
    fn a_lowercase_currency_is_uppercased_and_a_long_one_is_refused() {
        let mut s = settings();
        s.currency = "try".to_string();
        s.normalize().expect("a lowercase code is normalized");
        assert_eq!(s.currency, "TRY");

        s.currency = "EURO".to_string();
        assert!(s.validate().is_err(), "EURO is not an ISO 4217 shape");
    }

    #[test]
    fn the_badge_threshold_is_inclusive_at_its_own_number() {
        let mut s = settings();
        s.low_stock_badge_threshold = 5;
        assert!(s.is_low_stock(5), "at the threshold is low stock");
        assert!(s.is_low_stock(1));
        assert!(!s.is_low_stock(6), "one above the threshold is not");
    }

    #[test]
    fn the_quantity_cap_is_the_setting_and_not_a_browser_guess() {
        let mut s = settings();
        s.per_order_item_max = 3;
        assert_eq!(s.quantity_cap(), 3);
    }

    #[test]
    fn the_tax_wording_follows_the_setting() {
        let mut s = settings();
        assert!(s.shows_tax_inclusive());
        s.tax_display = "exclusive".to_string();
        assert!(!s.shows_tax_inclusive());
    }

    #[test]
    fn a_cart_is_abandoned_only_once_its_window_has_elapsed() {
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(10_000);
        let s = settings(); // 24 hours
        assert_eq!(
            s.abandonment_cutoff(now),
            Some(now - time::Duration::hours(24))
        );

        // 23h59m old: still being filled.
        assert!(!s.is_abandoned(
            now - time::Duration::hours(23) - time::Duration::minutes(59),
            now
        ));
        // Exactly 24h old is abandoned — the comparison is `<=`, so the window is a promise the
        // sweep keeps rather than one it overshoots by however long the tick is.
        assert!(s.is_abandoned(now - time::Duration::hours(24), now));
        assert!(s.is_abandoned(now - time::Duration::hours(72), now));
    }

    #[test]
    fn a_clock_skewed_into_the_future_never_abandons_a_live_cart() {
        // The regression this pair of functions exists to prevent: the process that wrote the
        // row has a clock behind ours, so `last_activity_at` reads as being in the future.
        // Every cutoff is trivially "before" a future instant, so a cutoff-only comparison
        // would call this abandoned and drop a basket somebody is still typing into.
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(10_000);
        let s = settings();
        assert!(!s.is_abandoned(now + time::Duration::days(1), now));
    }

    #[test]
    fn a_window_measured_from_the_epoch_is_not_a_window() {
        // Underflow: subtracting 24 hours from a clock sitting on the epoch has no answer, and
        // "no answer" has to mean "not abandoned" rather than panicking inside a sweep worker.
        let s = settings();
        assert!(!s.is_abandoned(
            time::OffsetDateTime::UNIX_EPOCH,
            time::OffsetDateTime::UNIX_EPOCH
        ));
    }
}
