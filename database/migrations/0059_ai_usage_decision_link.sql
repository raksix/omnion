-- REQ-098 · slice 4 — link the provider usage rows to the routing decision behind them.
--
-- The join column the request asks for is `ai_usage.decision_id`. Slice 3 added it behind a
-- `to_regclass('ai_usage')` guard, on the assumption that `ai_usage` was REQ-001's table. It is
-- not: the table this platform actually records a call in is `ai_provider_usage` (REQ-097's
-- health migration, 0032), so the guard was permanently false and the column was never added.
-- The conditional is kept, and the real table is added alongside it, because a guard that only
-- ever sees one table is a guard that was written against the wrong name.
--
-- Both alters are conditional for the same reason REQ-001's may not exist yet on some branches:
-- the first applier wins, and a race surfaces as a duplicate-column error, which is visible,
-- rather than as a silent divergence between two spellings of "the usage table".

do $$
begin
    if to_regclass('ai_provider_usage') is not null then
        if not exists (
            select 1 from information_schema.columns
            where table_name = 'ai_provider_usage' and column_name = 'decision_id'
        ) then
            alter table ai_provider_usage add column decision_id bigint
                references ai_route_decisions (id) on delete set null;
        end if;

        if not exists (
            select 1 from pg_indexes
            where tablename = 'ai_provider_usage' and indexname = 'ai_provider_usage_decision_idx'
        ) then
            create index ai_provider_usage_decision_idx
                on ai_provider_usage (decision_id);
        end if;
    end if;
end
$$;

-- The provider Usage tab groups by provider and window. A decision is a *routing* fact, so it
-- is deliberately not part of any existing index: the join that needs it is "every call this
-- decision produced", which is a lookup by decision id, and the index above answers that
-- directly. Widening the provider-recent index to carry a nullable column nobody filters on
-- would cost writes on the hot path of every AI call to serve a query nobody makes.
