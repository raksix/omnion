-- Omnion · 0054 · cdn: the purge queue and its history (REQ-011, slice 2)
--
-- Slice 1 shipped the rule engine, the headers and the two tables the policy needs. This
-- slice ships the other half: what happens when a cache must be told that something it
-- holds is wrong. The high-water mark across every writer branch when this was written was
-- 0053, so this is 0054.
--
-- **The queue is a table, not a task in memory.** A purge is an act against a system outside
-- the platform, and an in-memory queue turns a restart into a silently dropped invalidation
-- — the cache keeps serving the old version and nothing anywhere records that it was
-- asked to change. Rows here are the durable half of the worker's decision, and `status` is
-- advanced by the database rather than held in a task's local variable, for the same reason.
--
-- **Items are rows, not a jsonb blob on the purge.** The provider answers per target: one
-- URL of forty can be refused while the rest go through, and that is the `partial` status
-- the request asks for. A single `targets text[]` on the parent cannot represent "twenty of
-- these worked", and a retry that cannot name what failed is a retry that either repeats
-- work that succeeded or skips work that did not. `cdn_purge_items` is therefore one row
-- per target, with its own attempt count, its own provider status and its own error.
--
-- **The claim is a conditional update, not a `select` then an `update`.** Two API processes
-- (a rolling deploy, a horizontal scale-out) will drain the queue at the same time; reading
-- a row and then claiming it is a read-modify-write gap wide enough for both to pick up the
-- same batch. `for update skip locked` inside a transaction makes the claim atomic, and the
-- `pending` → `running` transition is written in the same statement that reads the ids.
--
-- **`next_attempt_at` is the retry schedule and the backoff lives in the worker.** A
-- `attempt` column with no time would make "retry in a minute" mean "retry immediately,
-- a thousand times". The partial index below covers exactly the query the worker's claim
-- makes — pending rows whose time has come — which is why it is partial: the running and
-- finished rows are the majority of the table over time and are never claimable again.
--
-- **`requested_by` is `on delete set null`, not cascade.** A purge history row is a record
-- of what an operator did to a production cache; deleting the account that asked must not
-- delete the record that they asked. The column becomes `null` and the row keeps its own
-- `kind`, `targets`, counts and timestamps, which is the part anybody reads.
--
-- `status` on the parent is derived from the items but stored, because the history table
-- filters on it and a filter that has to aggregate the item table on every row is a filter
-- that gets slow exactly when the table is big enough to matter.

-- ---------------------------------------------------------------------------------------------
-- Purges
-- ---------------------------------------------------------------------------------------------

create table if not exists cdn_purges (
    id uuid primary key default gen_random_uuid(),
    -- A purge outlives the site it was for: the site is deleted, the record of invalidating
    -- its cache is not. `on delete set null` keeps the history readable and honest at once.
    site_id uuid references sites (id) on delete set null,
    kind text not null,
    -- Absolute paths for `url`, surrogate keys for `tag`, empty for `all`.
    targets text[] not null default '{}',
    -- queued | running | succeeded | partial | failed
    status text not null default 'queued',
    -- The adapter that ran (or will run) this purge, captured at request time.
    --
    -- Captured rather than read from the settings row later: a purge queued under one
    -- provider and drained under another is a purge whose history row describes a call
    -- nobody made, and the operator reading that row has no way to tell.
    provider text not null,
    item_count int not null default 0,
    failed_count int not null default 0,
    requested_by uuid references users (id) on delete set null,
    requested_at timestamptz not null default now(),
    started_at timestamptz,
    finished_at timestamptz,
    -- The provider's own message, kept verbatim. This is the string the panel shows next to
    -- a `failed` badge, so it must not be summarised into something generic.
    error text,
    constraint cdn_purges_kind_valid
        check (kind in ('url', 'tag', 'all')),
    constraint cdn_purges_status_valid
        check (status in ('queued', 'running', 'succeeded', 'partial', 'failed')),
    constraint cdn_purges_counts_non_negative
        check (item_count >= 0 and failed_count >= 0),
    -- An `all` purge invalidates the whole zone and needs no target list, so it is the one
    -- kind that may be empty. A `url` or `tag` purge with no targets is a purge that would
    -- report success having done nothing, which is the outcome a cache operator can least
    -- afford: it looks like the invalidation worked.
    constraint cdn_purges_all_needs_no_targets
        check (kind = 'all' or cardinality(targets) > 0)
);

-- The history table is read newest-first per site, and the filters are status and kind. The
-- composite index serves the common case (one site's recent history) and the status column
-- serves the failed-only filter the "retry failed" bulk action issues.
create index if not exists cdn_purges_site_requested
    on cdn_purges (site_id, requested_at desc);
create index if not exists cdn_purges_site_status_requested
    on cdn_purges (site_id, status, requested_at desc);

-- ---------------------------------------------------------------------------------------------
-- Purge items
-- ---------------------------------------------------------------------------------------------

create table if not exists cdn_purge_items (
    id bigint generated always as identity primary key,
    purge_id uuid not null references cdn_purges (id) on delete cascade,
    target text not null,
    -- pending | running | done | failed
    status text not null default 'pending',
    attempts int not null default 0,
    -- When this item may next be attempted. The worker backoff writes it; the claim reads
    -- it. A `queued` purge whose items are all in the future is a purge waiting for a
    -- provider, not a purge that failed.
    next_attempt_at timestamptz not null default now(),
    -- The HTTP status the provider returned for this target, when there was one.
    response_status int,
    -- The provider's message for this target, verbatim.
    error text,
    done_at timestamptz,
    constraint cdn_purge_items_status_valid
        check (status in ('pending', 'running', 'done', 'failed')),
    constraint cdn_purge_items_attempts_non_negative
        check (attempts >= 0)
);

-- The detail drawer reads one purge's items in the order they were listed. This is that
-- read; without the index it is a sequential scan of the whole queue per drawer open.
create index if not exists cdn_purge_items_purge_id
    on cdn_purge_items (purge_id, id);
create index if not exists cdn_purge_items_purge_status
    on cdn_purge_items (purge_id, status);

-- The worker's claim. Partial on `status = 'pending'` because finished items are the
-- majority of this table after the first few purges and can never be claimed again.
create index if not exists cdn_purge_items_due
    on cdn_purge_items (next_attempt_at, id)
    where status = 'pending';
