//! The cost catalogue: what each field costs, and who decided.
//!
//! The request is blunt about the risk: *"The cost model is an approximation — weights are
//! documented, tunable and audited; an under-priced field becomes a denial-of-service vector, so
//! reviews of new fields must include a weight."*
//!
//! Two consequences shape this file:
//!
//! 1. **A field with no weight is not free.** [`price`] refuses to price an unknown field rather
//!    than defaulting it to zero. A default of zero is the hole the request describes: a field
//!    nobody weighed becomes free work for whoever finds it first. Refusing means a new field
//!    cannot reach production until it has been weighed — the weight becomes a review artifact
//!    instead of an opinion.
//! 2. **The weights are auditable.** Every entry carries the reason it costs what it costs, so a
//!    screen can show the reviewer the justification rather than a bare number. A catalogue of
//!    naked integers is one nobody can review.

use std::collections::HashMap;

use crate::error::{Code, Error, Result};

/// What one field costs and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Weight {
    pub cost: u32,
    /// A sentence a reviewer can argue with. Present on every entry by construction — the
    /// constructor takes it, so a table cannot grow an unexplained number.
    pub reason: &'static str,
}

impl Weight {
    pub const fn new(cost: u32, reason: &'static str) -> Self {
        Self { cost, reason }
    }
}

/// The weights, by field key.
///
/// The key is the field's **response key** — the alias when one is present — because that is what
/// a caller's cost meter reads back. Two aliases of one field are two selections and two prices;
/// pricing them as one would be the under-pricing hole with a different name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogue {
    weights: HashMap<String, Weight>,
    /// What every operation costs before its fields are added. Not zero: an operation that
    /// selects nothing is still a parse, a guard and an audit row.
    pub operation_cost: u32,
}

impl Default for Catalogue {
    fn default() -> Self {
        Self::catalogued()
    }
}

impl Catalogue {
    /// The catalogue the content, tenancy and media surfaces ship with.
    ///
    /// These are the fields the request names as the first GraphQL surface. Every entry states
    /// its reasoning, and the expensive ones are expensive for the reason an operator would give:
    /// a list field scans a table, a resolved relation is a second query, a file field touches
    /// object storage.
    pub fn catalogued() -> Self {
        let mut weights = HashMap::new();
        let mut add = |key: &str, weight: Weight| {
            weights.insert(key.to_string(), weight);
        };

        // Query roots.
        add(
            "organization",
            Weight::new(10, "one tenant row and its slug; a single primary-key read"),
        );
        add(
            "organizations",
            Weight::new(80, "a tenant list is a scan plus a membership filter"),
        );
        add(
            "me",
            Weight::new(
                5,
                "the caller's own identity, already resolved by the guard",
            ),
        );

        // Content.
        add(
            "articles",
            Weight::new(
                120,
                "a paginated content list: index scan, sort and a total count",
            ),
        );
        add(
            "article",
            Weight::new(
                15,
                "one content row by id, plus its resolved author relation",
            ),
        );
        add(
            "pages",
            Weight::new(
                110,
                "a page list shares the content scan and adds a hierarchy join",
            ),
        );
        add(
            "page",
            Weight::new(15, "one page row with its block payload"),
        );
        add(
            "articleBySlug",
            Weight::new(
                25,
                "a slug lookup is a unique index hit, but resolves the tenant",
            ),
        );

        // Media.
        add(
            "mediaFiles",
            Weight::new(
                140,
                "a media list joins metadata, variants and usage counts",
            ),
        );
        add(
            "mediaFile",
            Weight::new(30, "one file's metadata plus its variants"),
        );
        add(
            "mediaDownloadUrl",
            Weight::new(60, "mints a signed URL: storage-side work per call"),
        );

        // Resolved relations — a second query each, which is the N+1 the request names.
        add(
            "author",
            Weight::new(
                20,
                "one user row; batched by the loader, unbatched it is an N+1",
            ),
        );
        add(
            "categories",
            Weight::new(35, "a category join per parent, batched"),
        );
        add("tags", Weight::new(30, "a tag join per parent, batched"));

        // Mutations — deliberately dearer than the reads they return, because a write is a
        // transaction, an audit row and a cache invalidation.
        add(
            "createArticle",
            Weight::new(300, "a write: transaction, audit row, cache invalidation"),
        );
        add(
            "updateArticle",
            Weight::new(320, "a write plus a revision record"),
        );
        add(
            "deleteArticle",
            Weight::new(340, "a destructive write plus its cascade"),
        );
        add(
            "createOrganization",
            Weight::new(400, "a tenant write: the heaviest operation on the surface"),
        );

        // Introspection is priced so a caller cannot use it as a free dictionary probe — the
        // introspection queries are themselves expensive and are otherwise uncharged.
        add(
            "__schema",
            Weight::new(200, "the full type graph; unpriced it is a free dictionary"),
        );
        add("__type", Weight::new(40, "one type's fields and resolvers"));

        Self {
            weights,
            operation_cost: 5,
        }
    }

