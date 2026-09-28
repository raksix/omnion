//! The shared vocabulary of the sales module: statuses, units and the settings row.
//!
//! These are the words the API, the screens and the migrations all have to agree on. They live in
//! one place so a status can be added once and be visible to every reader — a screen that
//! switches on a status string that no longer exists is a screen with a permanently dead branch.

use serde::{Deserialize, Serialize};

/// What a quote is in its lifecycle.
///
/// The order of the variants **is** the pipeline the overview counts, and
/// [`QuoteStatus::is_open`] is the definition of "open quote" every list uses — a second
/// definition of "open" is how a dashboard and a list end up disagreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteStatus {
    /// Being written. The only status a quote's lines may change in.
    Draft,
    /// Over the organization's discount threshold, waiting for a manager (REQ-059).
    PendingApproval,
    /// Approved and ready to send.
    Approved,
    /// Given to the customer; the version snapshot exists and the lines are frozen.
    Sent,
    /// The customer accepted it through the public link.
    Accepted,
    /// The customer declined it, with the reason they gave.
    Declined,
    /// Past `valid_until` without a decision.
    Expired,
    /// Withdrawn by the organization, with the reason.
    Cancelled,
}

impl QuoteStatus {
    /// The value stored in `sales_quotes.status`, which the migration's check constraint allows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Sent => "sent",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored status. An unknown value is `None` rather than a default, so a row written by
    /// a newer version is reported rather than silently shown as a draft.
    ///
    /// Named `parse` and not `from_str` on purpose: `from_str` is the standard trait method's
    /// name, and an inherent method of that name shadows it, so a caller that expected
    /// `FromStr` would silently get a different contract.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "draft" => Self::Draft,
            "pending_approval" => Self::PendingApproval,
            "approved" => Self::Approved,
            "sent" => Self::Sent,
            "accepted" => Self::Accepted,
            "declined" => Self::Declined,
            "expired" => Self::Expired,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// True while the quote is still being worked on and its lines may change.
    #[must_use]
    pub const fn is_editable(self) -> bool {
        matches!(self, Self::Draft | Self::PendingApproval | Self::Approved)
    }

    /// True while the quote counts as open work in the overview.
    ///
    /// Deliberately **not** "everything that is not closed": a draft the author abandoned three
    /// months ago is not open work, but a sent quote waiting for an answer is. The list screen
    /// and the overview both read this, which is what keeps the two numbers equal.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Draft | Self::PendingApproval | Self::Approved | Self::Sent
        )
    }

    /// True once the customer (or the organization) has decided, and the quote will not change.
    #[must_use]
    pub const fn is_decided(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Declined | Self::Expired | Self::Cancelled
        )
    }

    /// Every status, for the tab bar and the filter the API accepts.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::Draft,
            Self::PendingApproval,
            Self::Approved,
            Self::Sent,
            Self::Accepted,
            Self::Declined,
            Self::Expired,
            Self::Cancelled,
        ]
    }
}

impl std::fmt::Display for QuoteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an order is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Created but not confirmed; nothing is reserved.
    Draft,
    /// Confirmed: stock is reserved (or the reservation is explicitly `none` while REQ-053 is
    /// not installed, which the screen says out loud rather than hiding).
    Confirmed,
    /// An invoice draft exists and has been handed to accounting (REQ-054).
    Invoiced,
    /// Delivered and closed.
    Delivered,
    /// Cancelled; any reservation has been released.
    Cancelled,
}

impl OrderStatus {
    /// The value stored in `sales_orders.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Confirmed => "confirmed",
            Self::Invoiced => "invoiced",
            Self::Delivered => "delivered",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored status, or `None` for a value this build does not know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "draft" => Self::Draft,
            "confirmed" => Self::Confirmed,
            "invoiced" => Self::Invoiced,
            "delivered" => Self::Delivered,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// True when the order has been confirmed or later, which is when it holds stock.
    #[must_use]
    pub const fn holds_reservation(self) -> bool {
        matches!(self, Self::Confirmed | Self::Invoiced | Self::Delivered)
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A unit a product can be sold in.
///
/// The spec lists the five it ships with and allows a custom one; the enum covers the known set
/// and [`Unit::Custom`] carries the rest, so a product is never refused for naming its own unit
/// in a way the database cannot store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// A countable item.
    Piece,
    /// Billable time.
    Hour,
    /// Sold by weight.
    Kilogram,
    /// Sold by volume.
    Litre,
    /// Sold by the day.
    Day,
    /// A unit the organization named itself.
    Custom(String),
}

impl Unit {
    /// The value stored in the row.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Piece => "piece",
            Self::Hour => "hour",
            Self::Kilogram => "kilogram",
            Self::Litre => "litre",
            Self::Day => "day",
            Self::Custom(name) => name,
        }
    }

    /// Read a stored unit, falling back to a custom unit rather than refusing: a product written
    /// with a unit this build has never heard of is still a product somebody sells.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "piece" => Self::Piece,
            "hour" => Self::Hour,
            "kilogram" => Self::Kilogram,
            "litre" => Self::Litre,
            "day" => Self::Day,
            other => Self::Custom(other.to_string()),
        }
    }

    /// True for a unit measured in weight or volume, which is the set where three decimal places
    /// is the norm rather than the exception.
    #[must_use]
    pub const fn is_fractional(&self) -> bool {
        matches!(self, Self::Kilogram | Self::Litre)
    }

    /// The units the product form offers, in the order the spec lists them.
    #[must_use]
    pub const fn presets() -> [&'static str; 5] {
        ["piece", "hour", "kilogram", "litre", "day"]
    }
}

