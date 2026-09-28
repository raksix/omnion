-- Omnion · 0044 · Alerts, silences, sampling and the settings row (REQ-126, slice 4)
-- (docs/requests/REQ-126-observability-stack.md).
--
-- Additive by design (docs/05-VERSIONING.md): three new tables and three columns added to the
-- existing single-row settings table. Nothing is rewritten, nothing is dropped, so a fresh
-- installation and a populated one reach the same schema and no row written by an earlier release
-- is touched.
--
-- ## Numbering note
--
-- The slot is global across the parallel waves. 0040 is this wave's own tracing migration, 0041
-- is held by wave 5 and 0042 by the media work on main, 0043 by wave 7 — so the alerts are 0044.
-- The name came from an `ls` of every sibling worktree and `origin/main`, re-run after the merge,
-- because two branches can each be internally consistent and still collide in their union, and
-- sqlx keys migrations on version AND checksum, so a collision makes the API refuse to boot at
-- all.
--
-- ## Why the sampling ratio lives in a column and not in a config file
--
-- The request says the settings screen covers "Sampling ratio (0.0–1.0), retention days per
-- signal within documented caps, per-module level overrides with an expiry, cardinality budget".
-- A config file would make every one of those a restart, and the request's own reason for having
-- the screen is "debugging does not need a redeploy". So the row is the source and the registry
-- is told what it is at boot and refreshed by `settings::apply`.
--
-- ## Why the alert evaluator coalesces into one row per rule per window
--
-- The risks section says it outright: "Alert thresholds must state their window and their
-- deduplication behaviour, or a flapping dependency floods every subscriber." The table has a
-- partial unique index on the open event of a rule, so a rule that flaps writes one `pending`
-- row, promotes it to `firing`, resolves it, and only a rule that fires again after resolving
-- starts a new one. The index is what makes that true in the database rather than in the code
-- that hopes it is the only writer.

create table if not exists obs_alert_rules (
    id                  uuid        primary key default gen_random_uuid(),
    name                text        not null unique,
    expr                text        not null,
    severity            text        not null default 'warning'
                                    check (severity in ('info', 'warning', 'critical')),
    for_seconds         int         not null default 300
                                    check (for_seconds between 0 and 86400),
    summary             text        not null default '',
    runbook_url         text,
    labels              jsonb       not null default '{}'::jsonb,
    -- `bundled` rules ship in `infra/observability/alerts.yml` and are seeded on boot; a
    -- `custom` rule was written in the panel. The distinction is what lets a bundled rule be
    -- re-seeded on upgrade without overwriting the edits an operator made to a custom one.
    source              text        not null default 'custom'
                                    check (source in ('bundled', 'custom')),
    checksum            text,
    enabled             boolean     not null default true,
    updated_by          uuid,
    updated_at          timestamptz not null default now()
);

comment on table obs_alert_rules is
    'Alert rules evaluated against the live metric registry. expr is a bounded selector in the '
    'project''s own subset of PromQL (see crates/telemetry/src/alerts.rs) rather than an arbitrary '
    'expression language: an alert rule is a query the panel must be able to explain and the '
    'evaluator must be able to run without a PromQL engine in the API process.';

create table if not exists obs_alert_events (
    id                  bigserial   primary key,
    rule_id             uuid        not null references obs_alert_rules (id) on delete cascade,
    state               text        not null check (state in ('pending', 'firing', 'resolved')),
    value               double precision,
    labels              jsonb       not null default '{}'::jsonb,
    started_at          timestamptz not null default now(),
    ended_at            timestamptz,
    notified            boolean     not null default false,
    -- The value that promoted this event from `pending` to `firing`, kept so the timeline can say
    -- "it crossed 5.2%" next to the 300-second window that made it fire.
    firing_value        double precision,
    fired_at            timestamptz,
    -- Why the event is in this state, in the operator's language: `threshold`, `dwell` or
    -- `silenced`. An alert whose reason is not recorded is an alert nobody can triage.
    reason              text        not null default 'threshold'
                                    check (reason in ('threshold', 'dwell', 'silenced')),
    context             jsonb       not null default '{}'::jsonb
);

create index if not exists obs_alert_events_rule_started_idx
    on obs_alert_events (rule_id, started_at desc);
create index if not exists obs_alert_events_state_started_idx
    on obs_alert_events (state, started_at desc);

