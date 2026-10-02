-- REQ-035 slice 1 · edge regions, per-service health, and the seeded registry.
--
-- Number 0239: this repository's file numbering is ONE shared namespace across its ten
-- worktrees, so the number is taken above the union high-water (0238, held by wave 6), not
-- above this branch's own last file (0235). A number taken from a single branch is a number
-- two writers will choose again, and the collision is silent: both files exist, git merges
-- them, and the second `create table` of the same name either errors 42P07 or -- when the
-- shapes differ -- leaves the database on whichever shape landed first.
--
-- Nothing here is dropped or rewritten. `regions` describes infrastructure the deployment
-- already provides; it never creates any, which is the boundary the REQ draws ("provisioning
-- infrastructure: the registry describes regions the deployment already provides"). That is
-- also why the seed below is idempotent and conflict-tolerant rather than a bare insert.

-- The registry. One row per region the deployment already runs.
create table if not exists regions (
    code            text        primary key,
    display_name    text        not null,
    country_group   text        not null,
    status          text        not null default 'healthy',
    api_endpoint    text        not null,
    admin_endpoint  text,
    web_endpoint    text,
    storage_bucket  text        not null,
    cache_namespace text        not null,
    is_default      boolean     not null default false,
    is_active       boolean     not null default true,
    traffic_share   numeric(5,2),
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint regions_status_check
        check (status in ('healthy', 'degraded', 'down', 'maintenance')),
    -- A region with no country group cannot be grouped, and grouping is what a routing
    -- rule's `country` match resolves against. The check is the reason the routing editor
    -- can offer three groups instead of a free-text box.
    constraint regions_country_group_check
        check (country_group ~ '^[a-z]{2}$'),
    -- The three identifiers an operator copies somewhere. A blank one is a row that renders
    -- as a region and routes nothing, so the shape is refused at the database rather than
    -- discovered when a request 404s at the edge.
    -- Two lowercase letters, a dash, then a place name: a letter, up to 29 further
    -- `[a-z0-9-]` characters, and a *letter or digit* last.
    --
    -- The trailing character is pinned on purpose. A dash is a legal character in the middle
    -- of a place name (`us-virginia-2` is a real shape) but a code ending in one is a typo,
    -- and a typo here is a code that appears in a hostname, a routing rule and a storage
    -- namespace. The looser `[a-z0-9-]{1,30}` accepted it, and the Rust validator written
    -- beside it did not -- which is how a test found out that the panel would refuse a code
    -- the database was willing to store. One rule, written once, quoted in both places.
    constraint regions_code_format check (code ~ '^[a-z]{2}-[a-z][a-z0-9-]{0,29}[a-z0-9]$'),
    constraint regions_bucket_label check (length(btrim(storage_bucket)) > 0),
    constraint regions_cache_namespace check (length(btrim(cache_namespace)) > 0)
);

comment on table regions is
    'Regions the deployment already provides (REQ-035). Descriptive, never provisioned: an admin write can rename a region or change its status, but it cannot create the infrastructure behind the code.';

-- Exactly one default, at the DATABASE level.
--
-- A boolean column with a `where is_default` partial unique index says "at most one", and
-- "at most one" is not the requirement: the routing policy's `default_region_code` points at
-- a region, and a registry with no default has nothing to point at. The invariant is therefore
-- enforced on the flip -- setting a second default clears the first in the same transaction --
-- and the index is what makes a concurrent second flip impossible rather than merely unlikely.
create unique index if not exists regions_single_default_idx
    on regions (is_default)
    where is_default;

create index if not exists regions_status_idx on regions (status);
create index if not exists regions_country_group_idx on regions (country_group);

-- The per-service health history. Seven services per region, one row per service per minute.
create table if not exists region_health_checks (
    id          bigserial   primary key,
    region_code text        not null references regions (code) on delete cascade,
    service     text        not null,
    status      text        not null,
    latency_ms  integer,
    checked_at  timestamptz not null default now(),
    detail      jsonb       not null default '{}'::jsonb,

    -- The seven the panel's badge row renders, closed so a new service needs a migration
    -- rather than appearing as an eighth badge nothing else knows how to draw.
    constraint region_health_checks_service_check
        check (service in ('api', 'admin', 'web', 'worker', 'database', 'storage', 'cache')),
    -- `unknown` is separate from `down` and the distinction is the whole point: an
    -- unreachable control plane must render `Unknown`, never green. A check that no longer
    -- runs cannot distinguish "the service is broken" from "nobody is asking".
    constraint region_health_checks_status_check
        check (status in ('healthy', 'degraded', 'down', 'unknown')),
    -- A negative latency is not a slow service, it is a clock that went backwards, and
    -- stored as a positive number it would be drawn as the fastest region on the panel.
    constraint region_health_checks_latency_check
        check (latency_ms is null or latency_ms >= 0)
);

comment on table region_health_checks is
    'One row per (region, service, minute). Retention is 30 days; `unknown` rows are written when the checker cannot reach the service, so "no data" and "unreachable" stay distinguishable.';

create index if not exists region_health_checks_recent_idx
    on region_health_checks (region_code, service, checked_at desc);
create index if not exists region_health_checks_status_idx
    on region_health_checks (status, checked_at desc);
-- The minute bucket: a plain column, floored by the WRITER.
--
-- `date_trunc('minute', checked_at)` is **STABLE, not IMMUTABLE** -- it depends on the
-- session's time zone. So it cannot be an index expression (42P17, "functions in index
-- expression must be marked IMMUTABLE") and it cannot be a generated column (42P17 again,
-- "generation expression is not immutable"). Both were tried on a real PostgreSQL; this
-- sentence is the record of the two failures.
--
-- The workaround people reach for next -- `date_trunc('minute', checked_at at time zone
-- 'utc')` -- IS immutable, and it is the *wrong* one: it buckets by UTC while the value
-- being bucketed is a `timestamptz`, so the bucket stops being the minute the check
-- happened in and becomes the UTC minute. On a server in `Europe/Istanbul` the bucket is
-- still right, and on a server in `America/New_York` the de-bounce silently stops
-- deduplicating across the UTC hour boundary -- under exactly the multi-region deployment
-- this table exists to describe.
--
-- So the bucket is a column the writer fills: `omnion_regions::store::record_check` floors
-- in UTC and binds the SAME value it names in `on conflict`, so the insert and its conflict
-- target cannot disagree by construction. The DEFAULT is there for a hand-written insert or
-- a restore (a column DEFAULT may be STABLE, unlike an index expression), and it is
-- deliberately the same expression rather than `now()` un-floored, so a row that arrives
-- without a bucket is still bucketed.
alter table region_health_checks
    add column if not exists bucket_at timestamptz not null default (date_trunc('minute', now()));

