-- Omnion · 0049 · media: retention policies, the run log and reference-based purge refusal
-- (REQ-010, slice 4)
--
-- Slice 1 gave the library a trash, slice 2 a version history, slice 3 share links and a
-- duplicate report, and `0044`/`0047` gave scanning and grants. What is still missing is the
-- part of a file manager that *forgets*: a folder nobody prunes grows until the object store
-- refuses the next upload, and the manual `Empty trash` button is an answer that only exists
-- when somebody remembers to press it.
--
-- Five decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **A legal hold beats every other rule, and it beats it by exclusion rather than by
--     order.** The alternative is a `where` clause that checks the hold in three places, in
--     the version sweep, in the trash sweep and in the purge — and the first place somebody
--     edits forgets the third. Here the hold is a column on `media`, so every destructive
--     statement is the *same* statement with the hold named once, and a file under hold is
--     invisible to all of them at once. The cost is a column every upload writes, and that is
--     cheaper than a rule three code paths disagree about.
--   * **The version sweep keeps the version the file is currently serving, whatever its age.**
--     The obvious query is "delete every version older than N days", and it deletes the
--     version the file is serving — leaving `media.storage_key` naming an object that no longer
--     exists. The current version is the highest number in `media_versions` (the same number
--     `next_version` hands out under a row lock), so it is excluded by *number* rather than by
--     age, and a policy with a keep-window of one day still cannot empty a file's history.
--   * **A purge refuses a file that is still referenced, and names the resources.** The
--     `media_references` rows already exist (slice 3, for the duplicate merge's repoint), and
--     they cascade away with the file — so a purge *can* delete a hero image a live page
--     resolves to, silently, with the page left holding a uuid nothing answers to. A purge is
--     a hard delete with no restore, which is exactly the operation that has to check, and the
--     refusal names the referring records rather than counting them: "3 pages still use this"
--     is an answer, "cannot purge" is not.
--   * **The run log is written for a run that found nothing.** "The last retention run was at
--     02:00 and it was clean" is the sentence that has to be available on the day an operator
--     asks why a file is still here, and a table that only records activity cannot answer it
--     on the day nothing happened.
--   * **A site gets its policy row from a trigger, not from a seed.** The same gap `0028` and
--     `0044` closed for presets and scan settings: a seed covers the sites that existed when
--     the migration ran, and a site created afterwards gets a pleasant `GET` that answers with
--     platform defaults while every save writes nothing. Onboarding, the tenancy API and a
--     future import each insert the site row themselves, so only a trigger sees all of them.

-- ---------------------------------------------------------------------------------------------
-- Policies
-- ---------------------------------------------------------------------------------------------

-- A retention policy is scoped to a site, and optionally to one folder. The scope is a pair of
-- nullable columns rather than a `path_prefix` string: a prefix match is evaluated against a
-- path an operator can retype, and a folder that gets renamed would silently fall out of its
-- own policy.
create table media_retention_policies (
    id                  uuid        primary key default gen_random_uuid(),
    site_id             uuid        not null references sites (id) on delete cascade,
    name                text        not null,
    -- When set, the policy governs this folder and everything under it. Null means the whole
    -- site, which is a scope rather than a wildcard: it is the row that is authoritative.
    folder_id           uuid        references media_folders (id) on delete cascade,
    -- How long a superseded version survives. The current version is exempt whatever its age.
    keep_versions_days  integer     not null default 365,
    -- How long a trashed file may be restored from.
    trash_days          integer     not null default 30,
    -- How long after the trash window a trashed file's bytes are actually removed. Never less
    -- than the trash window, so "purge before the restore window closes" is unrepresentable.
    purge_after_days    integer     not null default 90,
    -- A hold that beats every other rule. See the note at the top of the file.
    legal_hold          boolean     not null default false,
    enabled             boolean     not null default true,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint media_retention_policies_name_present check (length(btrim(name)) > 0),
    constraint media_retention_policies_name_length check (length(name) <= 120),
    constraint media_retention_keep_versions_positive check (keep_versions_days >= 1),
    constraint media_retention_trash_positive check (trash_days >= 1),
    constraint media_retention_purge_positive check (purge_after_days >= 1),
    -- The rule that makes the whole table safe to write: a policy that purges a file while it
    -- is still restorable would delete bytes the operator was promised they had.
    constraint media_retention_purge_after_trash check (purge_after_days >= trash_days),
    constraint media_retention_policy_name_key unique (site_id, name)
);

comment on table media_retention_policies is
    'How long a library keeps what. One row per site plus one per folder; the folder row wins '
    'for its subtree, and the site row is the fallback for everything else (REQ-010).';

comment on column media_retention_policies.legal_hold is
    'A hold that removes the file from every destructive sweep, at every depth, regardless of '
    'the windows beside it. Set it for a file under litigation, an audit or a dispute, and '
    'clear it by hand — no window can shorten it and no worker can shorten it.';

-- The lookup a sweep runs: the enabled policies of one site, and the folder subtree a policy
-- governs. `(site_id, folder_id)` because the worker asks per site and the screen asks per
-- folder; the site-only policies are the `folder_id is null` end of the index.
create index media_retention_policies_site_idx
    on media_retention_policies (site_id, folder_id) where enabled;

-- ---------------------------------------------------------------------------------------------
-- The hold
-- ---------------------------------------------------------------------------------------------

-- The hold lives on the file, not on the policy, and that is the point. Three sweeps have to
-- respect it (versions, trash, purge) and a policy-level flag would have to be joined into
-- every one of them; a column on the row makes each of them a single `where` that cannot
-- forget the others.
alter table media add column legal_hold boolean not null default false;

comment on column media.legal_hold is
    'Refuses every retention sweep on this file, at any depth and under any policy. Set by '
    'hand with a reason in the audit log; cleared by hand.';

-- The worker's own index. `media_trash_idx` is `(site_id, deleted_at desc) where deleted_at
-- is not null` and cannot answer "trashed more than 30 days ago" without reading the whole
-- index, so the eligible set gets its own partial index — written as `(site_id, deleted_at)`
-- because the sweep filters on the site first.
create index media_retention_trash_sweep_idx
    on media (site_id, deleted_at, id) where deleted_at is not null;

-- The version sweep reads one file's history and filters by age; `0026` already indexes
-- `(media_id, version desc)`, which is exactly that shape.

-- ---------------------------------------------------------------------------------------------
-- Runs
-- ---------------------------------------------------------------------------------------------

-- One row per sweep. A run that found nothing writes a row, because "the last retention run
-- was at 02:00 and it changed nothing" is the sentence an operator needs on the day they are
-- asking why a file is still in the trash.
create table media_retention_runs (
    id              uuid        primary key default gen_random_uuid(),
    site_id         uuid        references sites (id) on delete cascade,
    -- Null for an installation-wide pass that walked every site.
    policy_id       uuid        references media_retention_policies (id) on delete set null,
    -- `daily`, `manual`, `versions`, `trash` or `purge`. The first three say *who asked*; the
    -- last two say *what the run was for*, because "a run removed 0 files" is a different
    -- sentence from "the version sweep removed 0 files".
    kind            text        not null,
    -- Versions whose superseded history left the object store.
    versions_removed  bigint     not null default 0,
    -- Bytes the same removal reclaimed.
    versions_bytes  bigint      not null default 0,
    -- Trashed files whose bytes left.
    purged          bigint      not null default 0,
    purged_bytes    bigint      not null default 0,
    -- Files the sweep wanted to purge and did not, because something still points at them.
    -- Counted rather than merely refused, so the run log answers "why is my library still
    -- full" without the operator opening a file.
    refused         bigint      not null default 0,
    -- Files the hold removed from the eligible set.
    held_back       bigint      not null default 0,
    -- The run that could not finish. Null on a run that did.
    error           text        not null default '',
    actor_user_id   uuid        references users (id) on delete set null,
    started_at      timestamptz not null default now(),
    finished_at     timestamptz,
    constraint media_retention_runs_kind_check
        check (kind in ('daily', 'manual', 'versions', 'trash', 'purge')),
    constraint media_retention_runs_non_negative
        check (versions_removed >= 0 and versions_bytes >= 0 and purged >= 0
               and purged_bytes >= 0 and refused >= 0 and held_back >= 0),
    constraint media_retention_runs_time_sane
        check (finished_at is null or finished_at >= started_at)
);

create index media_retention_runs_site_idx
    on media_retention_runs (site_id, started_at desc);

comment on table media_retention_runs is
    'One row per retention pass, including a pass that found nothing. Retention is the one '
    'library feature whose *absence* of activity is indistinguishable from it being broken, so '
    'the log records the runs as well as the work (REQ-010).';

-- ---------------------------------------------------------------------------------------------
-- Seed and trigger
-- ---------------------------------------------------------------------------------------------

-- The fallback policy every site needs: 30 days in the trash, 90 days to the bytes, a year of
-- superseded versions. `Standard retention` rather than a name the screen invents, so an
-- operator who edits it is editing *a* row they can find by name.
insert into media_retention_policies (site_id, name)
select id, 'Standard retention' from sites
on conflict (site_id, name) do nothing;

create or replace function media_retention_policy_default() returns trigger
language plpgsql as $$
begin
    insert into media_retention_policies (site_id) values (new.id) on conflict do nothing;
    return new;
end $$;

create trigger media_retention_policy_default
    after insert on sites
    for each row execute function media_retention_policy_default();

-- ---------------------------------------------------------------------------------------------
-- The queries the sweeps run
-- ---------------------------------------------------------------------------------------------

-- Everything a sweep needs is in the crate (`crates/media/src/retention.rs`) rather than in a
-- view, for one reason: a view cannot take a *policy* as an argument, and the whole question
-- this migration answers is "which policy governs this file". The subtree match is written
-- there against `media_folders.path`, the same materialised path `0025` built.
