-- 0188_system_health.sql — samples, settings, incidents and worker heartbeats (REQ-014, slice 1).
--
-- Until this migration the platform could answer "is the process alive?" and
-- nothing else. An operator's real question — *which of the seven things this
-- deployment depends on is unhappy right now* — had no answer anywhere in it,
-- which is why the request calls the existing screen "the simple health screen".
--
-- Five decisions carry this file, and each is a place the obvious shortcut is
-- wrong:
--
--   * **A service with no sample is still a row, so the table is the *only*
--     record of what was measured.** The overview is built from the registry and
--     the store fills in the rest; that works because an absent sample and an
--     absent service are different things and the schema keeps them apart. If
--     the table were seeded with a `healthy` row per service, a fresh database
--     would render seven green rows for a platform nothing has ever looked at.
--   * **`state` is constrained to the four words, in the database.** Not for
--     tidiness: the panel's colour map is a lookup keyed by state, and a typo
--     that reaches the table renders as `undefined` in a class attribute — a
--     badge with no colour and no label, on the one screen whose whole job is
--     to be read at a glance. The vocabulary is closed here so a fifth word
--     cannot be introduced by a migration that never saw the Rust enum.
--   * **`health_settings` is a SINGLETON (`id smallint check (id = 1)`).**
--     Thresholds are a property of the deployment, not of a tenant: a per-org
--     warn limit would let one tenant's editor decide what "critical disk" means
--     for everybody reading the same row. `backup_settings` and
--     `security_settings` reached the same conclusion independently.
--   * **`worker_heartbeats` is keyed by worker *name*, not by a row per tick.**
--     A heartbeat table that appends is a heartbeat table that grows without
--     bound and answers "is it alive" only after a scan; an upsert keyed by name
--     makes "Workers 4/4" a single index lookup and makes a stopped worker's
--     `last_seen_at` freeze at the moment it died, which is exactly the fact
--     the stale rule needs.
--   * **Retention prunes samples by AGE, and the incidents table is not
--     reachable from the pruner.** A pruner that deleted the newest N rows would
--     measure the table's recent rate, which is the number that changes when the
--     platform is busy — so the busiest day would be the day the window
--     silently shrank. And an operator's record of an outage is not sample
--     data: `health_incidents` has no `sampled_at`, so the sweep's WHERE clause
--     cannot reach it even if somebody later writes the wrong statement.

-- ---------------------------------------------------------------------------------------------
-- health_samples — one metric, one value, one moment.
-- ---------------------------------------------------------------------------------------------

create table health_samples (
    id         bigint      generated always as identity primary key,
    -- A service key, unconstrained on purpose: the registry is the source of
    -- truth and a retired service's history must stay readable (a 7-day roll-up
    -- has to be able to draw a service that no longer exists). The write path
    -- refuses an unregistered key; this table stores what the platform measured,
    -- not what it is allowed to measure today.
    service    text        not null,
    metric     text        not null,
    value      double precision not null,
    unit       text        not null default '',
    state      text        not null,
    detail     jsonb       not null default '{}'::jsonb,
    sampled_at timestamptz not null default now(),
    -- The four words, closed in the database. A fifth state is a migration and a
    -- panel change, never a typo in one writer.
    constraint health_samples_state_check
        check (state in ('healthy', 'degraded', 'down', 'unknown')),
    constraint health_samples_detail_object check (jsonb_typeof(detail) = 'object')
);

-- The chart's read: "every sample of this metric since T". Descending on
-- sampled_at because that is the direction the index is walked from for a
-- "latest" lookup, and because the 7-day roll-up groups by bucket.
create index health_samples_metric_idx
    on health_samples (service, metric, sampled_at desc);
-- The retention sweep's read. Separate from the metric index on purpose: pruning
-- asks about *age alone* and an index that starts with `service, metric` cannot
-- serve it without a full scan, which on a table that grows for months is the
-- difference between a sweep that runs in a second and one that locks the table.
create index health_samples_sampled_at_idx
    on health_samples (sampled_at);

-- ---------------------------------------------------------------------------------------------
-- health_settings — the singleton row of policy the health centre enforces on itself.
-- ---------------------------------------------------------------------------------------------

create table health_settings (
    id          smallint   primary key default 1,
    constraint health_settings_singleton check (id = 1),
    -- How often the runner probes. 5-600 s: below 5 the probes cost more than
    -- the thing they measure, and above 600 "within one interval" stops being a
    -- promise an operator can rely on. Slice 1 stores it; slice 3's form writes
    -- it, and the bounds are here so every writer inherits them.
    check_interval_seconds integer not null default 60
        check (check_interval_seconds between 5 and 600),
    -- After how long a silent worker counts as stale. 30-3600 s: shorter than 30
    -- and a worker on a slow GC pause is reported dead; longer than an hour and
    -- "4/4" is a claim about a worker that died before lunch.
    worker_stale_seconds integer not null default 120
        check (worker_stale_seconds between 30 and 3600),
    -- Per metric { warn, crit }. `{}` means "no opinion", which the panel renders
    -- as a card with no threshold marker rather than as a card that is fine —
    -- the request's own risk note asks for a first-run hint instead of
    -- hard-coded assumptions about what normal is on someone else's machine.
    thresholds  jsonb      not null default '{}'::jsonb,
    constraint health_settings_thresholds_object check (jsonb_typeof(thresholds) = 'object'),
    -- Which transitions notify. Toggles rather than rows: the notification centre
    -- decides the transport (REQ-021) and this request only decides what is
    -- worth saying, so the setting is a pair of booleans and not a channel list.
    notifications jsonb    not null default '{}'::jsonb,
    constraint health_settings_notifications_object
        check (jsonb_typeof(notifications) = 'object'),
    updated_by  uuid        references users (id) on delete set null,
    updated_at  timestamptz not null default now()
);