comment on column region_health_checks.bucket_at is
    'The minute this check belongs to, floored in UTC by the writer and named by the same insert that sets on conflict. A column, not an expression index and not a generated column: date_trunc on timestamptz is STABLE, and PostgreSQL refuses both.';

-- The one-row-per-minute guarantee the de-bounce depends on, so two schedulers racing on the
-- same bucket update rather than append a neighbour that draws twice the variance.
create unique index if not exists region_health_checks_bucket_idx
    on region_health_checks (region_code, service, bucket_at);

-- Region-to-region p95 latency, for the matrix and the routing defaults.
--
-- Separate from `region_health_checks` on purpose: a health row is "is it up", a latency
-- sample is "how far away is it", and the two have different writers (the checker vs. the
-- probe) and different retention (30 days vs. 15). Collapsing them would mean either a
-- service that is up but far away cannot be recorded, or the checker starts writing probes.
create table if not exists region_latency_samples (
    id             bigserial   primary key,
    from_region    text        not null references regions (code) on delete cascade,
    to_region      text        not null references regions (code) on delete cascade,
    p95_ms         integer     not null,
    sample_count   integer     not null default 0,
    measured_at    timestamptz not null default now(),

    constraint region_latency_samples_p95_check check (p95_ms >= 0),
    constraint region_latency_samples_count_check check (sample_count >= 0),
    -- A region is not zero milliseconds from itself; the matrix renders this cell as
    -- "local" rather than as the fastest region on the panel.
    constraint region_latency_samples_distinct_check check (from_region <> to_region)
);

-- The hour bucket, for the same reason and by the same means as the health table's: a
-- column the writer floors, because `date_trunc` is STABLE and cannot index or generate.
alter table region_latency_samples
    add column if not exists bucket_at timestamptz not null default (date_trunc('hour', now()));

create unique index if not exists region_latency_samples_bucket_idx
    on region_latency_samples (from_region, to_region, bucket_at);

-- The seeded registry: the three regions the brief names, in the order an operator reads
-- them (home country first, then the two international ones).
--
-- Every column is written explicitly even where a default would do, because a seed that
-- relies on a default is a seed that changes meaning when the default changes. `tr-ankara`
-- carries the default flag; the two `on conflict (code) do nothing` tails mean re-running
-- this file -- or a boot that seeds on every start -- cannot duplicate a region or steal the
-- default flag from the region an operator promoted.
--
-- The endpoints are placeholders in the sense that they are host names the deployment is
-- expected to serve: the REQ says the registry *describes* regions, and a row whose endpoint
-- is empty would be a region that routes nowhere.
insert into regions (code, display_name, country_group, status, api_endpoint, admin_endpoint,
                     web_endpoint, storage_bucket, cache_namespace, is_default, is_active,
                     traffic_share)
values
    ('tr-ankara',    'Turkey · Ankara',     'tr', 'healthy', 'api.tr-ankara.omnion.test',
     'admin.tr-ankara.omnion.test', 'www.tr-ankara.omnion.test',
     'omnion-tr-ankara',    'tr-ankara',    true,  true, 100.00),
    ('eu-frankfurt', 'Europe · Frankfurt',  'eu', 'healthy', 'api.eu-frankfurt.omnion.test',
     'admin.eu-frankfurt.omnion.test', 'www.eu-frankfurt.omnion.test',
     'omnion-eu-frankfurt', 'eu-frankfurt', false, true, 0.00),
    ('us-virginia',  'US · Virginia',       'us', 'healthy', 'api.us-virginia.omnion.test',
     'admin.us-virginia.omnion.test', 'www.us-virginia.omnion.test',
     'omnion-us-virginia',  'us-virginia',  false, true, 0.00)
on conflict (code) do nothing;

-- A deployment that ships a single region still needs a default, and the routing surfaces
-- slice 3 will build point at one. This is the only statement above that is not
-- `if not exists`-shaped: it is a repair, not a definition, and it runs only when the
-- registry has regions but no default -- i.e. exactly the state that would leave the
-- routing policy with nothing to point at.
update regions
set is_default = true, updated_at = now()
where not exists (select 1 from regions where is_default)
  and code = (select min(code) from regions);
