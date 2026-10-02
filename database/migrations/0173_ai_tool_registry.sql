-- REQ-100 · AI tool system & permission matrix — slice 1: the registry tables.
--
-- The tool registry is the platform's answer to "which actions may an AI take, and who said
-- so". Four tables carry that, and each one is shaped by a failure mode it is defending against.
--
-- 1. **`ai_tools` is the operator's copy of compiled code, and that is why it is a table at
--    all.** The catalogue in `crates/ai-hub::catalogue` is the source of truth; a row here is
--    that code plus the four decisions an operator owns — enabled, timeout_ms,
--    max_calls_per_run, requires_approval. The seeder upserts the code half and never writes the
--    decision half (see `registry::seed`): a boot must not be able to un-gate a tool somebody
--    deliberately gated, and a boot must not silently re-enable one somebody disabled.
--
--    `class`, `risk` and `permission` are COPIES of code values, denormalised onto the row. That
--    looks redundant next to the Rust `ToolSpec` and is not: the registry screen filters and
--    sorts on them across 20+ rows, and the permission matrix has to render a tool's required
--    permission next to a cell without joining through the compiled table at query time. The
--    request says exactly this — "class and risk are code values copied onto the row for
--    filtering" — and the copy is kept honest by a test that compares the row to the spec.
--
-- 2. **`ai_identities` folds `organization_id` for uniqueness, for the same PostgreSQL reason
--    `ai_skills` does.** `unique (organization_id, key)` does not fire for NULL rows, so every
--    organization could register a platform-level-shaped default. The folded index collapses
--    NULL to the nil uuid — a value the schema already refuses as a foreign key — and the
--    constraint becomes real. The same fold enforces "one default identity per organization",
--    which is what lets `agents` resolve an identity without the caller naming one.
--
-- 3. **`ai_tool_grants` has a foreign key on `tool_key` (not the uuid), deliberately.** The
--    request says "a tool removed from code keeps its row (data is not dropped)". A grant that
--    cascaded away with the code would make "never silently deletes grants" impossible to honour
--    at the database level, and the grant row is the record of a decision somebody made. The FK
--    keeps the grant pointing at a real, possibly-retired, tool.
--
--    `effect` is a boolean, not a nullable tri-state. The request's tri-state is
--    allow / deny / inherit, and **inherit is the absence of a row**: "an inherited cell writes
--    no grant row". A nullable `effect` would have to carry "inherit" in the database too, and
--    then every read has to remember to filter it — the same class of bug the NULL-means-built-in
--    convention exists to prevent. So the column is two-valued and the third state is row
--    absence.
--
-- 4. **`ai_tool_calls` is append-only and never cascades a run away.** `run_id` and `step_id`
--    are `on delete set null`, not cascade: a pruned run must not erase the evidence of what an
--    agent actually did, which is the one thing this table exists to answer. The usage counts on
--    `/ai/tools` aggregate exactly this table, and an aggregation that silently loses rows to a
--    retention sweep is a usage number that drifts downwards for no visible reason.
--
-- `status` is closed on purpose. `ok`, `denied`, `failed`, `timeout` and `limited` are the five
-- outcomes the execution pipeline in `tools.rs` can produce; adding a sixth later would mean an
-- unreadable historical chart, so the constraint fails loudly at migration time instead.
--
-- Retention: 180 days, pruned by the same runner tick as REQ-098's decision log. `audit_log`
-- is the permanent record and is never pruned.

create table if not exists ai_tools (
    id                 uuid primary key default gen_random_uuid(),
    key                text        not null,
    class              text        not null,
    permission         text        not null,
    risk               text        not null,
    description        text        not null default '',
    input_schema       jsonb       not null,
    example            jsonb,
    idempotent         boolean     not null default false,
    requires_approval  boolean     not null default false,
    enabled            boolean     not null default true,
    timeout_ms         integer     not null default 30000,
    max_calls_per_run  integer     not null default 20,
    -- Non-null once the tool has left the compiled catalogue. Retirement is a flag and a note,
    -- never a delete: dropping the row would cascade the `ai_tool_grants` decisions away, and
    -- the request is explicit that the registry never silently deletes grants.
    retired_note       text,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),

    constraint ai_tools_key_shape check (key ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$'),
    constraint ai_tools_class_known check (class in ('content', 'media', 'users', 'sites', 'themes', 'plugins', 'ops')),
    constraint ai_tools_risk_known check (risk in ('low', 'medium', 'high')),
    -- The ranges are the request's, and they are checks rather than UI hints because a limit
    -- outside them is a limit the execution path cannot enforce meaningfully: a 0 ms timeout
    -- kills every call, and a cap of 1000 turns a runaway loop into a cost incident.
    constraint ai_tools_timeout_range check (timeout_ms between 1000 and 300000),
    constraint ai_tools_cap_range check (max_calls_per_run between 1 and 200),
    constraint ai_tools_schema_is_object check (jsonb_typeof(input_schema) = 'object')
    -- There is deliberately NO `risk <> 'high' or requires_approval` check here, and its absence
    -- is the point. The screen spec asks for "a high-risk tool enabled without an approval gate
    -- renders a warning stripe on its row" — a panel that renders a stripe about a state the
    -- database refuses to store is a panel whose warning can never appear, and an operator who
    -- ungates `deployment.deploy` to unblock a run would hit a constraint instead of a clear
    -- message. The *seed* gates every high-risk tool (`catalogue::default_requires_approval`), and
    -- the ungated combination stays representable so the operator can see, and be warned about,
    -- exactly the configuration they built.
);

