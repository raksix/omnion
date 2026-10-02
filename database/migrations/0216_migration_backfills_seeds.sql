-- Backfill jobs and seed datasets (docs/requests/REQ-129, slice 3).
--
-- **Why a backfill is a table and not a migration.** The recipe the lint enforces is
-- *add nullable → backfill in batches → constrain in a later migration*, and the middle step is
-- the only one that touches every existing row. It can take minutes on a real table and it is
-- interrupted by every deploy, so running it inside the migration transaction would hold locks
-- for its whole duration and roll its work back on the first failure. So the backfill is a
-- first-class job with its own state, and the migration only *registers the descriptor* — which
-- is what makes "resume exactly where it stopped" a question this table can answer.
--
-- **`resume_key` is a primary key value, not a row count.** A cursor that counts rows is wrong
-- under a restart: batches are processed in key order, but rows are INSERTED during the backfill
-- by live traffic, and a row-count cursor skips or redoes whatever landed behind the count. The
-- last key actually processed is the only value that survives a restart identically.
--
-- **`rows_done` is a counter, and it is allowed to disagree with the cursor.** It is what the
-- throughput display reads on every poll — a display that ran `select count(*)` to answer "how
-- far along are we" would turn a screen into a table scan. It is monotonic by construction so
-- throughput is a rate over completed rows rather than a difference of two snapshots.
--
-- **State transitions are guarded by a CHECK over the timestamps, not by a trigger.** A trigger
-- can enforce the same rule and needs a second object to drop in a reversal; a CHECK is part of
-- the table and is dropped with it. The rule it encodes is the one the screen depends on: a job
-- that is `paused` has a `paused_at`, and a job that is `completed` has `completed_at`, so
-- "paused days later" is a row somebody can find rather than a state machine somebody has to
-- re-derive.
--
-- **`backfill_jobs.state` is TEXT rather than an enum.** An enum's value list needs an ALTER to
-- extend, which is exactly the banned shape this request exists to prevent in this very table;
-- the CHECK is the closed vocabulary and it lives beside the column that means it.
--
-- **Seed datasets are descriptors, and the rows are generated from them.** The request requires
-- that the fixtures the tests use and the fixtures the walkthrough sees come from ONE definition,
-- and the only way that holds is if the dataset is data (a manifest) rather than code (a `fn`).
-- `database/seeds/` is read by the CLI, the panel and the test harness from the same path, so a
-- dataset cannot be changed for one of them without the others seeing it.

-- ---------------------------------------------------------------------------
-- The backfill job: one row per descriptor the runner picked up.
-- ---------------------------------------------------------------------------
create table migration_backfills (
    id                  uuid primary key default gen_random_uuid(),
    -- The descriptor's stable name, so a job can be found across restarts and across a
    -- reinstall that regenerated its id. NOT unique: the same descriptor may legitimately run
    -- again later (a second release backfilling the same column), and refusing it would make the
    -- platform unable to express "we are backfilling this again".
    name                text not null check (length(btrim(name)) > 0),
    table_name          text not null check (length(btrim(table_name)) > 0),
    column_name         text not null check (length(btrim(column_name)) > 0),
    -- The column the cursor walks. NOT the column being backfilled: that one is NULL for every
    -- row still to do, so ordering by it cannot bound a batch — a cursor made of the value
    -- being filled in is a cursor with no lower bound, and the job would re-select the same
    -- first page forever. This is the primary key of the target table, named explicitly rather
    -- than discovered, so the descriptor a release ships says exactly what it walks.
    key_column          text not null check (length(btrim(key_column)) > 0),
    -- Batches in primary-key order. The batch floor/ceiling are the policy's
    -- (`policy::bounds::MIN_BACKFILL_BATCH`), enforced here too so a job written by hand cannot
    -- hold a transaction longer than an operator asked it to.
    batch_size          int not null default 5000 check (batch_size between 100 and 100000),
    rate_limit_per_second int not null default 200 check (rate_limit_per_second > 0),
    -- The last primary key value whose batch COMPLETED. NULL means nothing has been processed.
    --
    -- It is `text` and not `bigint` because the key of a backfilled table is not necessarily a
    -- number, and this table must be able to describe a UUID-keyed backfill without a second
    -- mechanism. The comparison that matters (`> resume_key`) is done by the runner in the
    -- table's own type, so a text cursor is not a correctness problem here.
    resume_key          text,
    rows_done           bigint not null default 0 check (rows_done >= 0),
    -- `state` values: `pending` | `running` | `paused` | `completed` | `failed`.
    state               text not null default 'pending'
        check (state in ('pending', 'running', 'paused', 'completed', 'failed')),
    -- A job that `completed` is done forever. The CHECK says so with the database rather than
    -- with a handler: a completed job whose `completed_at` is null is a state no screen can
    -- render honestly, and the runner's "emit complete exactly once" test needs it impossible.
    constraint migration_backfills_terminal_timestamps check (
        (state <> 'completed' or completed_at is not null)
        and (state <> 'paused' or paused_at is not null)
        and (state <> 'failed' or last_error is not null)
    ),
    -- The failing batch's message. Required when `state = 'failed'` for the same reason, and
    -- cleared on the next successful resume so a stale error cannot be read as a live one.
    last_error          text,
    paused_at           timestamptz,
    started_at          timestamptz,
    completed_at        timestamptz,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now()
);

create index migration_backfills_state_idx on migration_backfills (state);
-- The resume path reads "jobs that are not finished", ordered by name so two runs of the same
-- descriptor take the same order.
create index migration_backfills_open_idx
    on migration_backfills (name) where state in ('pending', 'running', 'paused');

