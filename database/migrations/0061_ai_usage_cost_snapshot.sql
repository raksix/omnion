-- REQ-098 · slice 5 — the cost a call was *billed at*, stored at the moment of the call.
--
-- Slice 1's migration (0043_ai_model_prices.sql) promised this in prose and shipped nothing that
-- enforced it:
--
--   "Nothing here retro-edits history. `ai_provider_usage` stores token counts, not money; the
--    cost a request was billed at is derived from the price *at the moment of the call* and
--    stored, so changing a price today must never move a number written last month."
--
-- A usage row with only token counts cannot satisfy that: the costs screen would have to
-- re-derive every historical number from the *current* price, so an operator correcting a typo in
-- a price would silently restate last month's spend. The fix is the store, not the reader — the
-- four columns below are written once, inside the insert, from the model's own price at that
-- instant. Nothing in the platform ever updates them.
--
-- Four columns rather than one because a cost is a *formula*, not a number: persisting the sum
-- would make "what did the input side cost?" unanswerable, and an unexplained total cannot be
-- audited. The units follow 0043: micros of a currency per million tokens.

alter table ai_provider_usage
    add column cost_input_micros_per_mtok bigint,
    add column cost_output_micros_per_mtok bigint,
    add column cost_total_micros bigint,
    add column cost_calculated_at timestamptz;

-- The snapshot is a copy of `ai_models`' own constrained columns, so the same rules apply here:
-- a negative price would make a call *earn* money, and a total that disagrees with its own two
-- factors is a row no screen should render.
--
-- The check runs on each factor independently because a call whose endpoint reported no token
-- counts leaves every column null, and "unknown cost" must stay expressible. A zero is a real
-- cost (a free model) and is NOT the same as a null.
alter table ai_provider_usage
    add constraint ai_provider_usage_cost_non_negative_check check (
        (cost_input_micros_per_mtok is null or cost_input_micros_per_mtok >= 0)
        and (cost_output_micros_per_mtok is null or cost_output_micros_per_mtok >= 0)
        and (cost_total_micros is null or cost_total_micros >= 0)
    );

-- The cost screens group and sort by the total, and the panel's "In / Out cost" column on a
-- *historical* row reads the snapshot rather than the model — that is the whole point. Partial,
-- because the majority of rows (free models, or calls whose endpoint reported no usage) have no
-- cost at all, and an index over a mostly-null column is an index over nothing.
create index ai_provider_usage_cost_idx on ai_provider_usage (cost_total_micros desc, created_at desc)
    where cost_total_micros is not null;

-- The per-model rollup behind the catalog's cost column. Without it, "this model's spend last
-- month" is a full scan of the usage table for every row of a paginated catalog.
create index ai_provider_usage_model_cost_idx on ai_provider_usage (model_key, created_at desc)
    where model_key is not null and cost_total_micros is not null;
