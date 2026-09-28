-- Omnion · 0037 · The metric catalogue (REQ-126, slice 2)
-- (docs/requests/REQ-126-observability-stack.md).
--
-- Additive by design (docs/05-VERSIONING.md): one new table, and no change to any existing
-- table. Nothing is rewritten, nothing is dropped, so a fresh installation and a populated one
-- reach the same schema and no row written by an earlier release is touched.
--
-- ## Numbering note
--
-- The slot is global across the parallel waves. 0034 was claimed by wave 4's CRM work, 0035 by
-- this wave's own log store, and 0036 by the main writer's media work, so the catalogue is 0037.
-- The name came from an `ls` of every sibling worktree and `origin/main`, and that check has to be
-- re-run after every merge: two branches can each be internally consistent and still collide in
-- their union, and sqlx keys migrations on version AND checksum, so a collision makes the API
-- refuse to boot at all.
--
-- ## Why the catalogue is a TABLE and not the registry read straight from Rust
--
-- The registry (`crates/telemetry::metrics::FAMILIES`) is the declaration. The panel could import
-- it — it is a compiled-in list either way — and this table would be decoration. It is not
-- decoration, for three reasons the request names:
--
-- 1. **A module's own families are not ours.** The request says a family's source is `core`,
--    `module` or `worker`, and a module that ships under `modules/` registers its own families at
--    boot. A table is the one place a module can *add* to the catalogue without the core having to
--    know its name in advance. The core's own families are upserted on every boot, so a rename in
--    Rust is a rename in the table and never a stale row.
-- 2. **`last_seen_at` is the answer to "is this family still emitted?"** A family that has not
--    reported since the last boot is documented-but-dead, and an operator looking at a catalogue of
--    twenty families with eight of them empty is being shown a lie. That column turns "the panel
--    has no samples" into "this family stopped being recorded", which are different problems with
--    different fixes.
-- 3. **The cardinality the catalogue reports is measured, not declared.** `cardinality_estimate`
--    is written from the live series count on each boot, so a family quietly approaching its cap
--    says so before it starts folding, and the screen does not have to guess.
--
-- ## Why `name` is unique and NOT a foreign key anywhere
--
-- Nothing references a metric family by row id. The exposition, the chart selector and the Grafana
-- bundle all address a family by its *name*, because the name is the contract with every operator
-- tool that ever scrapes this instance. An id that nothing uses is an id that drifts from the name.
-- Hence: `name` unique, `id uuid` present only so a row has a stable handle for a future settings
-- screen, and no cascade to design around.

create table if not exists obs_metric_catalog (
    id                    uuid primary key default gen_random_uuid(),

    -- The metric name exactly as the exposition writes it (`omnion_http_requests_total`).
    name                  text not null unique,

    -- `counter`, `gauge` or `histogram`; mirrors `MetricKind::as_str` so a value written by Rust
    -- and a value checked by a reader can never disagree.
    kind                  text not null check (kind in ('counter', 'gauge', 'histogram')),

    -- The unit shown in the catalogue, e.g. `seconds`, `micros`, `1`.
    unit                  text not null default '',

    -- One sentence for the catalogue and for the exposition's HELP line.
    description           text not null default '',

    -- The label names, positionally. A recorder that offers an id here is truncated by the
    -- registry, so this list is the *declaration* of the bounded set, not a live reflection of it.
    labels                text[] not null default '{}',

    -- Who is expected to record it: `core`, `module` or `worker`.
    source                text not null default 'core'
                          check (source in ('core', 'module', 'worker')),

    -- The number of distinct series the registry held for this family at the last boot, written
    -- by the seeder. An estimate of the live value, not a live count: the count is in memory and
    -- the catalogue is for an operator between restarts.
    cardinality_estimate  int not null default 0 check (cardinality_estimate >= 0),

    -- The cap this family is held to. A family that reaches it folds into its `other` series and
    -- increments `omnion_registry_budget_exceeded`, so the number here is the point at which the
    -- scrape starts reporting a loss.
    cardinality_budget    int not null default 500 check (cardinality_budget > 0),

    -- `false` for a family that is deliberately not budgeted, which today means only
    -- `omnion_build_info`: an operator runs several versions during a rollout and each one is a
    -- legitimate distinct value.
    budgeted              boolean not null default true,

    -- When this family last reported a sample, per the process that wrote the row. NULL means it
    -- has been declared and never recorded, which the screen shows as "not recorded yet" rather
    -- than as an empty chart.
    last_seen_at          timestamptz,

    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now()
);

comment on table obs_metric_catalog is
    'The metric families this instance can record, seeded from the registry on every boot (REQ-126).';
comment on column obs_metric_catalog.name is
    'The exposition name; unique because every operator tool addresses a family by this and not by id.';
comment on column obs_metric_catalog.last_seen_at is
    'Last time a sample was recorded; NULL distinguishes "declared but never emitted" from "empty chart".';
comment on column obs_metric_catalog.cardinality_budget is
    'The series cap over which samples fold into the other-series and the fold is counted.';

-- The catalogue is read whole by the panel and written once per boot, so there is nothing to index
-- for lookups by name (`unique` covers it) and nothing to index for a filtered listing: twenty-one
-- rows. An index here would be a write cost on every boot for a read nobody filters.

-- ## Down script
--
-- The reversal is written as executable statements per the REQ-129 migration-safety policy rather
-- than as a second file, because the runner is a single forward-only `sqlx::migrate!` bundle: a
-- rollback has to be applied deliberately, by an operator, and it has to be *written down* where
-- the forward migration is read.
--
--   drop table if exists obs_metric_catalog;
--
-- A downgrade is safe in one direction only: it discards the catalogue, and a process running the
-- *previous* build does not read this table, so nothing that was working before the upgrade stops
-- working after the downgrade. Nothing outside this table references a family by row id, which is
-- exactly why the table can be dropped whole — had a settings row or an alert rule pointed at
-- `obs_metric_catalog.id`, this reversal would be a partial one and would have to say so.
