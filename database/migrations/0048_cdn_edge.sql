-- Omnion · 0048 · cdn: cache rules and the CDN settings row (REQ-011, slice 1)
--
-- The request names this migration `0011_cdn_edge.sql`. That number was taken long ago
-- (it is the events/webhooks migration) and the numbering is a shared namespace: picking
-- the number the request asks for is how two branches end up applying two different
-- migrations under one version, which `sqlx` then refuses to start on with
-- `VersionMismatch`. The high-water mark when this was written was 0047, so this is 0048.
--
-- Slice 1 is rules plus headers, and only the two tables that slice needs:
--
--   * **`cdn_settings` is one row per site, plus a platform row.** The request says
--     `site_id null` means the platform default. That shape needs a partial unique index
--     for the null row, because a plain `unique (site_id)` does not constrain NULLs in
--     PostgreSQL — two platform rows would both be legal and the second one to be written
--     would silently win. The per-site uniqueness and the platform uniqueness are therefore
--     two different indexes, not one constraint with a clever definition.
--   * **Credentials are write-only, in the database and in the API.** `credential_ciphertext`
--     is `bytea` and is never selected by a read path; the API's settings response reports
--     only whether a credential is *present*. A column that is nullable and never read is
--     how a key ends up in a JSON log line three layers up, so the intent is stated here.
--   * **Priority is a column, not a fractional number.** Rules are reordered by writing the
--     new order, and a `numeric` priority with gaps forever invites two rows to compare
--     equal after enough reorders. A plain `int` plus a rewrite-on-reorder keeps "lower
--     number wins" total and obvious.
--
-- The purge tables (`cdn_purges`, `cdn_purge_items`) are slice 2 and deliberately absent:
-- shipping a queue table with no worker behind it would put a status column on the panel
-- that can never move.

-- ---------------------------------------------------------------------------------------------
-- Settings
-- ---------------------------------------------------------------------------------------------

create table if not exists cdn_settings (
    id uuid primary key default gen_random_uuid(),
    -- One row per site; a null site is the installation-wide default that a site without its
    -- own row inherits. `on delete cascade` because a setting for a site that no longer
    -- exists is not a setting anybody can reach.
    site_id uuid references sites (id) on delete cascade,
    provider text not null default 'origin',
    endpoint_url text,
    zone_ref text,
    -- Write-only. The API never returns this column, never echoes it into an audit entry and
    -- never writes it into an event payload; the settings response carries a boolean instead.
    credential_ciphertext bytea,
    -- Trigger event -> enabled. The request fixes the six events; the column stays open
    -- (jsonb) so a future trigger is a new key, not a new migration.
    auto_purge jsonb not null default '{}'::jsonb,
    batch_size int not null default 100,
    max_attempts int not null default 5,
    updated_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cdn_settings_provider_known check (provider in ('origin', 'generic_http', 'cloudflare_style')),
    constraint cdn_settings_batch_size_range check (batch_size between 1 and 1000),
    constraint cdn_settings_max_attempts_range check (max_attempts between 1 and 10)
);

-- A site has at most one settings row. This index has to exclude the platform row, because
-- `site_id` is nullable and PostgreSQL's `unique` treats every NULL as distinct: without the
-- `where`, two platform rows are both accepted and the last write wins.
create unique index if not exists cdn_settings_site_unique
    on cdn_settings (site_id)
    where site_id is not null;

-- ...and the installation has at most one platform row. A partial index on `site_id is null`
-- is the only thing that actually constrains it.
create unique index if not exists cdn_settings_platform_unique
    on cdn_settings ((site_id is null))
    where site_id is null;

-- The settings page reads one site's row and the platform row together; this is the index
-- that keeps that a single lookup rather than a scan of a table that will never be large but
-- will be read on every request that builds cache headers.
create index if not exists cdn_settings_site_lookup on cdn_settings (site_id);

-- ---------------------------------------------------------------------------------------------
-- Cache rules
-- ---------------------------------------------------------------------------------------------

create table if not exists cdn_cache_rules (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    name text not null,
    -- Lower wins. The panel table is ordered by this and the matcher takes the first
    -- enabled rule that matches, so this column *is* the precedence.
    priority int not null default 0,
    -- A `*` glob inside one segment, `**` across segments. Validated in `omnion-cdn`
    -- (`PathPattern::parse`), so a stored pattern is always a pattern that can be matched.
    path_pattern text not null,
    methods text[] not null default '{GET,HEAD}',
    -- 0 means "do not store at the edge", which is a real setting and not a missing one.
    edge_ttl_seconds int not null default 300,
    browser_ttl_seconds int not null default 60,
    -- Stale-while-revalidate window; 0 disables it.
    swr_seconds int not null default 0,
    -- Which request parts form the cache key.
    cache_key jsonb not null default '{}'::jsonb,
    -- Cookie / query / header names that force a bypass.
    bypass jsonb not null default '{}'::jsonb,
    enabled boolean not null default true,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cdn_cache_rules_name_length check (char_length(name) between 1 and 64),
    constraint cdn_cache_rules_edge_ttl_range check (edge_ttl_seconds between 0 and 31536000),
    constraint cdn_cache_rules_browser_ttl_range check (browser_ttl_seconds between 0 and 31536000),
    constraint cdn_cache_rules_swr_range check (swr_seconds >= 0),
    constraint cdn_cache_rules_methods_not_empty check (cardinality(methods) > 0)
);

-- Rule names are unique per site, case-insensitively: two rules called "Blog" and "blog"
-- are the same name to a human reading the table, and the panel would show both.
create unique index if not exists cdn_cache_rules_site_name_unique
    on cdn_cache_rules (site_id, lower(name));

-- The matcher loads one site's rules in precedence order; this is the index that makes the
-- load a range scan instead of a sort.
create index if not exists cdn_cache_rules_site_priority
    on cdn_cache_rules (site_id, priority);

-- ---------------------------------------------------------------------------------------------
-- Freshness
-- ---------------------------------------------------------------------------------------------

-- Every table here is written through the API, which sets `updated_at` explicitly, but a
-- bulk fixup written by hand (or a future migration that touches many rows) should not
-- silently leave a stale timestamp behind. The function is shared by name with the other
-- migrations that use the same idiom.
create or replace function cdn_touch_updated_at() returns trigger
language plpgsql
as $$
begin
    new.updated_at = now();
    return new;
end;
$$;

drop trigger if exists cdn_settings_touch on cdn_settings;
create trigger cdn_settings_touch
    before update on cdn_settings
    for each row execute function cdn_touch_updated_at();

drop trigger if exists cdn_cache_rules_touch on cdn_cache_rules;
create trigger cdn_cache_rules_touch
    before update on cdn_cache_rules
    for each row execute function cdn_touch_updated_at();
