-- REQ-017 slice 1: staging environments and their clone jobs.
--
-- Staging is a content-and-configuration copy inside one installation, so this migration adds
-- environments and the content's `environment_id` and nothing else. It provisions no container,
-- no database and no hostname of its own — that separation is what keeps the request's "out of
-- scope" list honest.
--
-- Two corrections to the request's data model, both because the tables it names either do not
-- exist or are not owned the way it assumes, and both recorded in the REQ:
--
--   * `menus` and `site_settings` do not exist in this platform. Navigation is a content concern
--     with no table of its own; site configuration lives in `organization_settings`. Rather than
--     inventing two tables to satisfy a spec line, the environment boundary is placed on the rows
--     that carry the content: `pages`, `page_revisions` (through their page), `translations`,
--     `workflows` and `organization_settings`. A clone copies what the platform actually stores.
--   * `pages` has no `organization_id` — it reaches its organization through `sites`. The backfill
--     joins through that path, and a page whose site is missing cannot be attributed to an
--     environment at all, which is why the NOT NULL is applied only after the backfill and why
--     that backfill is a separate statement from the one that creates the environments.
--
-- The backfill runs in three ordered steps because each depends on the previous one:
--   1. every organization gets its production environment;
--   2. every existing content row is attached to it;
--   3. only then is `environment_id` made NOT NULL.

-- The one production environment per organization, plus any staging ones.
create table environments (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations(id) on delete cascade,
    key text not null,
    name text not null,
    type text not null check (type in ('production','staging')),
    status text not null default 'active' check (status in ('active','cloning','error','archived')),
    -- Where a staging environment was cloned from. `on delete set null` rather than cascade:
    -- archiving the source must not delete the copy taken from it.
    cloned_from_environment_id uuid references environments(id) on delete set null,
    cloned_at timestamptz,
    staging_host text,
    created_by uuid references users(id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint environments_key_format check (key ~ '^[a-z0-9]([a-z0-9-]{0,53}[a-z0-9])?$'),
    constraint environments_name_length check (length(btrim(name)) between 1 and 64),
    constraint environments_key_unique_per_org unique (organization_id, key)
);

-- Exactly one production environment per organization.
--
-- A partial unique index rather than a check constraint, because the invariant is "one row of this
-- type per organization", which a per-row constraint cannot express: `check (type <> 'production')`
-- would forbid the first one along with every later one.
create unique index environments_single_production
    on environments (organization_id) where type = 'production';

-- A staging host is shared infrastructure (a DNS name, a certificate), so two environments may
-- not claim the same one. Production rows carry a null host and sit outside the index.
create unique index environments_staging_host_unique
    on environments (staging_host) where staging_host is not null;

create index environments_organization_recent
    on environments (organization_id, created_at desc);

-- The clone's audit trail: the list screen's "Last clone" column and the Overview tab's step log.
create table environment_clone_jobs (
    id uuid primary key default gen_random_uuid(),
    environment_id uuid not null references environments(id) on delete cascade,
    status text not null default 'pending' check (status in ('pending','running','done','failed','cancelled')),
    areas text[] not null,
    -- The runner fills `items_total` as it counts each area and `items_done` as it copies. A row
    -- at 0/0 is a job that has not started, not a job that copied nothing; the progress bar
    -- renders the two differently, which is why they are two columns and not a nullable pair.
    items_total integer not null default 0 check (items_total >= 0),
    items_done integer not null default 0 check (items_done >= 0 and items_done <= items_total),
    -- Per-area counts, so the Overview tab can name the area a failure stopped at without
    -- re-counting anything.
    area_counts jsonb not null default '{}'::jsonb,
    exclude_archived boolean not null default false,
    error text,
    started_at timestamptz,
    finished_at timestamptz,
    created_by uuid references users(id) on delete set null,
    created_at timestamptz not null default now()
);

create index environment_clone_jobs_recent
    on environment_clone_jobs (environment_id, created_at desc);

-- One open clone per environment. This is what makes `clone_already_running` a real answer rather
-- than a race: two concurrent requests for the same environment cannot both insert a job, because
-- only one row may be pending or running. The uniqueness is enforced by the database rather than
-- by a check-then-insert in the route, which is the only version that survives two requests at
-- once.
create unique index environment_clone_jobs_single_open
    on environment_clone_jobs (environment_id) where status in ('pending','running');

-- Step 1: the production environment of every organization.
insert into environments (organization_id, key, name, type, status)
select o.id, 'production', 'Production', 'production', 'active'
from organizations o
on conflict (organization_id, key) do nothing;

-- Step 2: attach existing content.
--
-- `pages` reaches its organization through `sites`. A page whose site row is absent would resolve
-- to NULL, and the NOT NULL below would then fail the migration with a constraint error naming
-- `environment_id` rather than the orphaned page — so the join is inner and the orphan case is a
-- migration failure on purpose: a page with no site is a broken installation, not a page to
-- silently drop out of the environment model.
alter table pages add column environment_id uuid references environments(id) on delete cascade;
update pages p
set environment_id = e.id
from sites s
join environments e on e.organization_id = s.organization_id and e.type = 'production'
where p.site_id = s.id and p.environment_id is null;

-- Translations are organization-scoped rows, so they resolve directly.
alter table translations add column environment_id uuid references environments(id) on delete cascade;
update translations t
set environment_id = e.id
from environments e
where e.organization_id = t.organization_id and e.type = 'production'
  and t.environment_id is null;

-- Workflows are organization-scoped too, and carry a nullable site: an organization-wide workflow
-- is cloned with the organization, a site-scoped one with its site.
alter table workflows add column environment_id uuid references environments(id) on delete cascade;
update workflows w
set environment_id = e.id
from environments e
where e.organization_id = w.organization_id and e.type = 'production'
  and w.environment_id is null;

-- Site configuration. `organization_settings` is the settings table this platform actually has;
-- the request's `site_settings` does not exist and is not created here, because inventing a table
-- to satisfy a spec line leaves an empty table behind and a second, competing home for the same
-- configuration.
alter table organization_settings add column environment_id uuid references environments(id) on delete cascade;
update organization_settings s
set environment_id = e.id
from environments e
where e.organization_id = s.organization_id and e.type = 'production'
  and s.environment_id is null;

-- Step 3: the invariant, once the backfill has satisfied it.
alter table pages alter column environment_id set not null;
alter table translations alter column environment_id set not null;
alter table workflows alter column environment_id set not null;
alter table organization_settings alter column environment_id set not null;

-- The listing queries: an environment's content, and its most recently touched content.
create index pages_by_environment on pages (environment_id, site_id);
create index pages_by_environment_recent on pages (environment_id, updated_at desc);
create index translations_by_environment on translations (environment_id, resource_id);
create index workflows_by_environment on workflows (environment_id, site_id);
create index organization_settings_by_environment on organization_settings (environment_id);
