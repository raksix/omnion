-- Omnion · 0164 · automation projects: the container every automation resource lives in
--
-- REQ-133 slice 1 ("entity, membership and backfill"). A project is a named bucket for
-- workflows, credentials, folders, schedules and runs, with its own members and its own limits —
-- the thing an organization uses to keep one team's automations away from another's before it is
-- a permissions question.
--
-- The design decision this migration exists to make safe is the ORDER, because every part of it
-- is a constraint applied to rows that already exist:
--
--   1. create `automation_projects`;
--   2. create exactly one DEFAULT project per organization that already has one;
--   3. add a NULLABLE `project_id` to every container table;
--   4. backfill every existing row into its organization's default;
--   5. only then apply `not null`.
--
-- Applying the constraint before the backfill refuses the whole migration on any populated
-- database and is silent-OK on an empty one, which is how "it works on a fresh install" gets
-- written about a migration that has never run against real rows. The gate runs the same order
-- against a seeded database, because the empty case is the one that cannot fail.
--
-- Foreign keys from resources to projects use `on delete restrict`, NOT cascade: a project with
-- dependencies cannot disappear quietly. Deletion is a deliberate act with its own dependency
-- check (slice 4), and a cascade here would make "delete a project" a way to delete twenty
-- workflows without an audit row for any of them.
--
-- Released migrations are append-only (docs/05-VERSIONING.md). The ledger is shared across ten
-- writers, so the number is the high-water across every branch at commit time, not this one's.

create table automation_projects (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    -- The short form people write in a ticket ("PLAT", "OPS"). Uppercase letters and digits,
    -- 2..8 of them: long enough to be memorable, short enough to type into a search box.
    key             text        not null,
    name            text        not null,
    description     text        not null default '',
    color           text        not null default '#C96442',
    icon            text        not null default 'folder',
    is_default      boolean     not null default false,
    status          text        not null default 'active',
    owner_user_id   uuid        references users (id) on delete set null,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint automation_projects_key_format check (key ~ '^[A-Z0-9]{2,8}$'),
    constraint automation_projects_name_not_blank check (length(btrim(name)) > 0),
    constraint automation_projects_status_valid check (status in ('active', 'archived')),
    -- An archived project is read-only, so it may not take new work. The default project is
    -- where every resource without an explicit project lands, so archiving it would refuse an
    -- insert on every path that does not pass a project — a dead platform, not a protected one.
    constraint automation_projects_default_is_active check (not is_default or status = 'active')
);

-- A key is unique inside an organization; two organizations may both call themselves OPS.
create unique index automation_projects_org_key_uidx on automation_projects (organization_id, key);
create index automation_projects_org_status_idx on automation_projects (organization_id, status);
-- The default project is a singleton per organization. Enforced by an index rather than a
-- constraint because Postgres has no filtered unique constraint, and a plain unique
-- (organization_id) would allow exactly one project of any kind — the opposite of the rule.
create unique index automation_projects_one_default_uidx
    on automation_projects (organization_id) where is_default;

create table automation_project_members (
    project_id uuid        not null references automation_projects (id) on delete cascade,
    user_id    uuid        not null references users (id) on delete cascade,
    role       text        not null,
    added_by   uuid        references users (id) on delete set null,
    created_at timestamptz not null default now(),
    primary key (project_id, user_id),
    constraint automation_project_members_role_valid
        check (role in ('owner', 'editor', 'operator', 'viewer'))
);

-- "Which projects is this person in?" is asked on every screen that scopes, so it gets the index.
create index automation_project_members_user_idx on automation_project_members (user_id);

-- ── 1 of 5: one default project per organization that already has one ────────────────────────
--
-- An organization with no project has no place to put a resource, and the `not null` below
-- would make that a runtime failure on every insert path instead of a missing screen. Seeded in
-- the migration so a fresh install and an upgrade look identical from the first request.
insert into automation_projects (organization_id, key, name, description, is_default, status)
select o.id, 'DEFAULT', 'Default', 'Automations created before projects existed', true, 'active'
from organizations o;

-- ── 2 of 5: the nullable column, before anything depends on it ───────────────────────────────
--
-- `workflows` exists (0006). The other container tables the REQ names — `credentials`,
-- `workflow_folders`, `schedules` — are not on this branch: credentials ship with wave 7's
-- REQ-099 runtime and a schedule lives inside the workflow row itself here. Adding one later is
-- four lines copied from the pattern below (add the column, backfill by organization, set
-- `not null`, index it), and this order is the reason a later writer cannot get it wrong by
-- writing the constraint first.
alter table workflows
    add column project_id uuid references automation_projects (id) on delete restrict;

-- ── 3 of 5: backfill, one statement per table ───────────────────────────────────────────────
update workflows w
   set project_id = d.id
  from automation_projects d
 where d.organization_id = w.organization_id
   and d.is_default;

-- ── 4 of 5: only now is `not null` safe ─────────────────────────────────────────────────────
alter table workflows alter column project_id set not null;

-- The listing is project-scoped and reads newest-first; the composite serves both the list and
-- the switcher's counts without a second lookup.
create index workflows_project_updated_idx on workflows (project_id, updated_at desc);

-- The project audit screen is `audit_log` filtered by project. A project column no index covers
-- turns that filter into a sequential scan of the whole trail — on the one table that grows
-- without bound, and the one a screen exists to read.
alter table audit_log
    add column project_id uuid references automation_projects (id) on delete set null;
create index audit_log_project_created_idx on audit_log (project_id, created_at desc);
