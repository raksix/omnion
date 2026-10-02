-- The `graphql.*` settings row (REQ-130 slice 2).
--
-- **Number 0238, which is the UNION high-water across all ten worktrees plus one.** w7 holds 0237
-- (`0237_ai_tool_stats_daily.sql`), w8 0236. This branch's own tree stops at 0232, so numbering
-- from here would take 0233 — a number another writer may be writing right now. A migration number
-- is a shared namespace: two writers at one number is a merge that produces two files with the same
-- number, and `sqlx` then reports a `VersionMismatch` that kills every test in the workspace.
--
-- **Why a dedicated table and not a row in a generic settings store.** The request says these live
-- in "the platform settings store (REQ-112) under a `graphql.*` prefix", and REQ-112's store does
-- not exist yet. Writing them into an invented generic key-value table would create a second
-- settings mechanism that REQ-112 then has to merge, and — the reason that matters — the numbers in
-- this table are read on EVERY GraphQL request's refusal path, so they belong somewhere with real
-- column types and CHECK constraints rather than in JSON a handler has to validate every time.
--
-- **The caps are CONSTRAINTS, not handler rules.** `Settings::validate` produces a field-level
-- message naming the accepted range, which is what a settings screen needs; these constraints are
-- what make the range unrepresentable for every other writer. `max_depth` is additionally bounded by
-- the parser's own `PARSE_DEPTH_CEILING` (256) — a setting that promises a protection the parser
-- does not deliver is the failure mode the request warns about, so the row cannot hold it either.

create table graphql_settings (
    -- One row, always id = 1. A settings row that could be duplicated is a settings row two
    -- requests could read different answers from.
    id                  int primary key default 1 check (id = 1),
    max_depth           int not null default 10 check (max_depth >= 1 and max_depth <= 256),
    cost_budget         int not null default 1000 check (cost_budget >= 1),
    max_aliases         int not null default 15 check (max_aliases >= 1),
    max_fragments       int not null default 20 check (max_fragments >= 1),
    max_page_size       int not null default 100 check (max_page_size >= 1),
    -- Not zero-for-allowed: a sub-100 ms timeout refuses every query including `__typename`, which
    -- reads as an outage rather than a misconfiguration.
    timeout_ms          bigint not null default 10000 check (timeout_ms >= 100),
    -- Whether ad-hoc documents are refused. NOT a permission and NOT a capability: this is a
    -- statement about one environment, and a capability that removed the ability to turn it back
    -- off is a licence decision, not an operational one.
    persisted_only      boolean not null default false,
    -- Whether the authenticated playground is reachable. Separate from `persisted_only`: an
    -- installation may run allowlist-only with the playground open for authoring new documents.
    playground_enabled  boolean not null default true,
    updated_by          uuid references users (id) on delete set null,
    updated_at          timestamptz not null default now()
);

comment on table graphql_settings is
    'One row (id = 1) of GraphQL endpoint policy. The caps are column constraints so the range is unrepresentable, and max_depth is additionally bounded by the parser own nesting ceiling.';

-- The row itself. `insert … on conflict do nothing` rather than a plain insert: this runs inside
-- the same migration as the table, and on a database where a previous run of an equivalent schema
-- already seeded it, a plain insert would fail the whole migration.
insert into graphql_settings (id) values (1) on conflict (id) do nothing;