/// Whether a product or price list is live, and how a screen should read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Available to a new quote line.
    Active,
    /// Kept for history but not offered to a new line.
    Inactive,
}

/// The organization's sales settings — the row the quote builder reads for its defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The default currency for a new quote.
    pub currency: String,
    /// The largest line discount that can be sent without a manager's approval.
    pub discount_approval_threshold: i32,
    /// How many days a new quote is valid for.
    pub quote_validity_days: i32,
    /// The prefix of a quote number (`Q-2026-0001`).
    pub quote_number_prefix: String,
    /// The prefix of an order number.
    pub order_number_prefix: String,
}

impl Default for Settings {
    /// The migration's own defaults, so a caller that has no settings row still builds a quote
    /// the database would have accepted.
    fn default() -> Self {
        Self {
            currency: "TRY".to_string(),
            discount_approval_threshold: 15,
            quote_validity_days: 30,
            quote_number_prefix: "Q".to_string(),
            order_number_prefix: "SO".to_string(),
        }
    }
}

impl Settings {
    /// True when a quote whose largest line discount is `percent` needs a manager.
    ///
    /// The comparison is `>`, not `>=`: a quote at exactly the threshold is inside policy, and
    /// gating it would mean the number in the settings screen is not the number the builder
    /// enforces.
    #[must_use]
    pub fn needs_approval(&self, max_discount_percent: i32) -> bool {
        max_discount_percent > self.discount_approval_threshold
    }
}

/// The kind of CRM record a quote is addressed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomerKind {
    /// A company in REQ-051.
    Company,
    /// A person in REQ-051.
    Contact,
}

impl CustomerKind {
    /// The value stored in `customer_type`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::Contact => "contact",
        }
    }

    /// Read a stored value, or `None` for one this build does not know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "company" => Self::Company,
            "contact" => Self::Contact,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_survives_the_round_trip_through_the_database_value() {
        for status in QuoteStatus::all() {
            assert_eq!(
                QuoteStatus::parse(status.as_str()),
                Some(status),
                "{status}"
            );
        }
    }

    #[test]
    fn a_status_this_build_does_not_know_is_reported_rather_than_shown_as_a_draft() {
        // The dangerous default is `Draft`: a quote in a state this build cannot explain would
        // render as editable, and the first thing a person does with an editable document is edit it.
        assert_eq!(QuoteStatus::parse("quoted"), None);
        assert_eq!(QuoteStatus::parse(""), None);
    }

    #[test]
    fn only_a_working_quote_may_have_its_lines_changed() {
        for status in QuoteStatus::all() {
            let expected = matches!(
                status,
                QuoteStatus::Draft | QuoteStatus::PendingApproval | QuoteStatus::Approved
            );
            assert_eq!(status.is_editable(), expected, "{status}");
        }
    }

    #[test]
    fn a_sent_quote_is_open_and_a_declined_one_is_not() {
        // This is the definition the overview and the list both read, so it is pinned here.
        assert!(
            QuoteStatus::Sent.is_open(),
            "waiting for an answer is open work"
        );
        assert!(QuoteStatus::Draft.is_open());
        assert!(!QuoteStatus::Declined.is_open());
        assert!(!QuoteStatus::Expired.is_open());
        assert!(!QuoteStatus::Cancelled.is_open());
        assert!(QuoteStatus::Accepted.is_decided());
        assert!(!QuoteStatus::Sent.is_decided());
    }

    #[test]
    fn an_order_holds_stock_only_once_it_is_confirmed() {
        assert!(!OrderStatus::Draft.holds_reservation());
        assert!(OrderStatus::Confirmed.holds_reservation());
        assert!(OrderStatus::Invoiced.holds_reservation());
        assert!(!OrderStatus::Cancelled.holds_reservation());
    }

    #[test]
    fn a_unit_this_build_has_never_heard_of_is_still_a_unit() {
        let unit = Unit::parse("pallet");
        assert_eq!(unit, Unit::Custom("pallet".to_string()));
        assert_eq!(unit.as_str(), "pallet");
        assert_eq!(Unit::parse("piece"), Unit::Piece);
    }

    #[test]
    fn weight_and_volume_are_the_fractional_units() {
        // Three decimal places is normal for a kilo and odd for a piece, and the product form
        // reads this to choose its quantity step.
        assert!(Unit::Kilogram.is_fractional());
        assert!(Unit::Litre.is_fractional());
        assert!(!Unit::Piece.is_fractional());
        assert!(!Unit::Hour.is_fractional());
    }

    #[test]
    fn a_discount_at_exactly_the_threshold_is_inside_policy() {
        let settings = Settings::default();
        assert!(
            !settings.needs_approval(15),
            "the number in settings is the number enforced"
        );
        assert!(!settings.needs_approval(0));
        assert!(settings.needs_approval(16));
        assert!(settings.needs_approval(100));
    }

    #[test]
    fn the_default_settings_are_the_migrations_defaults() {
        // A caller with no settings row must build a quote the database would have accepted.
        let settings = Settings::default();
        assert_eq!(settings.currency, "TRY");
        assert_eq!(settings.discount_approval_threshold, 15);
        assert_eq!(settings.quote_validity_days, 30);
        assert_eq!(settings.quote_number_prefix, "Q");
    }
}
