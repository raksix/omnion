-- 0190_health_incident_policy.sql — the two things slice 3 needs that 0188 did not have
-- (REQ-014, slice 3: incidents + thresholds).
--
-- `0188_system_health.sql` shipped the whole incident *table* with slice 1 on purpose: the
-- schema is cheap, and a slice that migrates later has to touch a table other worktrees are
-- reading. But it deliberately left two things out, and both absences are defects the moment
-- a threshold can be saved rather than being a constant in `registry.rs`:
--
--   * **A threshold pair has no shape.** `thresholds jsonb` defaults to `{}`, which the
--     migration's own comment calls "no opinion" — and `{}` is exactly what a *stored* empty
--     document and a *malformed* one look like. The first settings save that writes
--     `{"cpu_percent": {"warn": "eighty-five"}}` succeeds, and the screen then reads a string
--     where it expects a number and renders nothing at all. The closure below refuses a
--     non-object, a non-numeric warn/crit, a missing key inside a pair, a warn at or above the
--     crit, and a negative number — all in the database, so the form, the API and any future
--     sweep inherit the same rules. A validator only the form calls is a validator the
--     scheduled task never gets.
--
--   * **A breached metric could not be told apart from a quiet one.** The request says
--     `health.threshold.breached` fires "at most once per metric per window so a flapping disk
--     does not flood an endpoint". Deduplicating that in the emitter means holding it in
--     memory across the interval, and a process restart forgets it — so the first post-restart
--     run re-fires for a disk that has been over the line for an hour. The marker row below
--     makes the suppression a *fact about the deployment* rather than about a process: one
--     row per metric per breach window, cleared when the value comes back under the warn line.
--
-- The breach window is a **stated column, not a computed one**: "the current 15-minute
-- bucket" is a sentence whose answer depends on when you ask, and an emitter that computes the
-- bucket from `now()` will write the neighbouring bucket at the top of the hour. The writer
-- computes the window start once and stores it, so the unique index does the deduplication and
-- a concurrent pair of runs cannot both fire for the same window.
--
-- Neither table is reachable from the sample pruner: `health_samples` is the only table with a
-- `sampled_at`, and neither table here has one.

-- ---------------------------------------------------------------------------------------------
-- health_thresholds — the deployed warn/crit pairs, validated.
-- ---------------------------------------------------------------------------------------------

create table health_thresholds (
    -- The metric key, e.g. `disk_percent`. Primary key rather than a row per deployment: the
    -- thresholds describe the machine, and `health_settings.thresholds` keeps the same document
    -- as the settings screen's own payload. This table is the *validated, queryable* copy —
    -- what the breach emitter joins against — while the settings row stays a document so a
    -- save is one write rather than a diff of two rows per metric.
    metric    text        primary key,
    warn      double precision not null,
    crit      double precision not null,
    -- Which word the value has to cross for this pair to matter. Always `above` for the
    -- metrics in the request (disk %, memory %, CPU %, latency ms, queue depth) because every
    -- one of them is a "more is worse" measure. Stored rather than assumed so that a future
    -- direction does not have to change the table, and refused at write time so an unknown
    -- word cannot sit in a column no reader knows how to honour.
    direction text        not null default 'above',
    updated_by uuid        references users (id) on delete set null,
    updated_at timestamptz not null default now(),
    constraint health_thresholds_direction_check
        check (direction in ('above', 'below')),
    -- The pair the request names as a rejected input, enforced where a save cannot bypass it.
    constraint health_thresholds_pair_check check (warn < crit),
    -- A negative threshold would cross at zero and read as "everything is already breached",
    -- so both ends are bounded and finite. `NaN` compares false against everything, which is
    -- why `is_finite` is checked here rather than trusting `<`/`>` to do it.
    constraint health_thresholds_warn_finite check (warn >= 0 and warn < 1.0e15),
    constraint health_thresholds_crit_finite check (crit >= 0 and crit < 1.0e15),
    -- An unnamed metric is a row nothing can chart and nothing can breach, so it is refused.
    constraint health_thresholds_metric_named check (length(btrim(metric)) between 1 and 64)
);

comment on table health_thresholds is
    'Validated warn/critical pairs per metric. The settings row carries the same document; this is the queryable copy the breach emitter joins against.';

-- "Which metrics have a pair at all" is the settings form's own read order, and it is also the
-- overview's question when it has to render a card with no threshold marker.
create index health_thresholds_metric_idx on health_thresholds (metric);

-- ---------------------------------------------------------------------------------------------
-- health_breaches — one row per metric per breach window, so a flapping disk cannot flood.
-- ---------------------------------------------------------------------------------------------

create table health_breaches (
    -- The metric that breached.
    metric       text        not null,
    -- The *window* this breach belongs to, computed once by the writer. See the header: a
    -- computed bucket that disagrees with itself at the top of the hour is a duplicate event,
    -- not a measurement.
    window_start timestamptz not null,
    -- The value that crossed the line, and the limit it crossed — so the incident's history
    -- reads "95% against an 85% warn" without re-reading whatever the threshold is *now*.
    -- A threshold edited mid-incident must not rewrite the past.
    value        double precision not null,
    crit_limit   double precision not null,
    -- How many consecutive runs stayed above the line inside this window. `1` is the first
    -- crossing; the count is what makes a single flaky sample visibly different from a disk
    -- that stays full for an hour.
    observations int         not null default 1,
    first_seen_at timestamptz not null default now(),
    last_seen_at  timestamptz not null default now(),
    resolved_at  timestamptz,
    constraint health_breaches_metric_named check (length(btrim(metric)) between 1 and 64),
    constraint health_breaches_observations_positive check (observations >= 1),
    constraint health_breaches_value_finite check (value >= -1.0e15 and value <= 1.0e15),
    -- The dedup. One row per metric per window, enforced by the database so two runs racing at
    -- the window boundary cannot both decide they are the first to notice.
    constraint health_breaches_window_unique unique (metric, window_start)
);

comment on table health_breaches is
    'Breach dedup marker: one row per metric per window. Cleared when the value returns under the warn limit.';

-- "Is anything breaching right now, and since when" is the overview's read, and it is partial
-- so it holds only the open rows rather than every window the platform has ever been over.
create index health_breaches_open_idx on health_breaches (metric) where resolved_at is null;

-- A breach row for a metric with no thresholds cannot be honoured by anything, and it is the
-- exact shape a hand-written fixture produces — so it is refused rather than left to sit as a
-- claim the platform cannot check.
alter table health_breaches
    add constraint health_breaches_metric_known
    foreign key (metric) references health_thresholds (metric) on delete cascade;