-- One OPEN event per rule. A `pending` or `firing` event is open; `resolved` is not. This is the
-- flapping guard: the evaluator promotes the existing row instead of inserting a second one, and
-- the database refuses the insert if two evaluators ever race.
create unique index if not exists obs_alert_events_open_per_rule_uidx
    on obs_alert_events (rule_id)
    where state <> 'resolved';

create table if not exists obs_silences (
    id                  uuid        primary key default gen_random_uuid(),
    rule_id             uuid        references obs_alert_rules (id) on delete cascade,
    reason              text        not null,
    starts_at           timestamptz,
    ends_at             timestamptz not null,
    created_by          uuid,
    created_at          timestamptz not null default now()
);

create index if not exists obs_silences_ends_idx on obs_silences (ends_at);
create index if not exists obs_silences_rule_idx on obs_silences (rule_id);

-- The check the request asks the service layer to enforce is enforced here as well, because a
-- service-layer check is only as good as the number of writers and there is exactly one writer
-- today plus an operator with a psql prompt.
--
-- `ends_at > coalesce(starts_at, now())` cannot be a table check: `now()` is stable, not
-- immutable, so a check constraint may not call it. The equivalent that CAN be a check is the
-- ordering `ends_at > starts_at`; the "already in the past" half is checked by the service on
-- write, which is where a `400` can be returned to a human instead of a constraint violation.
--
-- A silence with no `starts_at` is "from now", and the row above compares only when `starts_at`
-- is present — a `NULL` comparison is not `false` in a check constraint, so an open-ended start
-- passes instead of being rejected.
alter table obs_silences
    add constraint obs_silences_ordering check (ends_at > starts_at);

-- ── the settings row grows the columns the request's settings screen names ─────────────────────
--
-- Every column added here has a writer in this same slice: the sampling ratio is read by the
-- tracing spine's sampler, the trace retention by the retention worker, the cardinality budget by
-- the registry, `prometheus_public` by the exposition route, and the level overrides by the
-- settings screen and the expiry sweep. A column added without its reader is the defect that
-- made four columns of REQ-125's audit depth structurally dead on arrival.
alter table obs_log_settings
    add column if not exists sampling_ratio       double precision not null default 0.1,
    add column if not exists traces_retention_days int         not null default 7,
    add column if not exists cardinality_budget   int          not null default 10000,
    add column if not exists prometheus_public    boolean      not null default false;

comment on column obs_log_settings.sampling_ratio is
    'The share of non-error requests whose trace is kept, 0.0 to 1.0. Errors are always sampled '
    'regardless (REQ-126 slice 3), so lowering this never hides a failure — it only makes the '
    'index cheaper.';
comment on column obs_log_settings.traces_retention_days is
    'How long obs_trace_index rows are kept. Capped by the retention worker at '
    'crates/telemetry::store::MAX_WINDOW_DAYS, the same cap the log store has, so no screen can '
    'ask for a window the database will not answer.';
comment on column obs_log_settings.cardinality_budget is
    'The registry-wide series cap. A value above the compiled-in default is refused: the cap is '
    'a memory bound, and a number typed into a settings screen cannot grow the process''s heap.';

-- The caps the request calls "documented caps", written where the screen can read them instead
-- of in a message string only.
create table if not exists obs_settings_caps (
    id                  smallint     primary key default 1 check (id = 1),
    logs_retention_max  int          not null default 30,
    traces_retention_max int         not null default 30,
    sampling_max        double precision not null default 1.0,
    cardinality_max     int          not null default 100000,
    cardinality_default int          not null default 10000,
    log_levels          text[]       not null default array['trace', 'debug', 'info', 'warn', 'error'],
    updated_at          timestamptz  not null default now()
);

insert into obs_settings_caps (id)
values (1)
on conflict (id) do nothing;

-- Reversal, in the order REQ-129's policy asks for: drop what this file created, nothing else.
--
--   drop table if exists obs_settings_caps;
--   alter table obs_log_settings
--       drop column if exists prometheus_public,
--       drop column if exists cardinality_budget,
--       drop column if exists traces_retention_days,
--       drop column if exists sampling_ratio;
--   drop table if exists obs_silences;
--   drop table if exists obs_alert_events;
--   drop table if exists obs_alert_rules;
--
-- The three settings columns come off last and only `if exists`, because they are the one thing
-- here that touches a table an earlier release created. Everything this file adds is dropped
-- whole.
