-- Omnion · 0030 · automation versions and the audit read surface
--
-- Slice 4 of the automation depth pass (docs/requests/REQ-003) finished the engine in
-- migration 0029 (rate window, concurrency, last error) and the endless-loop guard in code.
-- What is left of the request's *operations surfaces* needs two tables this migration adds:
--
-- This migration adds ONE table and one column:
--
--   * `workflow_versions` — a definition snapshot per write. The request asks for "Versions
--     (definition diffs with Restore)" and the panel has no way to show a diff it cannot
--     reconstruct: a rule is stored as a single mutable row, so yesterday's definition is
--     gone the moment `PUT` lands. Every create and every update appends a row carrying the
--     definition **as it was written**, the actor, and a summary of what changed — which is
--     also the "diff summary" the audit criterion asks for, so the two are one fact read
--     two ways rather than two summaries that can disagree.
--
--   * nothing for the audit trail — see below.
--
-- `automation_settings` (the request's data model) already ships in `0023_automation_actions`
-- with exactly the shape the request describes, so it is **not** recreated here. A migration
-- that creates a table another migration owns is the one error this repository cannot undo:
-- the fresh database stops booting and every test in the workspace goes red at once.
--
-- The audit criterion ("who changed what, when") is answered from `audit_log`, which every
-- privileged write already records; slice 4's job is to *read* it through a permission and
-- present it, not to invent a second trail. So no audit table ships here: the read endpoint
-- is the surface, and a second trail is one more place for the truth to be stale.
--
-- Append-only (docs/05-VERSIONING.md): 0029 is the number before this one and is never
-- renumbered.

-- ---------------------------------------------------------------------------------------------
-- workflow_versions: one row per definition write
-- ---------------------------------------------------------------------------------------------

create table workflow_versions (
    id              uuid        primary key,
    workflow_id     uuid        not null references workflows (id) on delete cascade,
    organization_id uuid        not null references organizations (id) on delete cascade,
    -- Which write produced this row: `created` or `updated`.
    change          text        not null,
    -- `workflows.version` at the moment of the write, so a restore can put the number back.
    version         integer     not null,
    -- The whole definition as it was written: trigger, conditions, steps, the bounds.
    definition      jsonb       not null,
    -- What changed in words: the fields whose value differs from the previous version.
    summary         jsonb       not null default '{}'::jsonb,
    -- The version this row was restored from, when it was written by a restore. `null` for a
    -- row written by editing, and a restore writes the NEXT number rather than the one it
    -- read, so the history is a line and not a loop.
    restored_from    uuid        references workflow_versions (id) on delete set null,
    -- Who wrote it. `null` when the account was deleted; the row is still the history.
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    constraint workflow_versions_change_valid check (change in ('created', 'updated', 'restored'))
);

-- The history of one rule, newest first. Partial on the rule so a delete cascades with it
-- and the index stays small.
create index workflow_versions_workflow_idx
    on workflow_versions (workflow_id, version desc);

-- A rule cannot carry two rows of the same version: a restore writes the *next* number, not
-- the one it read, so this is what makes "version" mean something.
create unique index workflow_versions_version_unique
    on workflow_versions (workflow_id, version);

-- `workflows.version` is the number the Versions tab shows and the one a restore bumps. It is
-- added here because every version write increments it in the same transaction as the row it
-- writes, and a number that only exists in the panel's mind is a number nobody can restore to.
--
-- It starts at **0**, not 1: the first version is the rule's *creation*, and `record` claims a
-- number by incrementing. A default of 1 would make a brand-new rule's create write version 2
-- and leave version 1 as a number that names no write — the history would then begin at 2 for
-- no reason a person could explain. A rule written before this migration has no version row at
-- all, so its 0 is the honest answer and the tab says so in words rather than inventing a
-- history.
alter table workflows add column if not exists version integer not null default 0;

-- Any rule that already existed carries no version row (this migration creates the table), so
-- every one of them is 0. Written out rather than left to the default so a database that
-- carried a `version` column from an earlier partial run is corrected too.
update workflows set version = 0;