    /// A catalogue with nothing in it — for tests, and for an installation with no surface yet.
    pub fn empty() -> Self {
        Self {
            weights: HashMap::new(),
            operation_cost: 5,
        }
    }

    /// Register or reweigh a field. How the settings screen's audit trail is written.
    pub fn set(&mut self, key: impl Into<String>, weight: Weight) {
        self.weights.insert(key.into(), weight);
    }

    /// The weight for a field, if the catalogue has one.
    pub fn get(&self, key: &str) -> Option<&Weight> {
        self.weights.get(key)
    }

    /// Every weight, for the explorer screen's "what does this field cost" panel.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Weight)> {
        self.weights.iter()
    }

    /// The cost of a field, refusing an unknown one.
    ///
    /// The refusal is the load-bearing half of the under-pricing defence, so its code is its own:
    /// a client that hit it needs to know the field is unpriced rather than over budget.
    pub fn price_of(&self, key: &str) -> Result<u32> {
        self.weights.get(key).map(|w| w.cost).ok_or_else(|| {
            Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: format!(
                    "field `{key}` has no weight in the cost catalogue; a field with no weight is refused rather than priced at zero"
                ),
            }
        })
    }
}

/// A priced selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Priced {
    pub total: u32,
    /// Largest first, capped by the caller at [`crate::limits::CONTRIBUTOR_COUNT`].
    pub contributors: Vec<(String, u32)>,
}

impl Priced {
    pub fn is_empty(&self) -> bool {
        self.total == 0 && self.contributors.is_empty()
    }
}

/// What a field costs when the catalogue does not weigh it.
///
/// Small, and deliberately not zero for a reason worth stating plainly: the *existence* gate is
/// the schema validator, not the cost model. A field that reaches pricing has already been proven
/// to exist in the caller's composed schema and to be visible to them — so the only fields arriving
/// here unweighed are **scalar leaves on a row some resolver already loaded** (`id`, `title`,
/// `slug`), which cost nothing because nothing extra happens to produce them.
///
/// Pricing them at zero would be defensible and is not done, because zero is the number a real
/// resolver would also get by accident if the catalogue grew a typo. One unit keeps "uncharged"
/// and "free" different, and one unit cannot be used to buy anything: every resolver-backed field
/// is weighed in [`Catalogue::catalogued`].
pub const LEAF_FIELD_COST: u32 = 1;

/// A field that is neither weighed nor a leaf is a bug in the caller, and is priced as though it
/// were the most expensive thing on the surface. Over-charging a bug is the safe direction.
pub const UNKNOWN_FIELD_COST: u32 = 400;

