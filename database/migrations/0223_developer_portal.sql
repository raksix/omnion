-- REQ-033 · internal developer platform (slice 1) — API keys, their usage, request logs and
-- scaffold generations.
--
-- Four tables. The safety property this migration is really about is stated on
-- `api_keys.secret_hash`: the column is a one-way hash, so a key that has been handed to a
-- developer cannot be read back out of the database, by this API or by anyone with a copy of
-- the dump. Every other table here is storage for decisions the crate makes.
--
-- Migration number 0223: the file numbering is a single shared namespace across the ten
-- worktrees of this repository, so the number is chosen above the union high-water (0222, taken
-- by w7) rather than above this branch's own last file (0217).

-- ─────────────────────────────────────────────────────────────────── api_keys

create table if not exists api_keys (
    id              uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    name            text not null,
    -- The public identifier a caller sends instead of the secret. Unique on its own so a
    -- lookup by prefix is an index probe rather than a scan, and short enough to paste.
    prefix          text not null,
    -- One-way. The plaintext exists exactly once, in the creation response, and the column
    -- can never produce it again. See `omnion_developer::secret`.
    secret_hash     text not null,
    scopes          jsonb not null default '[]'::jsonb,
    environment     text not null default 'sandbox',
    rate_tier       text not null default 'standard',
    -- Null means "any source address". An empty array is refused at the edge rather than
    -- silently meaning the same thing as null, because a UI that submits no rows is asking
    -- "no restriction" and a UI that submits one empty row is asking a question we cannot read.
    ip_allowlist    jsonb,
    expires_at      timestamptz,
    last_used_at    timestamptz,
    revoked_at      timestamptz,
    created_by      uuid not null references users (id) on delete cascade,
    created_at      timestamptz not null default now(),
    constraint api_keys_name_known
        check (length(btrim(name)) between 3 and 60),
    constraint api_keys_environment_known
        check (environment in ('live', 'sandbox')),
    constraint api_keys_rate_tier_known
        check (rate_tier in ('standard', 'high')),
    constraint api_keys_scopes_is_array
        check (jsonb_typeof(scopes) = 'array'),
    -- "Rotate" is the operation that breaks a caller's integration, so it is recorded as a
    -- distinct instant rather than inferred from a changed hash.
    rotated_at      timestamptz,
    constraint api_keys_revoke_and_rotate_are_exclusive
        check (revoked_at is null or rotated_at is null or rotated_at <= revoked_at)
);

create unique index if not exists api_keys_prefix_key
    on api_keys (prefix);
create unique index if not exists api_keys_org_name_key
    on api_keys (organization_id, name);
create index if not exists api_keys_org_revoked_idx
    on api_keys (organization_id, revoked_at);

-- ─────────────────────────────────────────────────────────── api_key_usage_daily

-- A rollup, not a log. `api_request_logs` below is the history; this is the shape a chart can
-- read without scanning fourteen days of rows, and the two agree because the writer updates
-- the same counters it reports.
create table if not exists api_key_usage_daily (
    api_key_id uuid not null references api_keys (id) on delete cascade,
    day        date not null,
    requests   integer not null default 0,
    errors     integer not null default 0,
    p95_ms     integer,
    primary key (api_key_id, day),
    constraint api_key_usage_daily_counts_sane
        check (requests >= 0 and errors >= 0 and errors <= requests),
    constraint api_key_usage_daily_p95_sane
        check (p95_ms is null or p95_ms >= 0)
);

-- ─────────────────────────────────────────────────────────────── api_request_logs

-- Deliberately narrow. The request's *metadata* only: a body is never written, so "why can't I
-- see the payload" is answered by the schema rather than by a redaction list that might one
-- day grow a hole. The risk note in the request says the same thing and means it.
create table if not exists api_request_logs (
    id              bigserial primary key,
    organization_id uuid not null references organizations (id) on delete cascade,
    api_key_id      uuid references api_keys (id) on delete set null,
    actor_user_id   uuid references users (id) on delete set null,
    method          text not null,
    path            text not null,
    status          smallint not null,
    duration_ms     integer not null,
    request_id      text not null,
    bytes_in        integer,
    bytes_out       integer,
    error_code      text,
    created_at      timestamptz not null default now(),
    constraint api_request_logs_status_known
        check (status between 100 and 599),
    constraint api_request_logs_duration_sane
        check (duration_ms >= 0),
    constraint api_request_logs_sizes_sane
        check ((bytes_in is null or bytes_in >= 0) and (bytes_out is null or bytes_out >= 0))
);

-- Three indexes, one per filter the screen offers, so no column of the table is unindexed and
-- none of the three queries degrades into the two others.
create index if not exists api_request_logs_org_created_idx
    on api_request_logs (organization_id, created_at desc);
create index if not exists api_request_logs_key_created_idx
    on api_request_logs (api_key_id, created_at desc);
create index if not exists api_request_logs_org_status_created_idx
    on api_request_logs (organization_id, status, created_at desc);

-- ────────────────────────────────────────────────────────────────── sdk_scaffolds

-- An audit of generations, not a code store. The archive itself is a media object; this row
-- says who generated what and when, so a scaffold cannot be generated unrecorded.
create table if not exists sdk_scaffolds (
    id              uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    kind            text not null,
    name            text not null,
    target          text not null,
    object_key      text not null,
    byte_size       bigint,
    created_by      uuid not null references users (id) on delete cascade,
    created_at      timestamptz not null default now(),
    constraint sdk_scaffolds_kind_known
        check (kind in ('plugin', 'theme', 'workflow')),
    constraint sdk_scaffolds_target_known
        check (target in ('live', 'sandbox')),
    constraint sdk_scaffolds_size_sane
        check (byte_size is null or byte_size >= 0),
    constraint sdk_scaffolds_name_known
        check (length(btrim(name)) between 1 and 60)
);

create index if not exists sdk_scaffolds_org_created_idx
    on sdk_scaffolds (organization_id, created_at desc);
