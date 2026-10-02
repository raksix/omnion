-- Omnion · 0035 · The bounded log store and its settings row (REQ-126, slice 1)
-- (docs/requests/REQ-126-observability-stack.md).
--
-- Additive by design (docs/05-VERSIONING.md): one new table, one new settings table, and no
-- change to an existing table. Nothing is rewritten, nothing is dropped, and a fresh installation
-- and a populated one reach the same schema.
--
-- ## Numbering note
--
-- The slot is global across the parallel waves and 0034 was already claimed by wave 4's CRM work
-- when this file was written, so the name came from an `ls` of every sibling worktree and
-- `origin/main` at write time rather than from the next free number on this branch alone. That
-- check has to be re-run after every merge, not only when a file is written: two branches can
-- each be internally consistent and still collide in their union, and sqlx keys migrations on
-- version AND checksum, so a collision makes the API refuse to boot at all.
--
-- ## Why `obs_log_entries` and not a `tracing` sink on stdout
--
-- The request asks for both: one JSON line per event on stdout *and* a bounded log explorer that
-- an operator can search. Stdout is the export path (slice 3 wires the exporter rows to it) and
-- this table is the read path. Keeping them separate is what makes the store bounded and
-- prunable, and it is why the acceptance criterion "a search by request id shows every line from
-- that request across API and workers" is answerable at all — stdout, by the time an operator
-- holds an id, has usually rotated.
--
-- ## What the columns are for, one at a time
--
-- The interesting decision is what is NULLABLE. Every identity column is nullable on purpose:
-- a line emitted at boot has no request, a line emitted by a runner has no user, and a line
-- emitted by the CLI has no organization. The only honest value for "this line was not part of a
-- request" on such a row is null, and a schema that forced a value would be forcing the writer
-- to invent one.
--
-- `route` holds a TEMPLATE (`/api/v1/secrets/{id}`), never a literal path. The request is
-- explicit about this and it is a cardinality rule as much as a privacy one: a literal id makes
-- every row its own label in the metric families, which is exactly the failure mode the request
-- calls the most dangerous one in this area.

create table if not exists obs_log_entries (
    id              bigint generated always as identity primary key,
    ts              timestamptz      not null,
    level           text             not null
                                 check (level in ('trace', 'debug', 'info', 'warn', 'error')),
    target          text             not null,
    message         text             not null,
    request_id      uuid,
    trace_id        text,
    span_id         text,
    user_id         uuid,
    organization_id uuid,
    route           text,
    method          text,
    status          int,
    duration_ms     int,
    source          text             not null default 'api'
                                 check (source in ('api', 'worker', 'cli')),
    host            text,
    version         text,
    -- Redacted at write time by the one shared pass (crates/telemetry::redact). The column holds
    -- the FILTERED object, not the caller's: a row can therefore be exported to an operator's own
    -- backend without a second filtering step that might disagree with this one.
    fields          jsonb            not null default '{}'::jsonb
);

comment on table obs_log_entries is
    'The bounded log store behind /observability/logs (REQ-126). Written with the redaction pass '
    'already applied, pruned by the retention job, and capped at the read side by the store''s '
    'window constant. Not a log platform: anything older than the retention window belongs to the '
    'operator''s own backend.';

comment on column obs_log_entries.ts is
    'When the line was emitted, not when it was written. A worker that ran behind its queue still '
    'has to be orderable.';

comment on column obs_log_entries.route is
    'A route TEMPLATE, never a literal path. A literal id would make every request its own metric '
    'label, which is the cardinality failure the request calls the most dangerous one here.';

comment on column obs_log_entries.fields is
    'Post-redaction only. A credential-shaped value or an e-mail address is replaced by its hint '
    'before the row is written, so a dump of this table cannot leak what the log path refused to '
    'print.';

-- The four indexes the explorer actually uses, and nothing more.
--
-- `(ts desc)` is the default ordering of every screen and of the retention prune. `(request_id)`
-- is the join that makes the store worth having: the caller holding an id from an error banner
-- must land on every line that request produced, across the API and the workers. `(level, ts
-- desc)` is the level filter the explorer opens with. `(trace_id)` backs the trace-to-log hop.
--
-- A `(source, ts desc)` index is deliberately absent: `source` is one of three values, so the
-- planner reads it from the `(ts desc)` index and filters, and a second index here would only add
-- write cost to the hottest table in the observability path.
create index if not exists obs_log_entries_ts_idx
    on obs_log_entries (ts desc);
create index if not exists obs_log_entries_request_idx
    on obs_log_entries (request_id)
    where request_id is not null;
create index if not exists obs_log_entries_trace_idx
    on obs_log_entries (trace_id)
    where trace_id is not null;
create index if not exists obs_log_entries_level_ts_idx
    on obs_log_entries (level, ts desc);

-- ── the single settings row ────────────────────────────────────────────────────────────────────
--
-- `id smallint primary key default 1 check (id = 1)` is the standard way to say "exactly one
-- row" in Postgres: a second insert is a constraint violation rather than a second competing
-- setting. The columns here are the ones slice 1 needs; the sampling, retention and level
-- columns the request's settings screen names are added by slice 4 with the behaviour that reads
-- them, so that no column is ever added without a writer — the defect that made four columns of
-- REQ-125's audit depth structurally dead on arrival.
create table if not exists obs_log_settings (
    id                        smallint primary key default 1 check (id = 1),
    log_level_default         text      not null default 'info'
                                      check (log_level_default in
                                             ('trace', 'debug', 'info', 'warn', 'error')),
    -- Per-module temporary raises: {"omnion_secrets::store": {"level": "debug",
    -- "expires_at": "2026-09-28T10:00:00Z"}}. Read by the settings screen and by the retention
    -- job; slice 4 adds the expiry sweep that returns a module to its default.
    log_level_overrides       jsonb     not null default '{}'::jsonb,
    logs_retention_days       int       not null default 14
                                      check (logs_retention_days between 1 and 30),
    updated_by                uuid,
    updated_at                timestamptz not null default now()
);

comment on table obs_log_settings is
    'The single-row settings table for the log store. logs_retention_days is capped at 30 to match '
    'the store''s MAX_WINDOW_DAYS, so a search can never reach further back than retention keeps: '
    'a window the store would refuse must not be one the database can answer.';

-- The seeded row. `on conflict do nothing` so re-applying is a no-op and a second writer's boot
-- cannot fail on it.
insert into obs_log_settings (id)
values (1)
on conflict (id) do nothing;

-- Reversal, in the order REQ-129's policy asks for: drop what this file created, nothing else.
--
--   drop table if exists obs_log_settings;
--   drop table if exists obs_log_entries;
--
-- Both are dropped with their indexes implicitly, and there is nothing else in this file to undo.
-- The down script exists as a comment rather than as a second file because the migration runner
-- reads a single `.sql` per version; REQ-129's screen parses this block and offers it as the
-- reversible action, which is why it is written as executable statements and not as prose.