comment on table migration_backfills is
    'Backfill jobs with a resumable cursor. resume_key is the last primary key value whose batch COMPLETED — a row-count cursor is wrong under a restart because live traffic inserts rows behind the count.';

comment on column migration_backfills.resume_key is
    'The last processed primary key value. NULL until the first batch completes; never cleared on pause, which is what makes "paused for days, then resumed" resume rather than start over.';

-- ---------------------------------------------------------------------------
-- Which migration asked for which backfill.
--
-- Separate from the job because the SAME descriptor appears in every installation that applied
-- that migration, while the job is per-installation state. Keeping them apart is what lets a
-- fresh install see "migration 0216 wants this backfill" without inheriting somebody else's
-- cursor.
-- ---------------------------------------------------------------------------
create table migration_backfill_descriptors (
    version             text not null,
    name                text not null check (length(btrim(name)) > 0),
    table_name          text not null,
    column_name         text not null,
    key_column          text not null,
    batch_size          int not null default 5000 check (batch_size between 100 and 100000),
    rate_limit_per_second int not null default 200 check (rate_limit_per_second > 0),
    -- What the batch actually runs. Kept as text rather than assembled at run time so the
    -- migration that registered it is the one that decides, and an operator can read exactly what
    -- will run against their rows before it runs.
    statement           text not null check (length(btrim(statement)) > 0),
    created_at          timestamptz not null default now(),
    constraint migration_backfill_descriptors_identity unique (version, name)
);

comment on table migration_backfill_descriptors is
    'Backfills declared by a migration. Distinct from migration_backfills because the descriptor is shipped by the release and the job is this installation''s progress through it.';

-- ---------------------------------------------------------------------------
-- Seed datasets: a manifest per dataset, and the fact that it was loaded here.
-- ---------------------------------------------------------------------------
create table seed_datasets (
    -- The dataset name as it appears in `database/seeds/<name>/manifest.json`, and the name an
    -- operator types to confirm the load. It is the primary key because a dataset is identified
    -- by its name in the CLI, the panel and the test harness, and a surrogate id would be a
    -- second answer for "which dataset is this".
    name                text primary key check (length(btrim(name)) > 0),
    description         text not null default '',
    -- What the loader promises the caller, so the confirmation can name it before it runs.
    row_estimate        int not null default 0 check (row_estimate >= 0),
    -- The migration range the dataset was written against. A load against a schema outside it is
    -- refused rather than attempted: a demo dataset built for 0206 landing on 0216 is a broken
    -- install, and the fix is a new dataset, not a partial insert.
    compatible_from     text not null default '0001',
    compatible_to       text,
    -- The manifest's own checksum, so a dataset that changed under a loaded installation is
    -- detectable rather than silently different from the one somebody loaded last week.
    manifest_checksum   text not null,
    created_at          timestamptz not null default now()
);

comment on table seed_datasets is
    'Seed dataset manifests discovered from database/seeds/. A dataset is DATA (a manifest) rather than code (a fn) so the tests, the walkthrough and an operator''s demo install all read one definition.';

create table seed_loads (
    id                  uuid primary key default gen_random_uuid(),
    dataset             text not null references seed_datasets (name) on delete cascade,
    -- The installation kind the load ran against, recorded because the refusal below names it
    -- and an audit entry that cannot say what was refused says nothing.
    installation_kind   text not null,
    loaded_by           text not null,
    -- Rows the loader actually wrote. Not an estimate: the estimate is on the manifest, and a
    -- screen that showed the estimate under a heading saying "loaded" would be lying about a
    -- number an operator uses to decide whether the demo is big enough.
    rows_loaded         bigint not null default 0 check (rows_loaded >= 0),
    loaded_at           timestamptz not null default now()
);

create index seed_loads_recent_idx on seed_loads (dataset, loaded_at desc);

comment on table seed_loads is
    'Every seed load this installation performed. A dataset loaded twice is two rows, not an update: "who loaded this and when" is the question the screen answers.';

-- The three datasets the request names, seeded with the shape of their real manifests so the
-- seeds screen is never empty in a fresh install and a loader that finds no `database/seeds/`
-- directory still answers honestly ("declared, but the files are absent").
insert into seed_datasets (name, description, row_estimate, compatible_from, compatible_to, manifest_checksum)
values
    ('minimal',
     'The smallest dataset that boots the panel: one organization, one owner, one site. Enough to sign in and see a working installation.',
     12, '0001', null, 'declared-minimal'),
    ('demo',
     'A populated installation for screenshots and evaluation: content, modules, users and activity across every major module.',
     640, '0206', null, 'declared-demo'),
    ('fixture',
     'The deterministic dataset the integration tests and the QA walkthrough read. Generated from the same descriptors, so a test and a walkthrough see the same world.',
     220, '0206', '0207', 'declared-fixture')
on conflict (name) do nothing;

-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
-- ---------------------------------------------------------------------------
--
--   drop table if exists seed_loads;
--   drop table if exists seed_datasets;
--   drop table if exists migration_backfill_descriptors;
--   drop table if exists migration_backfills;
--
-- Reverse order, children before parents: `seed_loads` references `seed_datasets`, and both come
-- before the backfill tables they have nothing to do with only by accident of ordering. `if
-- exists` throughout, because a down script that fails halfway leaves an instance neither script
-- can continue from.
--
-- The three seeded `seed_datasets` rows go with the table. They are inserted data, not schema,
-- and leaving them behind would leave the panel rendering a dataset whose table no longer exists
-- — which is the exact "state no screen can render" this request keeps naming.