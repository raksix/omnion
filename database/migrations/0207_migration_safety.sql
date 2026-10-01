-- The migration ledger, the run journal, the policy singleton and the lint findings
-- (docs/requests/REQ-129, slice 1).
--
-- **Why these four tables are one migration.** A ledger row with nowhere to record who ran it, a
-- run with no policy to run it under, and a policy with nowhere to record what the lint found are
-- all states no screen can render. The ledger screen reads all four to answer one question —
-- "may this release's database be rolled back, and by whom, and has anybody proved it" — so they
-- ship together.
--
-- **`schema_migrations` is NOT SQLx's `_sqlx_migrations`.** SQLx keeps its own bookkeeping table
-- because it has to survive a binary that no longer exists, and it answers exactly one question:
-- has this version been applied. It carries no actor, no source, no duration and no reversibility
-- evidence, and it cannot say who ran it or whether the reversal was ever rehearsed. This ledger
-- is the operator-facing one and the two are reconciled, not merged: the runner writes both, and a
-- row here is only written when SQLx has committed the corresponding migration, so the ledger can
-- never claim something the database did not do.
--
-- **`checksum` is over the file as applied, never a re-derivation.** The whole point of the
-- column is that an operator editing an applied migration is caught. A checksum recomputed from
-- whatever the file says at read time agrees with itself forever and detects nothing, so the
-- checksum is written once from the bytes that ran and compared on every later read.
--
-- **An edit is drift, and the only fix is a new migration.** There is no "update the checksum"
-- path in the policy table on purpose: a button that re-blesses an edited migration destroys the
-- only evidence that the schema on disk and the schema in the database were ever the same.
--
-- **`migration_policy` is a single row.** A policy with a history is a versioned policy with a
-- history table (the deployment centre's config versioning already does that for the release
-- manifest), so the second one here would be the second implementation of the same idea. What
-- this table keeps is the CURRENT setting plus who changed it; the audit trail is the audit log,
-- which every other write in this tree already uses.
--
-- **Waivers survive a re-lint because they are keyed, not indexed by finding order.** The lint
-- pass rewrites `migration_violations` on every run; a waiver column on a row that is deleted and
-- recreated would vanish on the next run. So a waiver is a row here whose uniqueness is
-- `(version, pattern, line)` — the same key the finding uses — and re-detecting a finding that is
-- waived restores the waiver instead of clearing it.

-- ---------------------------------------------------------------------------
-- The ledger: one row per migration this installation has applied.
-- ---------------------------------------------------------------------------
create table schema_migrations (
    -- The `NNNN` prefix, zero padded, as a TEXT. It is not an integer because the file naming
    -- convention is `NNNN_name.sql` and a ledger keyed by an int loses the leading zeros that make
    -- `0199` sort before `0200` in every editor an operator is looking at.
    version             text primary key check (version ~ '^[0-9]{4,}$'),
    name                text not null,
    -- sha256 of the file exactly as applied. Compared on every read; a mismatch is drift and
    -- blocks the run with a message naming this file.
    checksum            text not null check (checksum ~ '^[0-9a-f]{64}$'),
    applied_at          timestamptz not null default now(),
    duration_ms         int not null default 0 check (duration_ms >= 0),
    statement_count     int not null default 0 check (statement_count >= 0),
    -- Who ran it: an account id, or a service name for a deploy job or CI run. Deliberately not
    -- a foreign key to `users`, for the same reason REQ-128's upgrade acknowledgement is not: an
    -- installation's schema history has to survive the account that applied it being deleted.
    actor               text not null,
    source              text not null check (source in ('cli', 'deploy', 'ci', 'boot')),
    -- `false` means "no down script was found in the file", which is NOT the same as "the down
    -- script does not work" and NOT the same as "there is no down script and that is fine". The
    -- policy's `require_down_scripts` decides which of those is an error.
    has_down            boolean not null default false,
    -- Set by the up → down → up gate, never by hand. A NULL here is the honest state.
    down_verified_at    timestamptz,
    down_verified_by    text,
    -- The reason a migration was allowed to ship without a down script, copied from its waiver so
    -- the ledger row says why on its own.
    waiver_reason       text,
    constraint schema_migrations_verified_pair check (
        (down_verified_at is null and down_verified_by is null)
        or (down_verified_at is not null and down_verified_by is not null)
    )
);

comment on table schema_migrations is
    'One row per migration this installation applied, with the checksum of the file as applied and the evidence — or the absence of evidence — that its reversal was rehearsed. down_verified_at is the only field that means a down script was executed against a real database.';

-- The reverse lookup: "which migrations arrived after this one", which is what an operator asks
-- when a release moved a column and they want the blast radius.
create index schema_migrations_applied_idx
    on schema_migrations (applied_at desc);

-- ---------------------------------------------------------------------------
-- The run journal: every apply, every reversal, every failure, including the ones that never
-- touched a migration.
-- ---------------------------------------------------------------------------
create table migration_runs (
    id              bigserial primary key,
    version         text not null,
    direction       text not null check (direction in ('up', 'down')),
    status          text not null check (status in ('running', 'succeeded', 'failed', 'aborted')),
    started_at      timestamptz not null default now(),
    finished_at     timestamptz,
    duration_ms     int check (duration_ms is null or duration_ms >= 0),
    actor           text not null,
    source          text not null check (source in ('cli', 'deploy', 'ci', 'boot')),
    error           text,
    -- The plan this run executed: statements, lock risk, violations. Written at start so a
    -- FAILED row can still be read as "this is what it was trying to do", which is the only
    -- useful thing about a failed migration.
    plan            jsonb not null default '{}',
    -- A run that reached `finished_at` is one whose outcome is known; the inverse is not true, so
    -- the constraint goes the direction that is actually checkable.
    constraint migration_runs_finished_has_duration check (
        finished_at is null or duration_ms is not null
    )
);

