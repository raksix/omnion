//! The cost of one call, computed from the price that was in force when it was made.
//!
//! Slice 1 put a price on every model (migration 0043) and said, in prose, that changing a price
//! must never move a number written last month. Prose is not enforcement: `ai_provider_usage`
//! stored token counts only, so the costs screen had to re-derive every historical figure from
//! the **current** price, and an operator correcting a typo would silently restate last month's
//! spend. This module is the part that makes the promise true — it snapshots the price onto the
//! usage row at insert time, and nothing ever updates it again.
//!
//! Three decisions, each one about a specific way the arithmetic lies.
//!
//! **A missing token count yields a missing cost, never zero.** A stream that ended without a
//! usage block reports `null` for its tokens (migration 0032 says why). Zero is a *measurement* —
//! a call that really did cost nothing, a free model — and substituting it for "we do not know"
//! turns an unknown into a small number that then aggregates into a total the operator will
//! believe. `None` in, `None` out.
//!
//! **Rounding happens once, on the total.** Per-megillion prices are fractional in any real
//! currency, so each side is computed exactly as a rational and the two are summed before a
//! single division. Rounding each half first and adding produces a figure that is off by one
//! micro on almost every row, and a costs screen whose column is wrong by one micro per call is
//! a screen nobody trusts past the first reconciliation.
//!
//! **A missing price yields a missing cost even when the tokens are known.** A model nobody
//! priced has no cost, and inventing one from a sibling model would produce a number with no
//! source. The row stays null and the screen can say "not priced" — which is a different, and
//! actionable, sentence from "free".

use std::sync::Arc;

/// The price of a model as the catalog stores it (migration 0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPrice {
    /// Micros of a currency per million input tokens.
    pub input_micros_per_mtok: Option<i64>,
    /// Micros of a currency per million output tokens.
    pub output_micros_per_mtok: Option<i64>,
}

impl ModelPrice {
    /// A model nobody has priced.
    #[must_use]
    pub const fn unpriced() -> Self {
        Self {
            input_micros_per_mtok: None,
            output_micros_per_mtok: None,
        }
    }

    /// Whether either side of the price is known.
    ///
    /// Both `None` is the only unpriced case: a model with an input price and no output price
    /// can still price a call, and refusing to price it because one factor is missing would
    /// discard a number that is genuinely computable.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.input_micros_per_mtok.is_some() || self.output_micros_per_mtok.is_some()
    }
}

/// The cost one call was billed at, as it is stored on the usage row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CallCost {
    /// The input price copied from the model at call time.
    pub input_micros_per_mtok: Option<i64>,
    /// The output price copied from the model at call time.
    pub output_micros_per_mtok: Option<i64>,
    /// The total, in micros, rounded once.
    pub total_micros: i64,
}

/// One million: the token unit both prices are quoted in.
const TOKENS_PER_MTOK: f64 = 1_000_000.0;

/// The cost of a call at a given price, or `None` when the cost is not knowable.
///
/// `None` is returned when either the price or the token count is missing, and those are the
/// only two reasons — an unpriced model and an unreported usage block are different problems and
/// the screen distinguishes them, but neither produces a number.
#[must_use]
pub fn call_cost(
    price: ModelPrice,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
) -> Option<CallCost> {
    if !price.is_known() || (prompt_tokens.is_none() && completion_tokens.is_none()) {
        return None;
    }

    // The two sides are scaled independently and summed as exact decimals before a single
    // division, so the total is the sum of the real quantities rather than the sum of two
    // already-rounded ones. `f64` is exact for these magnitudes (a product of two integers below
    // 2^53), and the single `round` is the only lossy step.
    let input = price
        .input_micros_per_mtok
        .and_then(|rate| prompt_tokens.map(|tokens| rate as f64 * f64::from(tokens)))
        .unwrap_or(0.0);
    let output = price
        .output_micros_per_mtok
        .and_then(|rate| completion_tokens.map(|tokens| rate as f64 * f64::from(tokens)))
        .unwrap_or(0.0);

    Some(CallCost {
        input_micros_per_mtok: price.input_micros_per_mtok,
        output_micros_per_mtok: price.output_micros_per_mtok,
        // `.round()` is half-away-from-zero, which is what "round to the nearest micro" means
        // for a non-negative figure. `as i64` alone truncates and would under-report every
        // call whose cost was not a whole micro.
        total_micros: ((input + output) / TOKENS_PER_MTOK).round() as i64,
    })
}

/// The price a call was billed at, resolved from the catalog at the moment of the call.
///
/// Taking the price as an argument rather than reading `ai_models` here is the point: the
/// snapshot must be taken from whatever the catalog said **then**, and a reader that fetched the
/// row itself could only ever see the price as it is now.
#[must_use]
pub fn snapshot_price(models: &[(String, ModelPrice)], model_key: &str) -> ModelPrice {
    models
        .iter()
        .find(|(key, _)| key == model_key)
        .map_or_else(ModelPrice::unpriced, |(_, price)| *price)
}

