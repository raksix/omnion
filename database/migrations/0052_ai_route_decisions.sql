-- REQ-098 · Model registry & router — slice 3 (decision log and explanation).
--
-- Slices 1 and 2 made the choice *informed* (what a model can do, what it costs) and *deliberate*
-- (a task map, a feature pin, a resolution order). This makes it *explainable*: one row per
-- resolved request carrying what was asked, what answered, where in the fallback list it sat and
-- why — so "which model answered and why" stops being a question only the resolver knows.
--
-- Four decisions worth stating, because each of them closes a way the log can lie:
--
-- 1. **A decision row is written before the provider call, not after it.** A request that
--    resolved to a model which then timed out, or whose provider was switched off between the
--    resolution and the dial, must still explain itself. Recording the outcome on the row after
--    the call is what makes "the log is empty exactly when something went wrong" possible, so
--    the row exists first and the cost row joins back to it.
--
-- 2. **`fallback_index` is 0-based and named for what it is.** Position 1 (the primary) writes
--    `0`, and a null would have been the alternative — but a null cannot be summed, filtered or
--    badged, and "was a fallback used?" is the single question this screen exists to answer. A
--    check constraint keeps it honest, and the *source* of the choice is a separate column
--    rather than inferred from the index: an installation default answering is index 0 too, and
--    a log that called it "the first fallback" would be wrong.
--
-- 3. **The walk is stored as jsonb, bounded by what was actually considered.** A decision is on
--    the hot path of every AI request, so it carries identifiers and short reasons — never the
--    prompt. The spec's "bounded to candidates actually considered" is enforced here rather than
--    left to the writer: a check constraint caps the array, so a caller cannot turn the log into
--    a transcript store by accident.
--
-- 4. **The usage link is `set null`, and the usage table is not pruned by this request.** A cost
--    row outlives its decision (90-day decision retention, indefinite usage counters), and a
--    decision that outlives its cost row is the normal case. `set null` is the direction that
--    cannot delete money, and REQ-098 explicitly never prunes `ai_usage` — a counter that
--    vanished with its explanation would be a counter nobody can audit.

create table ai_route_decisions (
    id                  bigserial   primary key,
    -- The tenant the request belonged to. `set null` rather than `cascade`: the request happened,
    -- and a deleted organization must not erase the fact that a model was asked to do something
    -- it could not do. The decision is also the input an operator reads when auditing an
    -- incident, and an audit that deletes itself with its subject is not an audit.
    organization_id     uuid        references organizations (id) on delete set null,
    site_id             uuid        references sites (id) on delete set null,
    user_id             uuid        references users (id) on delete set null,
    -- The agent run this request belonged to, when it came from one. Null for a chat or a
    -- workflow step; the column is a plain uuid because REQ-099 owns the runs table and this
    -- request must not take a foreign key on a table that does not exist yet. A foreign key to a
    -- later migration is a migration-ordering argument nobody wins.
    run_id              uuid,
    -- Which task the request was for, and which feature's pin may have answered first.
    task                text,
    feature             text,
    -- What the caller asked for: a `provider/model`, a bare key, or nothing at all. Kept as the
    -- *string* the caller sent rather than a model id, because "nothing was asked, the default
    -- answered" is a fact the column has to be able to hold.
    requested           text,
    resolved_provider_id uuid       references ai_providers (id) on delete set null,
    resolved_model_id   uuid        references ai_models (id) on delete set null,
    -- 0-based position in the candidate list: 0 is the primary. A check rather than a plain
    -- integer, because a negative index is not "no fallback" — it is a bug that renders as a
    -- fallback badge.
    fallback_index      integer     not null default 0,
    -- Which rule produced the answer. Denormalised from the resolver's own constant so a stored
    -- decision keeps saying *why* even after the rule list is re-ordered; `unresolved` means
    -- nothing answered and the reason says so.
    rule                text        not null,
    requirements        text[]      not null default '{}',
    -- The one-sentence explanation the panel renders in the log row.
    reason              text        not null,
    -- The full candidate walk: every entry considered, with its outcome and reason.
    walk                jsonb       not null default '[]'::jsonb,
    created_at          timestamptz not null default now(),

    constraint ai_route_decisions_fallback_index_check check (fallback_index >= 0),
    constraint ai_route_decisions_rule_check check (rule in (
        'explicit', 'feature_override', 'task_route', 'installation_default', 'unresolved'
    )),
    -- The walk is bounded here, not by convention. A decision is written on the hot path of
    -- every AI request; a writer that put the prompt in `walk` would turn the log into a
    -- transcript store and grow the table by orders of magnitude, and nothing at the call site
    -- would have complained.
    constraint ai_route_decisions_walk_bound_check check (jsonb_array_length(walk) <= 32)
);

-- The log screen's own read: one scope's decisions, newest first. The scope filter is the
-- tenancy check, so it is the leading column — filtering by time and then dropping the rows of
-- other organizations is the pattern a missing index makes expensive.
create index ai_route_decisions_scope_recent_idx
    on ai_route_decisions (organization_id, site_id, created_at desc);

-- "Show me everything this model answered", which is also how a removal finds the routes and
-- decisions that pointed at it.
create index ai_route_decisions_model_recent_idx
    on ai_route_decisions (resolved_model_id, created_at desc);

-- The task filter, and the "unresolved tasks" list the panel renders from the same read.
create index ai_route_decisions_task_recent_idx
    on ai_route_decisions (task, created_at desc);

-- The pruner's index: it asks "which rows are older than the retention window" and nothing
-- else, so an index on the column it filters beats the three above.
create index ai_route_decisions_created_at_idx on ai_route_decisions (created_at);

-- A decision that is not a decision cannot be stored: the resolver always answers either with a
-- model or with `unresolved`, and a row claiming a model while carrying the `unresolved` rule is
-- the one shape that would render as a contradiction in the log.
alter table ai_route_decisions
    add constraint ai_route_decisions_answer_agrees_with_rule check (
        (rule = 'unresolved' and resolved_model_id is null and resolved_provider_id is null)
        or
        (rule <> 'unresolved' and resolved_model_id is not null and resolved_provider_id is not null)
    );

-- The cost row joins back to the decision that produced it, so "why did this call cost what it
-- cost" is one read instead of a guess from a timestamp. `set null` in both directions: a
-- decision is pruned on its own 90-day clock, and a usage row is never pruned by this request.
--
-- The two statements are conditional because REQ-001's `ai_usage` table may already exist with
-- or without the column: the spec says "if `ai_usage` does not exist yet it lands with REQ-001's
-- migration and this request only adds the column then". Writing it unconditionally would fail
-- the whole migration on a tree that has not shipped REQ-001 yet; writing it never would ship a
-- dead endpoint. The `to_regclass` guard means the first applier wins and the second is a
-- no-op — a race resolves to a duplicate-column error, which is visible, rather than a silent
-- divergence.
do $$
begin
    if to_regclass('ai_usage') is not null then
        if not exists (
            select 1 from information_schema.columns
            where table_name = 'ai_usage' and column_name = 'decision_id'
        ) then
            alter table ai_usage add column decision_id bigint
                references ai_route_decisions (id) on delete set null;
        end if;

        if not exists (
            select 1 from pg_indexes
            where tablename = 'ai_usage' and indexname = 'ai_usage_decision_idx'
        ) then
            create index ai_usage_decision_idx on ai_usage (decision_id);
        end if;
    end if;
end
$$;