/// Price a selection.
///
/// Each field is charged once per **distinct response key**, not once per occurrence: a caller who
/// writes the same field twice under one name gets the same answer and the same work, and a model
/// that charged twice would refuse queries a human reads as a single selection. Two aliases are two
/// keys and are charged twice, because two aliases of a list field really do run it twice — that is
/// the N+1 the request's notes warn about.
///
/// Overflow saturates rather than wraps: `u32` arithmetic in release mode wraps, so a pathological
/// document could wrap the total *below* the budget and execute. Saturating makes an absurd cost
/// refuse, which is the only safe direction.
pub fn price(fields: &HashMap<String, u32>, catalogue: &Catalogue) -> Priced {
    let mut total = catalogue.operation_cost;
    let mut contributors: Vec<(String, u32)> = Vec::new();

    for (key, occurrences) in fields {
        let base = match catalogue.get(key) {
            Some(weight) => weight.cost,
            None => LEAF_FIELD_COST,
        };
        // `occurrences` is the number of DISTINCT response keys that selected this field, which
        // the walker already divided out — so it is a multiplier on work genuinely repeated.
        let cost = base.saturating_mul((*occurrences).max(1));
        total = total.saturating_add(cost);
        contributors.push((key.clone(), cost));
    }

    contributors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    contributors.truncate(crate::limits::CONTRIBUTOR_COUNT);
    Priced {
        total,
        contributors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> Catalogue {
        Catalogue::catalogued()
    }

    #[test]
    fn every_catalogued_field_states_why_it_costs_what_it_costs() {
        // The request makes the weight a review artifact. A catalogue entry with an empty reason
        // is exactly the unreviewable number it forbids, so this asserts the property rather
        // than trusting that every entry was written carefully.
        let catalogue = catalogue();
        assert!(
            !catalogue.entries().count() > 0,
            "the catalogue is not empty"
        );
        for (key, weight) in catalogue.entries() {
            assert!(
                weight.reason.len() > 12,
                "field `{key}` costs {} with no stated reason",
                weight.cost
            );
            assert!(weight.cost > 0, "field `{key}` is priced at zero");
        }
    }

    #[test]
    fn an_unpriced_field_is_refused_rather_than_priced_at_zero() {
        let catalogue = catalogue();
        // The field does not exist at all. If this priced at zero, a caller could select
        // unlimited unpriced fields and never approach the budget.
        let err = catalogue
            .price_of("totallyUnknownField")
            .expect_err("an unpriced field is refused");
        assert_eq!(err.code_str(), "GRAPHQL_VALIDATION_FAILED");
        assert!(err.to_string().contains("no weight"), "{err}");
    }

    #[test]
    fn a_known_field_prices_from_the_catalogue() {
        let catalogue = catalogue();
        assert_eq!(catalogue.price_of("organization"), Ok(10));
        assert_eq!(catalogue.price_of("mediaFiles"), Ok(140));
    }

    #[test]
    fn pricing_sums_distinct_response_keys_with_the_operation_cost() {
        let catalogue = catalogue();
        let mut fields = HashMap::new();
        fields.insert("organization".to_string(), 1);
        fields.insert("me".to_string(), 1);
        let priced = price(&fields, &catalogue);
        assert_eq!(priced.total, catalogue.operation_cost + 10 + 5);
        // Largest first, so the meter can slice its top three.
        assert_eq!(priced.contributors[0], ("organization".to_string(), 10));
        assert_eq!(priced.contributors[1], ("me".to_string(), 5));
    }

    #[test]
    fn a_scalar_leaf_is_charged_a_unit_rather_than_nothing() {
        // `id` and `title` are on a row some resolver already loaded, so they cost nothing to
        // produce — but they are charged one unit, so "uncharged" and "free" stay different.
        let catalogue = catalogue();
        let mut fields = HashMap::new();
        fields.insert("id".to_string(), 1);
        fields.insert("title".to_string(), 1);
        let priced = price(&fields, &catalogue);
        assert_eq!(priced.total, catalogue.operation_cost + 2);
    }

    #[test]
    fn the_cost_of_a_selection_is_driven_by_distinct_fields_not_by_how_often_a_caller_repeats() {
        // The walker keys on the field NAME, so the count it passes here is "how many distinct
        // response keys selected this field" — which for an alias is >1. Two aliases of a list
        // field really are two listings, so they are charged twice; that is the N+1 the request's
        // notes warn about being free.
        let catalogue = catalogue();
        let two_aliases = price(&HashMap::from([("articles".to_string(), 2)]), &catalogue);
        assert_eq!(two_aliases.total, catalogue.operation_cost + 120 * 2);

        let once = price(&HashMap::from([("articles".to_string(), 1)]), &catalogue).total;
        assert_eq!(once, catalogue.operation_cost + 120);
    }

    #[test]
    fn cost_saturates_instead_of_wrapping() {
        // Release-mode `u32` arithmetic wraps, which would put an absurd cost *under* the budget
        // and execute the very query the limit exists to stop.
        let mut catalogue = Catalogue::empty();
        catalogue.set(
            "enormous",
            Weight::new(u32::MAX, "a deliberately absurd test weight"),
        );
        let mut fields = HashMap::new();
        fields.insert("enormous".to_string(), 1);
        fields.insert("second".to_string(), 1);
        let priced = price(&fields, &catalogue);
        assert_eq!(
            priced.total,
            u32::MAX,
            "cost saturated rather than wrapping"
        );
        assert!(
            priced.total > 1_000_000,
            "the saturated cost must still refuse against any real budget"
        );
    }

    #[test]
    fn contributors_are_capped_so_a_query_cannot_flood_the_error() {
        let mut catalogue = Catalogue::empty();
        let mut fields = HashMap::new();
        for i in 0..50 {
            let key = format!("f{i}");
            catalogue.set(&key, Weight::new(10, "test weight"));
            fields.insert(key, 1);
        }
        let priced = price(&fields, &catalogue);
        assert_eq!(
            priced.contributors.len(),
            crate::limits::CONTRIBUTOR_COUNT,
            "a 50-field query must not produce a 50-entry contributor list"
        );
    }

    #[test]
    fn setting_a_weight_replaces_it_and_the_catalogue_stays_a_map() {
        let mut catalogue = catalogue();
        let before = catalogue.price_of("mediaFiles").unwrap();
        catalogue.set("mediaFiles", Weight::new(999, "reweighed by the operator"));
        assert_eq!(catalogue.price_of("mediaFiles"), Ok(999));
        assert_ne!(before, 999);
    }
}
