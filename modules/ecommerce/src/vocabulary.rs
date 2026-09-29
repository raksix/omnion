//! The storefront vocabulary: the closed lists and the bounds the panel and the database
//! must agree on.
//!
//! The same rule as `modules/crm-intake/src/vocabulary.rs`, for the same reason. REQ-118
//! splits a set of per-site knobs across three layers — this crate, the panel, and the
//! `check` constraints in `database/migrations/0169_storefront_settings.sql` — and each of
//! them can be changed without touching the other two. A `page_size` of 200 added here and
//! not to SQL passes every unit test in the crate and is then refused by the database on the
//! customer's website; a value the panel offers that the crate rejects is a control that
//! saves an error with no field name in it.
//!
//! **The duplication in SQL is a test, not a comment.** `tests_agree_with_the_migration`
//! reads the migration file and asserts the same numbers and lists appear in it. Both
//! directions fail quietly without it, and both read as "nothing happened".

/// How a tax-inclusive price is worded and shown.
///
/// This is a *presentation* choice. The stored subtotal, the discount, the tax and the total
/// are identical either way — inclusive display divides the stored tax-inclusive total to
/// show the tax line, exclusive display adds the stored tax on top. Deciding it once, here,
/// is what stops the cart and the order confirmation from wording the same total two ways.
pub const TAX_DISPLAYS: [&str; 2] = ["inclusive", "exclusive"];

/// How a listing page is cut into pages.
///
/// `infinite` is a *client* behaviour over the same URLs: every page of an infinite list is
/// also reachable as a real URL, because a crawlable catalogue is a requirement (REQ-118
/// §Risks, "Faceted URLs and SEO") and a scroll that can only be reached by scrolling is a
/// page Google never sees.
pub const PAGINATIONS: [&str; 3] = ["pagination", "load_more", "infinite"];

/// The listing layouts a theme may declare.
pub const LISTING_VARIANTS: [&str; 3] = ["grid", "list", "masonry"];

/// The smallest page size a catalogue will page at.
///
/// Four, not one: a page size of 1 turns every listing into a crawl trap, and a page size of
/// 0 renders an empty page with a working "next" link forever.
pub const MIN_PAGE_SIZE: i32 = 4;

/// The largest page size a catalogue will page at.
///
/// Ninety-six, and the reason is a public endpoint. A listing row costs a join against
/// variants and stock, on an unauthenticated route, for anybody on the internet; an
/// unbounded page size is a denial-of-service knob that reads as a convenience setting.
pub const MAX_PAGE_SIZE: i32 = 96;

/// The smallest quantity ceiling a site may set.
pub const MIN_PER_ORDER_ITEM_MAX: i32 = 1;

/// The largest quantity ceiling a site may set.
///
/// One hundred — the same bound the cart item row carries, so a setting can never permit a
/// quantity the row refuses to store.
pub const MAX_PER_ORDER_ITEM_MAX: i32 = 100;

/// The smallest low-stock badge threshold.
///
/// One, not zero: a threshold of zero badges every product that is in stock, which reads
/// identically to having no badge and trains customers to ignore it.
pub const MIN_LOW_STOCK_BADGE_THRESHOLD: i32 = 1;

/// The largest low-stock badge threshold.
pub const MAX_LOW_STOCK_BADGE_THRESHOLD: i32 = 100;

/// The shortest abandonment window, in hours.
///
/// One hour, and never zero: a cart is a thing a person is *in the middle of filling*, and a
/// zero-hour window would mark it abandoned the moment the row is created — the
/// abandoned-cart campaign would then e-mail people about a basket they are still typing
/// into. The failure is not cosmetic, it is a support and a deliverability problem.
pub const MIN_ABANDONMENT_HOURS: i32 = 1;

/// The longest abandonment window, in hours (thirty days).
///
/// Long enough for a seasonal or B2B basket; bounded because an unbounded window turns the
/// abandoned-cart view into a table of carts that are merely old.
pub const MAX_ABANDONMENT_HOURS: i32 = 720;

/// The mail template reference used when a site has not chosen one.
pub const DEFAULT_CONFIRMATION_TEMPLATE: &str = "order_confirmation";

/// `true` when `value` is a tax display mode the platform knows.
#[must_use]
pub fn is_tax_display(value: &str) -> bool {
    TAX_DISPLAYS.contains(&value)
}

/// `true` when `value` is a pagination mode the platform knows.
#[must_use]
pub fn is_pagination(value: &str) -> bool {
    PAGINATIONS.contains(&value)
}

/// `true` when `value` is a listing variant the platform knows.
#[must_use]
pub fn is_listing_variant(value: &str) -> bool {
    LISTING_VARIANTS.contains(&value)
}

/// `true` when `count` is a page size this platform will serve.
#[must_use]
pub fn is_page_size(count: i32) -> bool {
    (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&count)
}

/// `true` when `count` is a per-order quantity ceiling this platform will accept.
#[must_use]
pub fn is_per_order_item_max(count: i32) -> bool {
    (MIN_PER_ORDER_ITEM_MAX..=MAX_PER_ORDER_ITEM_MAX).contains(&count)
}

/// `true` when `count` is a low-stock badge threshold this platform will accept.
#[must_use]
pub fn is_low_stock_badge_threshold(count: i32) -> bool {
    (MIN_LOW_STOCK_BADGE_THRESHOLD..=MAX_LOW_STOCK_BADGE_THRESHOLD).contains(&count)
}

