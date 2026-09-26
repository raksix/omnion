-- Omnion · 0015 · analytics: collection, rollups and the privacy rules (docs/requests/REQ-007)
--
-- The platform's own analytics engine (REQ-007, slice 1). Three layers, each with one job:
--
-- * **Raw rows** — `analytics_visits`, `analytics_pageviews`, `analytics_events`. What one
--   batched beacon per page becomes. Raw rows are retention-bound: the purge job removes whole
--   days older than the site's `retention_days`.
-- * **Rollups** — `analytics_daily` and `analytics_hourly`. Rebuilt from the raw rows by the
--   rollup worker, idempotently: a bucket recomputed twice holds the same numbers, because a
--   bucket run deletes its own rows and writes the aggregate it just computed (never adds).
--   Dashboards read rollups; detail reports read raw rows inside the retention window.
-- * **Configuration** — `analytics_settings` (one row per site, seeded here for every existing
--   site), the goal tables (goals, their steps and their deduplicated hits) and `analytics_purges`
--   (the audit record every retention run or erasure leaves).
--
-- Privacy facts this schema encodes, so the code and the settings row cannot disagree:
--
-- * The visitor identifier is a **daily-salted hash** of (site, address, user agent, salt). The
--   salt of a day lives in `analytics_salts` and nothing else is kept: no cookie column, no
--   browser storage, no raw address. `analytics_pageviews` carries no identifier at all.
-- * `ip_prefix` is **null whenever `anonymize_ip` is on** — nothing is stored. With the switch
--   off the column holds at most the truncated prefix (IPv4 /24, IPv6 /48); a full address is
--   never written, whatever the setting.
-- * `dimension_value` for a total is the empty string, so "the site's total for this metric"
--   is one row rather than an aggregate the reader has to guess.
--
-- Time is UTC: a "day" here is the UTC calendar day. A per-site timezone arrives with the
-- per-site configuration request (REQ-113) and this table is the one that will carry it.

create table analytics_settings (
    site_id         uuid        primary key references sites (id) on delete cascade,
    tracking_enabled boolean    not null default true,
    -- `cookieless` is the default and the promise: the collector writes nothing to the browser.
    -- `cookie` exists for cross-day counting only, and only when a site explicitly chooses it.
    mode            text        not null default 'cookieless',
    anonymize_ip    boolean     not null default true,
    respect_dnt     boolean     not null default true,
    bot_filter      boolean     not null default true,
    sample_rate     integer     not null default 100,
    retention_days  integer     not null default 180,
    excluded_paths  text[]      not null default '{}',
    excluded_ips    cidr[]      not null default '{}',
    updated_by      uuid        references users (id) on delete set null,
    updated_at      timestamptz not null default now(),

    constraint analytics_settings_mode_check check (mode in ('cookieless', 'cookie')),
    constraint analytics_settings_sample_rate_check check (sample_rate between 1 and 100),
    constraint analytics_settings_retention_check check (retention_days between 7 and 1080)
);

-- Every site that exists right now gets its row in the same statement that creates the table:
-- a site without settings would make the collector guess, and guessing is how defaults drift.
insert into analytics_settings (site_id) select id from sites;

-- The salt of one day. One row per day, by design: yesterday's salt cannot reproduce today's
-- hash, so a visitor stops being one visitor at midnight — that is the privacy promise being
-- structural rather than a promise in a comment. Rows older than the raw data are pruned with it.
create table analytics_salts (
    day  date primary key,
    salt text not null,

    constraint analytics_salts_salt_format check (salt ~ '^[0-9a-f]{64}$')
);

create table analytics_visits (
    id              bigserial   primary key,
    site_id         uuid        not null references sites (id) on delete cascade,
    visitor_hash    text        not null,
    started_at      timestamptz not null,
    last_seen_at    timestamptz not null,
    pageview_count  integer     not null default 0,
    is_bounce       boolean     not null default true,
    entry_path      text,
    exit_path       text,
    referrer_host   text,
    referrer_path   text,
    source          text,
    medium          text,
    campaign        text,
    term            text,
    content         text,
    device_type     text,
    os              text,
    browser         text,
    screen_width    integer,
    screen_height   integer,
    language        text,
    country_code    char(2),
    -- Null whenever the site anonymizes addresses (the default). Never a full address.
    ip_prefix       inet,

    constraint analytics_visits_visitor_format check (visitor_hash ~ '^[0-9a-f]{64}$'),
    constraint analytics_visits_pageview_check check (pageview_count >= 0),
    constraint analytics_visits_device_check check (
        device_type is null or device_type in ('desktop', 'mobile', 'tablet', 'other')
    )
);

create index analytics_visits_site_started_idx on analytics_visits (site_id, started_at desc);
create index analytics_visits_visitor_idx on analytics_visits (site_id, visitor_hash, started_at desc);

create table analytics_pageviews (
    id           bigserial   primary key,
    site_id      uuid        not null references sites (id) on delete cascade,
    visit_id     bigint      not null references analytics_visits (id) on delete cascade,
    path         text        not null,
    title        text,
    occurred_at  timestamptz not null,
    duration_ms  integer,
    scroll_depth smallint,
    is_entry     boolean     not null default false,
    is_exit      boolean     not null default false,

    constraint analytics_pageviews_duration_check check (duration_ms is null or duration_ms >= 0),
    constraint analytics_pageviews_scroll_check check (
        scroll_depth is null or scroll_depth between 0 and 100
    )
);

