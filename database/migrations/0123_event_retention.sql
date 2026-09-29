-- 0123_event_retention.sql — the event bus's own retention (REQ-016, slice 3).
--
-- The bus is append-only and the platform writes to it on every mutation, so an installation
-- that runs for a year accumulates a table whose growth has no ceiling and no reader past a
-- point: `/events` shows the last page, the API keeps a keyset cursor over every row, and the
-- automation matcher replays from its own cursor. Nothing ever *forgets*.
--
-- Four decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **A pending delivery is not swept, and the reason is that a receiver may still be
--     waiting for it.** The obvious query is "delete events older than N and let the cascade
--     take the deliveries with it", and that cascade is exactly the bug: a row the runner
--     has not reached yet — `next_attempt_at` in the future, or claimed by a runner that
--     died — would be deleted out from under an endpoint that never got its fact. The
--     sweep therefore selects events with **no delivery at all** or with **only settled
--     deliveries**, and a `pending` row pins its event for ever. An event nobody ever
--     queued to anybody is the bulk of the bus, which is exactly the part worth deleting.
--   * **The window is a column on the organization, not a column on the event.** "30 days"
--     is a policy an operator sets once and then changes; storing it per event would mean a
--     sweeper that has to *compare* the two to decide what is old. The rule is read from
--     `organizations.event_retention_days` and the cutoff is computed per organization in the
--     same statement, so an organization that changes its own window changes what its next
--     tick deletes — and an organization that has never set one falls back to the platform
--     default rather than to `null`, because `null` would mean "keep for ever", which is a
--     decision nobody made deliberately.
--   * **The run log is written for a run that deleted nothing.** The same rule `0049` set for
--     the media sweeper, and for the same reason: "the last sweep was at 03:00 and it found
--     nothing" is the sentence an operator needs on the day they ask why an event from
--     March is still here. A table that only records activity cannot answer it on the day
--     nothing happened.
--   * **A sweep is per organization and bounded.** One statement over the whole table would
--     take a lock proportional to the entire backlog and hold it while every row cascades.
--     The sweeper walks organizations with a `limit` and deletes in batches, so a large
--     backlog is worked down over several ticks instead of in one transaction nobody can
--     cancel.

-- ---------------------------------------------------------------------------------------------
-- The policy
-- ---------------------------------------------------------------------------------------------

-- How long this organization keeps its events. `not null default 30` rather than a nullable
-- column: a sweep needs a number, and an organization whose window is unknown is an
-- organization whose history grows for ever because nobody ever decided. `check` refuses a
-- zero or negative window, which would mean "delete everything immediately" and is never
-- what a typed value means.
alter table organizations
    add column if not exists event_retention_days integer not null default 30;

alter table organizations
    drop constraint if exists organizations_event_retention_days_check;
alter table organizations
    add constraint organizations_event_retention_days_check
        check (event_retention_days between 1 and 3650);

-- The sweeper's only index is on `created_at`, because the only thing it ever asks the table
-- is "which rows are older than this". The feed's keyset read uses `id`, which the primary
-- key already carries.
create index if not exists events_created_at_idx on events (created_at);

-- ---------------------------------------------------------------------------------------------
-- The run log
-- ---------------------------------------------------------------------------------------------

-- One row per sweep, written whether or not it deleted anything. `organization_id` is
-- nullable so a platform-level run (a sweep of the `null` organization, whose events are the
-- platform's own) is representable, and `deliveries` is separate from `events` because a
-- cascade removes a delivery the operator may still be looking for in the endpoint's
-- history — the count that says so belongs to the run that did it.
create table event_retention_runs (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        references organizations (id) on delete cascade,
    started_at      timestamptz not null default now(),
    finished_at     timestamptz,
    window_days     integer     not null,
    cutoff          timestamptz not null,
    events_deleted  integer     not null default 0,
    deliveries_deleted integer  not null default 0,
    failed          integer     not null default 0,
    error           text
);

-- The screen reads "the last run for this organization", which is the run log read newest
-- first; without this the answer is a sort over every sweep the organization has ever run.
create index if not exists event_retention_runs_org_started_idx
    on event_retention_runs (organization_id, started_at desc);

-- A finished run is the only one the screen shows, and an unfinished one is either a run in
-- flight or a process that died mid-sweep — which is why the runner stamps `finished_at` even
-- when it deleted nothing.
create index if not exists event_retention_runs_finished_idx
    on event_retention_runs (finished_at) where finished_at is not null;