/// `true` when `hours` is an abandonment window this platform will accept.
#[must_use]
pub fn is_abandonment_hours(hours: i32) -> bool {
    (MIN_ABANDONMENT_HOURS..=MAX_ABANDONMENT_HOURS).contains(&hours)
}

/// `true` when `value` is an ISO 4217-shaped currency code the platform will store.
///
/// Shape, not membership: a hard-coded list of currencies rots the day a country adopts one
/// and the platform refuses a legitimate store setting. Upper case and three letters is what
/// the `char(3)` column and the money formatting actually depend on.
#[must_use]
pub fn is_currency_code(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|b| b.is_ascii_uppercase())
}

/// A stable key for the confirmation template reference, so a blank reference falls back to
/// the platform default rather than to a template named `""` that sends nothing.
#[must_use]
pub fn normalize_template(value: Option<&str>) -> String {
    match value.map(str::trim) {
        Some(reference) if !reference.is_empty() => reference.to_string(),
        _ => DEFAULT_CONFIRMATION_TEMPLATE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_are_exactly_what_the_migration_checks() {
        assert_eq!(TAX_DISPLAYS, ["inclusive", "exclusive"]);
        assert_eq!(PAGINATIONS, ["pagination", "load_more", "infinite"]);
        assert_eq!(LISTING_VARIANTS, ["grid", "list", "masonry"]);
    }

    #[test]
    fn the_bounds_reject_the_values_a_form_would_send() {
        // Zero and negative arrive from a hand-written API call, not from the panel.
        assert!(!is_page_size(0));
        assert!(!is_page_size(-1));
        assert!(!is_page_size(3));
        assert!(!is_page_size(97));
        assert!(is_page_size(4));
        assert!(is_page_size(96));
    }

    #[test]
    fn a_zero_abandonment_window_is_refused() {
        // The regression this crate exists to make impossible: a cart abandoned at birth.
        assert!(!is_abandonment_hours(0));
        assert!(!is_abandonment_hours(-5));
        assert!(is_abandonment_hours(1));
        assert!(is_abandonment_hours(720));
    }

    #[test]
    fn a_zero_badge_threshold_is_refused() {
        // A threshold of zero badges everything in stock, which reads as no badge at all.
        assert!(!is_low_stock_badge_threshold(0));
        assert!(is_low_stock_badge_threshold(1));
        assert!(is_low_stock_badge_threshold(100));
        assert!(!is_low_stock_badge_threshold(101));
    }

    #[test]
    fn a_currency_code_is_shape_checked_not_list_checked() {
        assert!(is_currency_code("EUR"));
        assert!(is_currency_code("TRY"));
        // A country adopting a currency must not need a platform release.
        assert!(is_currency_code("ZWL"));
        assert!(!is_currency_code("eur"));
        assert!(!is_currency_code("EURO"));
        assert!(!is_currency_code("EU"));
    }

    #[test]
    fn a_blank_template_reference_falls_back_rather_than_storing_nothing() {
        assert_eq!(
            normalize_template(Some("   ")),
            DEFAULT_CONFIRMATION_TEMPLATE
        );
        assert_eq!(normalize_template(None), DEFAULT_CONFIRMATION_TEMPLATE);
        assert_eq!(normalize_template(Some("order_v2")), "order_v2");
    }

    /// The vocabulary and the migration are written twice, and this is the test that makes the
    /// duplication safe. A number added here and not to the SQL passes every other test in
    /// this file and is refused by the database on a customer's website.
    #[test]
    fn tests_agree_with_the_migration() {
        let sql = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../database/migrations/0169_storefront_settings.sql"
        ))
        .expect("the storefront settings migration is present in the tree");

        for value in TAX_DISPLAYS {
            assert!(
                sql.contains(&format!("'{value}'")),
                "tax display {value} is accepted here and absent from the migration",
            );
        }
        for value in PAGINATIONS {
            assert!(
                sql.contains(&format!("'{value}'")),
                "pagination {value} is accepted here and absent from the migration",
            );
        }
        assert!(
            sql.contains(&format!(
                "check (page_size between {MIN_PAGE_SIZE} and {MAX_PAGE_SIZE})"
            )),
            "the page-size bounds differ between the crate and the migration",
        );
        assert!(
            sql.contains(&format!(
                "check (per_order_item_max between {MIN_PER_ORDER_ITEM_MAX} \
                 and {MAX_PER_ORDER_ITEM_MAX})"
            )),
            "the per-order maximum bounds differ between the crate and the migration",
        );
        assert!(
            sql.contains(&format!(
                "check (low_stock_badge_threshold between {MIN_LOW_STOCK_BADGE_THRESHOLD} \
                 and {MAX_LOW_STOCK_BADGE_THRESHOLD})"
            )),
            "the low-stock bounds differ between the crate and the migration",
        );
        assert!(
            sql.contains(&format!(
                "check (abandonment_hours between {MIN_ABANDONMENT_HOURS} \
                 and {MAX_ABANDONMENT_HOURS})"
            )),
            "the abandonment bounds differ between the crate and the migration",
        );
        assert!(
            sql.contains(&format!("default '{DEFAULT_CONFIRMATION_TEMPLATE}'")),
            "the default confirmation template differs between the crate and the migration",
        );
    }
}