-- One row per tool: the compiled key is the identity.
create unique index ai_tools_key_unique on ai_tools (key);
-- The registry table's own ordering, and the class filter's index.
create index ai_tools_class_key_idx on ai_tools (class, key);
-- "which agents use this tool" for the disable confirmation.
create index ai_tools_enabled_idx on ai_tools (enabled) where enabled;

-- An identity is a named set of grants a run borrows. `is_default` is what lets an agent be
-- configured without naming one.
create table if not exists ai_identities (
    id              uuid primary key default gen_random_uuid(),
    -- NULL is the platform-level default, shared by every organization. See the header note.
    organization_id uuid        references organizations (id) on delete cascade,
    key             text        not null,
    name            text        not null,
    description     text        not null default '',
    is_default      boolean     not null default false,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint ai_identities_key_shape check (key ~ '^[a-z][a-z0-9_-]{0,63}$'),
    constraint ai_identities_name_len check (char_length(name) between 1 and 80),
    constraint ai_identities_description_len check (char_length(description) <= 500)
);

-- Decision (2). A plain unique (organization_id, key) does not fire for the NULL row.
create unique index ai_identities_org_key_unique
    on ai_identities (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), key);
-- Exactly one default per organization, platform-level included.
create unique index ai_identities_one_default
    on ai_identities (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid))
    where is_default;
create index ai_identities_org_idx on ai_identities (organization_id) where organization_id is not null;

-- The grant. `tool_key` references the tool's key, so a grant survives a tool leaving the
-- compiled set. See decision (3): inherit is the absence of a row, never a third value.
create table if not exists ai_tool_grants (
    id           uuid primary key default gen_random_uuid(),
    identity_id  uuid        not null references ai_identities (id) on delete cascade,
    tool_key     text        not null references ai_tools (key) on delete cascade,
    -- true = allow, false = an explicit deny, which beats any allow from any source.
    effect       boolean     not null,
    granted_by   uuid        references users (id) on delete set null,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),

    constraint ai_tool_grants_tool_key_shape check (tool_key ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$')
);

-- One decision per (identity, tool): a second row for the same pair is a race the client lost.
create unique index ai_tool_grants_pair_unique on ai_tool_grants (identity_id, tool_key);
-- "which identities grant this tool" and the matrix's per-tool column.
create index ai_tool_grants_tool_idx on ai_tool_grants (tool_key);

-- The call log. Append-only; the usage counts on /ai/tools are an aggregation of exactly this.
create table if not exists ai_tool_calls (
    id              bigserial primary key,
    organization_id uuid        references organizations (id) on delete set null,
    site_id         uuid,
    run_id          uuid        references ai_runs (id) on delete set null,
    step_id         uuid        references ai_run_steps (id) on delete set null,
    agent_id        uuid        references ai_agents (id) on delete set null,
    identity_id     uuid        references ai_identities (id) on delete set null,
    user_id         uuid        references users (id) on delete set null,
    tool_key        text        not null,
    status          text        not null,
    error_code      text,
    duration_ms     integer,
    args_bytes      integer,
    result_bytes    integer,
    created_at      timestamptz not null default now(),

    constraint ai_tool_calls_status_known check (status in ('ok', 'denied', 'failed', 'timeout', 'limited')),
    constraint ai_tool_calls_duration_sane check (duration_ms is null or duration_ms >= 0),
    constraint ai_tool_calls_args_bytes_sane check (args_bytes is null or args_bytes >= 0),
    constraint ai_tool_calls_result_bytes_sane check (result_bytes is null or result_bytes >= 0)
);

-- The registry list's "Calls 30 d", scoped to one organization.
create index ai_tool_calls_org_time_idx on ai_tool_calls (organization_id, created_at desc);
-- One tool's history, and the per-tool usage endpoint.
create index ai_tool_calls_tool_time_idx on ai_tool_calls (tool_key, created_at desc);
-- "Recent calls" on a tool's detail screen, joined to the run that made it.
create index ai_tool_calls_run_idx on ai_tool_calls (run_id, created_at);
-- The error-rate column: the aggregation that would otherwise be a sequential scan.
create index ai_tool_calls_status_time_idx on ai_tool_calls (status, created_at desc);