-- The row exists from the first migration. An empty table here would mean every
-- read handles "no row yet" and every write inserts-or-updates, for a table that
-- by definition holds exactly one row forever.
insert into health_settings (id) values (1);

-- ---------------------------------------------------------------------------------------------
-- health_incidents — the honest history, opened by a state transition (slice 3).
-- ---------------------------------------------------------------------------------------------
--
-- The table ships in slice 1 empty on purpose. Building the store and the screen
-- for it before anything can open a row would produce a screen that is only ever
-- right in its empty state, and an empty-state-only screen is the one this
-- project cannot accept. What slice 1 does guarantee is the schema the rest of
-- the request assumes: a partial unique index per service, so "one open incident
-- per service" is a database invariant and not a rule the transition detector
-- has to remember on a bad night.

create table health_incidents (
    id          uuid        primary key default gen_random_uuid(),
    service     text        not null,
    from_state  text        not null,
    to_state    text        not null,
    summary     text        not null default '',
    detail      jsonb       not null default '{}'::jsonb,
    started_at  timestamptz not null default now(),
    resolved_at timestamptz,
    -- A maintenance window suppresses the *incident*, never the state: the row
    -- the panel shows is still `degraded` while it is red, and this column is
    -- what says "we knew, and we were told not to page". A suppression that hid
    -- the state instead would turn "we are down on purpose" into "we are fine".
    suppressed  boolean     not null default false,
    acknowledged_by   uuid   references users (id) on delete set null,
    acknowledged_at   timestamptz,
    note        text,
    constraint health_incidents_from_state_check
        check (from_state in ('healthy', 'degraded', 'down', 'unknown')),
    constraint health_incidents_to_state_check
        check (to_state in ('healthy', 'degraded', 'down', 'unknown')),
    -- A run that is still open has no end; a resolved one always does. Without
    -- this, "duration" on the incidents table is a subtraction that silently
    -- reads as 0 for a row somebody forgot to close.
    constraint health_incidents_resolved_shape
        check ((resolved_at is null) = (to_state = 'healthy')),
    constraint health_incidents_acknowledged_shape
        check ((acknowledged_by is null) = (acknowledged_at is null))
);

create index health_incidents_started_at_idx on health_incidents (started_at desc);
-- "Open incidents" is the list the overview counts and the one an operator
-- filters to first. It is partial so the index holds only the rows that are
-- actually open: after a year most rows here are resolved, and an index over all
-- of them to answer "what is broken now" is an index that pays for history it
-- never returns.
create index health_incidents_open_idx
    on health_incidents (service) where resolved_at is null;

-- One open incident per service. A UNIQUE index rather than a constraint
-- because the predicate is a WHERE clause, and a suppressed incident is still
-- open as far as the state is concerned.
create unique index health_incidents_one_open_per_service
    on health_incidents (service) where resolved_at is null and not suppressed;

-- ---------------------------------------------------------------------------------------------
-- health_maintenance_windows — when a red row is expected (slice 4).
-- ---------------------------------------------------------------------------------------------

create table health_maintenance_windows (
    id         uuid        primary key default gen_random_uuid(),
    starts_at  timestamptz not null,
    ends_at    timestamptz not null,
    -- An EMPTY array means "every service": a window is a fact about a period of
    -- time, and the common case is a deploy that touches all of them. Storing a
    -- null for "all" would make every query a two-branch case.
    services   text[]      not null default '{}',
    note       text        not null default '',
    created_by uuid        references users (id) on delete set null,
    created_at timestamptz not null default now(),
    -- Ended before it started. In the database rather than only in the form,
    -- because the request names this as a rejection the create form must make
    -- *and* a background task will later read these rows: a validator that only
    -- the form calls is a validator the sweep never gets.
    constraint health_maintenance_windows_order check (ends_at > starts_at)
);

create index health_maintenance_windows_range_idx
    on health_maintenance_windows (starts_at, ends_at);

-- ---------------------------------------------------------------------------------------------
-- worker_heartbeats — the table that makes "Workers 4/4" a fact.
-- ---------------------------------------------------------------------------------------------

create table worker_heartbeats (
    id           text        primary key,
    kind         text        not null,
    host         text        not null,
    version      text        not null,
    state        text        not null default 'running',
    started_at   timestamptz not null default now(),
    last_seen_at timestamptz not null default now(),
    meta         jsonb       not null default '{}'::jsonb,
    constraint worker_heartbeats_state_check
        check (state in ('running', 'stopping', 'stopped', 'failed')),
    constraint worker_heartbeats_meta_object check (jsonb_typeof(meta) = 'object')
);

-- "Which kinds are alive, and which are not" is one grouped read, and grouping by
-- kind with a descending `last_seen_at` is the order the query returns in.
create index worker_heartbeats_kind_idx
    on worker_heartbeats (kind, last_seen_at desc);