create index migration_runs_version_started_idx
    on migration_runs (version, started_at desc);

-- The partial index the lock screen uses: a run in `running` is a run holding the advisory lock,
-- so this is the shortest possible "is a migration running right now" query.
create index migration_runs_running_idx
    on migration_runs (started_at) where status = 'running';

comment on table migration_runs is
    'The journal of migration runs. A row is written before the first statement and updated after the last, so a crashed runner leaves a `running` row that is evidence rather than nothing — which is why the status vocabulary has no "never happened".';

-- ---------------------------------------------------------------------------
-- The policy singleton: what the lint refuses, and how long a blocked DDL waits before it fails.
-- ---------------------------------------------------------------------------
create table migration_policy (
    -- `id = 1` is what makes this a singleton without a trigger: a second row is a constraint
    -- violation rather than a second policy somebody has to reconcile.
    id                              smallint primary key default 1 check (id = 1),
    require_down_scripts            boolean not null default true,
    -- Short on purpose. A DDL that waits behind a long transaction blocks everything queued behind
    -- it, so failing in five seconds and naming the blocker beats succeeding in five minutes.
    lock_timeout_ms                 int not null default 5000
        check (lock_timeout_ms between 100 and 60000),
    statement_timeout_ms            int not null default 300000
        check (statement_timeout_ms between 1000 and 3600000),
    -- pattern → enabled. The patterns themselves are the code's closed vocabulary
    -- (`omnion_migrations::lint::PATTERNS`); this table says which of them are switched ON, so a
    -- maintainer can tighten a rule in an installation without a new binary.
    banned_patterns                 jsonb not null default '{}',
    backfill_batch_size             int not null default 5000
        check (backfill_batch_size between 100 and 100000),
    backfill_rate_per_second        int not null default 200
        check (backfill_rate_per_second > 0),
    require_approval_for_destructive boolean not null default true,
    updated_by                      text,
    updated_at                      timestamptz not null default now()
);

comment on table migration_policy is
    'The single row of migration policy. A pattern switched off here is not "allowed", it is "not enforced by this installation", and the plan preview says which of the two it was.';

-- Seeded by `default policy` rather than by an `insert ... select` above, for one reason: the
-- default has to be readable in Rust too (`policy::default_row()`), and two copies of a default
-- drift. This statement is the copy that lands in the database; the Rust one is proven equal to it
-- by the walk that asserts a fresh install's row.
insert into migration_policy (id) values (1)
    on conflict (id) do nothing;

-- ---------------------------------------------------------------------------
-- What the lint found, and what a maintainer said about it.
--
-- The findings table is rewritten on every CI run; the waiver is not. A waiver lives in the same
-- row it waives because the key is `(version, pattern, line)` — the finding's own identity — so
-- re-detecting a waived finding carries the waiver forward instead of clearing it.
-- ---------------------------------------------------------------------------
create table migration_violations (
    id              bigserial primary key,
    version         text not null,
    pattern         text not null,
    severity        text not null check (severity in ('error', 'warning')),
    line            int not null check (line > 0),
    excerpt         text not null default '',
    detected_at     timestamptz not null default now(),
    waived_by       text,
    waived_at       timestamptz,
    waiver_reason   text,
    constraint migration_violations_waiver_complete check (
        (waived_by is null and waived_at is null and waiver_reason is null)
        or (waived_by is not null and waived_at is not null and length(btrim(waiver_reason)) > 0)
    ),
    -- The finding's identity, not its row's. This is what makes a waiver survive a re-lint.
    constraint migration_violations_identity unique (version, pattern, line)
);

create index migration_violations_unwaived_idx
    on migration_violations (severity) where waived_at is null;

comment on table migration_violations is
    'Banned-shape findings over the shipped migration files, rewritten by every CI run. The uniqueness constraint is on the FINDING, so a waiver is carried forward across re-lints instead of silently expiring the next time somebody pushes a migration.';

-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
-- ---------------------------------------------------------------------------
-- Reverse order, children before parents. `if exists` throughout: a down script that fails
-- halfway leaves an instance in a state neither script can continue from.
--
--   drop table if exists migration_violations;
--   drop table if exists migration_policy;
--   drop table if exists migration_runs;
--   drop table if exists schema_migrations;
--
-- This file is the FIRST migration in the tree that is required to have a working down script,
-- because the policy row above says `require_down_scripts = true` and the ledger is what makes
-- that requirement enforceable rather than aspirational. It is written in the commented-out form
-- every other migration here uses, for the reason `0162_reliability.sql` records at length: the up
-- half is run by `Db::migrate`, and a reversal written as live statements would be executed by it
-- too — on the first apply it would create the policy singleton and then drop every table this
-- file defines, leaving the instance with no ledger and a migration row claiming success.
--
-- The reversal is nevertheless VERIFIABLE, which is the point of this file: `omnion migrate
-- verify-down 0207` extracts the commented statements, runs them against a scratch database
-- rebuilt from the previous migration, and compares the resulting structure against the structure
-- from before the apply. A file whose down script is prose rather than statements fails that
-- gate, and this one is the first thing in the repository that has to pass it.