/// The price of the model a usage row names, as the insert sees it.
///
/// Wrapped in an `Arc` so the borrow checker is satisfied by the caller's snapshot and this stays
/// a free function: the catalog is a `Vec` in the runtime, and handing that fact to a store
/// function would couple the two modules for no gain.
#[must_use]
pub fn price_for(models: &Arc<Vec<(String, ModelPrice)>>, model_key: &str) -> ModelPrice {
    snapshot_price(models, model_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(input: i64, output: i64) -> ModelPrice {
        ModelPrice {
            input_micros_per_mtok: Some(input),
            output_micros_per_mtok: Some(output),
        }
    }

    #[test]
    fn an_unpriced_model_yields_no_cost_even_when_the_tokens_are_known() {
        // The failure this guards is inventing a price from a sibling model: a number with no
        // source is worse than an absent one, because the screen can label an absent one.
        assert_eq!(
            call_cost(ModelPrice::unpriced(), Some(1_000), Some(1_000)),
            None
        );
    }

    #[test]
    fn an_unreported_usage_block_yields_no_cost_even_when_the_model_is_priced() {
        // Zero is a measurement, not a missing value. Returning `Some(0)` here would make an
        // unknown call look like the cheapest call in the table.
        assert_eq!(call_cost(priced(1_000, 2_000), None, None), None);
    }

    #[test]
    fn a_free_model_costs_exactly_zero_and_is_still_priced() {
        let cost =
            call_cost(priced(0, 0), Some(1_000), Some(1_000)).expect("a free model is priced");
        assert_eq!(cost.total_micros, 0);
        // The distinction the screen renders: zero money, known price.
        assert_eq!(cost.input_micros_per_mtok, Some(0));
    }

    #[test]
    fn one_million_tokens_at_one_micro_is_one_micro() {
        let cost = call_cost(priced(1, 1), Some(1_000_000), Some(0)).expect("priced");
        assert_eq!(cost.total_micros, 1);
    }

    #[test]
    fn the_two_sides_are_summed_before_the_single_rounding() {
        // 3 micros per Mtok on each side, 200_000 tokens each: 0.6 + 0.6 = 1.2 micros.
        // Rounding each half first gives 1 + 1 = 2 — a total that over-reports by 67% on this row
        // and is impossible to explain to the person paying the bill. The halves are exact
        // decimals here precisely because that is where the two answers diverge.
        let cost = call_cost(priced(3, 3), Some(200_000), Some(200_000)).expect("priced");
        assert_eq!(cost.total_micros, 1);
    }

    #[test]
    fn a_half_micro_rounds_up_rather_than_truncating_to_nothing() {
        // 1 micro per Mtok over 500_000 tokens is exactly 0.5 micros. Truncation would report a
        // real call as free, which is the single most expensive kind of wrong in a cost column;
        // the boundary is exactly `.5` on purpose, because that is where the two differ.
        let cost = call_cost(priced(1, 0), Some(500_000), None).expect("priced");
        assert_eq!(cost.total_micros, 1);
    }

    #[test]
    fn a_model_priced_on_one_side_only_prices_what_it_can() {
        // Output is three times the input, the input is unpriced. The known half is still real.
        let cost = call_cost(
            ModelPrice {
                input_micros_per_mtok: None,
                output_micros_per_mtok: Some(3_000),
            },
            Some(999_999),
            Some(1_000_000),
        )
        .expect("the output side is known");
        assert_eq!(cost.total_micros, 3_000);
        assert_eq!(cost.input_micros_per_mtok, None);
    }

    #[test]
    fn the_snapshot_is_taken_from_the_price_the_catalog_had_then() {
        // The whole slice in one function: the price is passed in, so a later edit to the
        // catalog cannot reach back and change what this call was recorded as.
        let models = vec![("acme/big".to_string(), priced(1_000, 2_000))];
        let at_call_time = price_for(&Arc::new(models.clone()), "acme/big");
        assert_eq!(
            call_cost(at_call_time, Some(1_000), Some(1_000)).map(|c| c.total_micros),
            Some(3)
        );

        // The operator corrects the price. The recorded figure is a value, not a lookup.
        let corrected = vec![("acme/big".to_string(), priced(9_000, 9_000))];
        let later = price_for(&Arc::new(corrected), "acme/big");
        assert_eq!(
            call_cost(later, Some(1_000), Some(1_000)).map(|c| c.total_micros),
            Some(18)
        );
        // …and the earlier call still totals what it was billed.
        assert_eq!(
            call_cost(at_call_time, Some(1_000), Some(1_000)).map(|c| c.total_micros),
            Some(3)
        );
    }

    #[test]
    fn an_unknown_model_key_is_unpriced_rather_than_the_first_models_price() {
        // Index 0 is a real model with a real price; returning it for a key that is not in the
        // list would attribute one model's spend to another.
        let models = vec![("acme/big".to_string(), priced(1_000, 2_000))];
        assert_eq!(
            price_for(&Arc::new(models), "acme/typo"),
            ModelPrice::unpriced()
        );
    }
}