create index analytics_pageviews_site_occurred_idx on analytics_pageviews (site_id, occurred_at desc);
create index analytics_pageviews_site_path_idx on analytics_pageviews (site_id, path, occurred_at desc);

create table analytics_events (
    id          bigserial      primary key,
    site_id     uuid           not null references sites (id) on delete cascade,
    visit_id    bigint         references analytics_visits (id) on delete cascade,
    name        text           not null,
    path        text,
    value       numeric(14, 2),
    properties  jsonb          not null default '{}'::jsonb,
    occurred_at timestamptz    not null,

    constraint analytics_events_name_format check (name ~ '^[a-z][a-z0-9_.-]{0,63}$')
);

create index analytics_events_site_name_idx on analytics_events (site_id, name, occurred_at desc);
create index analytics_events_site_path_idx on analytics_events (site_id, occurred_at desc);
create index analytics_events_properties_idx on analytics_events using gin (properties);
create index analytics_events_visit_idx on analytics_events (visit_id);

-- A rollup bucket. `metric` names what is counted (`pageviews`, `visitors`, `events`,
-- `downloads`, `forms`, `conversions`, `filtered`) and `dimension_kind` names how it is split
-- (`total`, `path`, `device`, `browser`, `os`, `country`, `language`, `referrer`, `name`,
-- `file`). `filtered` is the one bucket the rollup does not recompute: it counts beacons the
-- collector dropped (bots, privacy signals, exclusions, sampling) and is only ever incremented.
create table analytics_daily (
    site_id         uuid   not null references sites (id) on delete cascade,
    day             date   not null,
    metric          text   not null,
    dimension_kind  text   not null,
    dimension_value text   not null,
    count           bigint not null default 0,

    primary key (site_id, day, metric, dimension_kind, dimension_value),
    constraint analytics_daily_count_check check (count >= 0)
);

create index analytics_daily_site_day_idx on analytics_daily (site_id, day desc, metric);

create table analytics_hourly (
    site_id         uuid        not null references sites (id) on delete cascade,
    bucket          timestamptz not null,
    metric          text        not null,
    dimension_kind  text        not null,
    dimension_value text        not null,
    count           bigint      not null default 0,

    primary key (site_id, bucket, metric, dimension_kind, dimension_value),
    constraint analytics_hourly_count_check check (count >= 0)
);

create index analytics_hourly_site_bucket_idx on analytics_hourly (site_id, bucket desc, metric);

create table analytics_goals (
    id         uuid        primary key,
    site_id    uuid        not null references sites (id) on delete cascade,
    name       text        not null,
    kind       text        not null,
    match      jsonb       not null default '{}'::jsonb,
    enabled    boolean     not null default true,
    created_by uuid        references users (id) on delete set null,
    created_at timestamptz not null default now(),

    constraint analytics_goals_name_check check (char_length(name) between 1 and 120),
    constraint analytics_goals_kind_check check (
        kind in ('pageview', 'event', 'download', 'form_submit')
    ),
    constraint analytics_goals_site_name_key unique (site_id, name)
);

create table analytics_goal_steps (
    goal_id  uuid    not null references analytics_goals (id) on delete cascade,
    position integer not null,
    kind     text    not null,
    match    jsonb   not null,

    primary key (goal_id, position),
    constraint analytics_goal_steps_position_check check (position between 1 and 5),
    constraint analytics_goal_steps_kind_check check (
        kind in ('pageview', 'event', 'download', 'form_submit')
    )
);

-- One row per visitor per step: re-sending the same beacon cannot double-count a funnel step,
-- which is what makes a funnel readable at all.
create table analytics_goal_hits (
    id            bigserial   primary key,
    goal_id       uuid        not null references analytics_goals (id) on delete cascade,
    visitor_hash  text        not null,
    step_position integer     not null,
    occurred_at   timestamptz not null,

    constraint analytics_goal_hits_visitor_format check (visitor_hash ~ '^[0-9a-f]{64}$'),
    constraint analytics_goal_hits_step_check check (step_position between 1 and 5),
    constraint analytics_goal_hits_key unique (goal_id, visitor_hash, step_position)
);

create index analytics_goal_hits_goal_idx on analytics_goal_hits (goal_id, occurred_at desc);

-- The audit record of every data operation that removes rows: a retention purge or the erasure
-- of one visitor. `rows_removed` is what the run actually deleted, so "did we honour the
-- retention promise" is a question with an answer.
create table analytics_purges (
    id            uuid        primary key,
    site_id       uuid        references sites (id) on delete cascade,
    kind          text        not null,
    cutoff        timestamptz,
    rows_removed  bigint      not null default 0,
    actor_user_id uuid        references users (id) on delete set null,
    created_at    timestamptz not null default now(),

    constraint analytics_purges_kind_check check (kind in ('retention', 'erasure')),
    constraint analytics_purges_rows_check check (rows_removed >= 0)
);

create index analytics_purges_site_created_idx on analytics_purges (site_id, created_at desc);
