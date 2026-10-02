-- REQ-098 · Model registry & router — slice 1 (the catalog).
--
-- A model row in `ai_models` carried *whether* a model can do things but nothing about what it
-- costs. This adds the money: what the operator believes a million input and a million output
-- tokens cost, when that belief was written down, and how the platform should read the number.
--
-- Three deliberate choices, all of them about not lying to the operator:
--
-- 1. **The unit is micros of a currency per million tokens**, not per 1K. The accounting the
--    engine owns already sums in micros (so a whole-cent price never rounds to nothing), and the
--    panel may render it per 1K for reading; the column stays per million because that is the
--    number a vendor's price page quotes.
-- 2. **`price_source` and `price_updated_at` exist because the number drifts.** A price nobody
--    dated is a price nobody can trust, and the model screen says "estimate, entered 3 months
--    ago" rather than pretending a hand-typed figure is the vendor's current list price.
-- 3. **Nothing here retro-edits history.** `ai_provider_usage` stores token counts, not money;
--    the cost a request was billed at is derived from the price *at the moment of the call* and
--    stored, so changing a price today must never move a number written last month. That
--    invariant is the reason the columns are additive and separate rather than a single
--    recomputed figure.

alter table ai_models
    add column input_cost_micros_per_mtok bigint,
    add column output_cost_micros_per_mtok bigint,
    add column price_source text not null default 'manual',
    add column price_updated_at timestamptz,
    add column capabilities_verified_at timestamptz,
    add column capabilities_source text not null default 'manual';

-- A price is either a whole number of micros or nothing at all; a negative price would make
-- "spend nothing, earn money" a routing strategy, so the column refuses it rather than the API
-- layer having to remember.
alter table ai_models
    add constraint ai_models_price_non_negative_check check (
        (input_cost_micros_per_mtok is null or input_cost_micros_per_mtok >= 0)
        and (output_cost_micros_per_mtok is null or output_cost_micros_per_mtok >= 0)
    );

-- A price is a claim about where it came from, and the vocabulary is closed: `manual` is what an
-- operator typed, `discovery` is what an endpoint reported about itself, `probe` is what a live
-- test measured.
alter table ai_models
    add constraint ai_models_price_source_check check (
        price_source in ('manual', 'discovery', 'probe')
    );

-- The capability vocabulary is closed too, for the same reason: the panel's source badge reads
-- it, and a source nobody recognises would render as a blank cell.
alter table ai_models
    add constraint ai_models_capabilities_source_check check (
        capabilities_source in ('manual', 'discovery', 'probe')
    );

-- The catalog screen sorts and filters on the money columns, and the routing screen joins
-- candidate models by name; both are the sort the panel's "In / Out cost" column header issues.
create index ai_models_price_idx on ai_models (input_cost_micros_per_mtok, output_cost_micros_per_mtok)
    where input_cost_micros_per_mtok is not null or output_cost_micros_per_mtok is not null;

-- One partial index for "what is cheap enough to serve the cheap task", which is the question the
-- router asks most often and the question the catalog's cost column is read to answer.
create index ai_models_cheap_idx on ai_models (output_cost_micros_per_mtok, context_window desc)
    where enabled;
