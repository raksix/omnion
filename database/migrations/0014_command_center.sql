-- Omnion · 0014 · command centre: the palette's own memory (docs/requests/REQ-032)
--
-- Two facts the command centre keeps, and nothing else:
--
-- * **What this account did in the palette.** `command_recents` holds the searches and the
--   commands a person ran, per user (never per organization — the history is personal), newest
--   first, trimmed to fifty rows per account. The raw query text is stored because that is what
--   a "recent" is; it is capped, per-user and clearable, and it never leaves this table for an
--   export.
-- * **How often each command runs.** `command_usage_daily` counts runs per account, command and
--   day — the adoption numbers suggestions lean on later. It deliberately stores **no query
--   text**: adoption is a count, and a count that carries text is a transcript.
--
-- The command registry itself has no table (a command is code, not configuration — REQ-032
-- §Data model); this migration only gives the palette somewhere to remember.

create table command_recents (
    id              bigserial primary key,
    user_id         uuid        not null references users (id) on delete cascade,
    -- The account's organization when it has one; `null` at the platform level, exactly as
    -- `audit_log` files its own rows (an account that belongs to no organization still has a
    -- palette).
    organization_id uuid        references organizations (id) on delete cascade,
    kind            text        not null,
    -- The typed text of a search; `null` for a command.
    query           text,
    -- The registry id of a command; `null` for a search.
    command_id      text,
    -- How many results the search answered with, when the API knew; `null` for a command.
    result_count    integer,
    created_at      timestamptz not null default now(),

    constraint command_recents_kind_check check (kind in ('query', 'command')),
    constraint command_recents_query_format check (query is null or char_length(query) <= 200),
    constraint command_recents_shape_check check (
        (kind = 'query'   and query is not null and command_id is null) or
        (kind = 'command' and command_id is not null and query is null)
    ),
    constraint command_recents_command_format check (
        command_id is null or command_id ~ '^[a-z][a-z0-9_.-]{0,62}$'
    ),
    constraint command_recents_results_check check (result_count is null or result_count >= 0),

    -- Nullable columns never collide in a unique index, so the dedupe key is folded to a
    -- non-null pair: repeating a search or a command upserts the row it already has instead of
    -- stacking a second copy, which is what keeps "recent" a list of things rather than of
    -- keystrokes.
    query_key       text        not null generated always as (coalesce(query, '')) stored,
    command_key     text        not null generated always as (coalesce(command_id, '')) stored,

    constraint command_recents_dedupe_key unique (user_id, kind, query_key, command_key)
);

-- The palette reads "my newest rows" on every open; the trim below reads the same order.
create index command_recents_user_idx on command_recents (user_id, created_at desc, id desc);

create table command_usage_daily (
    user_id         uuid    not null references users (id) on delete cascade,
    organization_id uuid    references organizations (id) on delete cascade,
    command_id      text    not null,
    day             date    not null default current_date,
    runs            integer not null default 0,

    primary key (user_id, command_id, day),
    constraint command_usage_daily_runs_check check (runs >= 0)
);
