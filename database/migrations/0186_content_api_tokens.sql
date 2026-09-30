-- REQ-019 slice 1: content API tokens.
--
-- A token is a *published-content* credential, not an account credential. Two shapes of the same
-- question therefore need different answers, and the request text blurs them:
--   * the panel's own media CRUD surface (`/api/v1/media`) authenticates with a session and
--     carries write scopes, so a token accepted there would hand a read-only integration the
--     ability to delete files;
--   * the headless read surface (`/api/v1/content/*`) authenticates with `Authorization: Bearer
--     omn_<prefix>_<secret>` and only ever sees published revisions.
-- Hence a separate table rather than reusing `service_account_keys`: the columns that make a
-- content token safe (site scope, read scopes, origin allow-list) have no home on a machine
-- credential, and bolting them on would mean every panel API key silently gained a "read only"
-- flag that does nothing.
--
-- The plaintext secret exists exactly once, in the response to `POST /content-api/tokens`, and
-- never leaves the process: only its SHA-256 is stored, and `prefix` is what the panel shows.

-- Tokens are org-scoped, optionally site-scoped. `site_id is null` means every site the
-- organization owns -- NOT "every site on the box", which is the cross-tenant leak this table's
-- existence is meant to make impossible.
create table api_tokens (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations(id) on delete cascade,
    site_id uuid references sites(id) on delete cascade,
    name text not null check (char_length(name) between 1 and 64),
    -- The panel's copy button shows this and nothing else. The `omn_` marker is part of the
    -- display value, so it lives in the column rather than being prepended on every render.
    prefix text not null unique check (prefix ~ '^omn_[0-9a-f]{8}$'),
    token_hash text not null unique check (length(token_hash) = 64),
    scopes text[] not null check (cardinality(scopes) between 1 and 8),
    allowed_origins text[] not null default '{}',
    rate_limit_per_minute integer not null default 120
        check (rate_limit_per_minute between 10 and 600),
    expires_at timestamptz,
    revoked_at timestamptz,
    last_used_at timestamptz,
    created_by uuid references users(id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

-- Name uniqueness is per organization and case-insensitive, because "Prod" and "prod" in the
-- same list is a support ticket, not two tokens.
create unique index api_tokens_org_name_lower on api_tokens (organization_id, lower(name));
create index api_tokens_org_created on api_tokens (organization_id, created_at desc);
-- The lookup on every request is "prefix, not revoked", so the index is partial on purpose.
create index api_tokens_prefix_active on api_tokens (prefix) where revoked_at is null;

-- Daily metering. Counters are accumulated per request (Redis, slice 3) and flushed here, so the
-- read path never writes a row per request -- a content token is a high-volume credential and a
-- write per call would turn the usage view into a write amplifier.
create table api_token_usage_daily (
    token_id uuid not null references api_tokens(id) on delete cascade,
    day date not null,
    endpoint text not null,
    requests integer not null default 0,
    errors integer not null default 0,
    throttled integer not null default 0,
    primary key (token_id, day, endpoint)
);

create index api_token_usage_daily_day on api_token_usage_daily (day desc);
