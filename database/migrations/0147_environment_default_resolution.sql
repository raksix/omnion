-- REQ-017 slice 1 follow-up: give every content write an environment without asking the caller.
--
-- Migration 0145 made `environment_id` NOT NULL on `pages`, `translations`, `workflows` and
-- `organization_settings` so that "which environment does this row belong to" could never be
-- null. That is the right invariant and it is the wrong *moment*: at the time it was applied,
-- every existing write path in the platform inserted into those tables without naming an
-- environment, so the first page created after the migration would fail on a not-null violation
-- and the error would name `environment_id` rather than the missing decision behind it.
--
-- The two obvious repairs are both wrong:
--
--   * a column default cannot express the answer, because the environment is reached through a
--     different row (`pages.site_id -> sites.organization_id -> environments`). A default is a
--     constant, and the constant would be a *fixed* environment — every tenant's content
--     landing in whichever organization happened to be created first. That is a cross-tenant
--     data leak, and it is silent.
--   * editing every call site to pass an environment is a change to another wave's crate
--     (`crates/content` belongs to wave 2b) and to the seed paths of six other writers, none of
--     which can be coordinated from here. A migration that requires six branches to change in
--     lockstep is a migration that gets merged to main and breaks five worktrees.
--
-- A `before insert` trigger resolves the environment from the row's own organization instead:
-- an explicit `environment_id` is always honoured (that is how a clone writes its rows and how
-- a staging write names its environment), and a row that names none lands in the organization's
-- production environment. The NOT NULL invariant stays exactly as strict as 0145 left it — the
-- trigger cannot leave a row unresolved, because an organization with no production environment
-- is a corrupt state and the row is refused rather than guessed into one.
--
-- The trigger is `before insert` rather than `before insert or update` on purpose: a row that
-- already carries an environment never re-resolves it, so an update cannot silently move a
-- staging row into production. Promotion is the only thing that moves rows between environments,
-- and it does that explicitly.

-- Every organization gets its production environment the moment it exists.
--
-- Migration 0145 backfilled the organizations that were already in the table, which is the right
-- thing for a migration and the wrong thing for the product: `organizations::create_organization`
-- inserts a row and nothing else, so a tenant created *after* the migration would have no
-- production environment, no staging, and — because the page-resolution trigger below refuses a
-- row whose organization has none — a site whose first page cannot be created at all. A staging
-- feature that breaks page creation for every new tenant is worse than no staging feature.
--
-- The trigger makes the invariant "one production environment per organization" hold for the
-- lifetime of the row rather than for the moment the migration ran. The partial unique index
-- still owns the "exactly one" half, so a hand-written second production row is still refused by
-- the database; this only guarantees the first one exists.
create or replace function omnion_create_production_environment()
returns trigger
language plpgsql
as $$
begin
    insert into environments (organization_id, key, name, type, status)
    values (new.id, 'production', 'Production', 'production', 'active')
    on conflict (organization_id, key) do nothing;
    return new;
end;
$$;

create trigger organizations_create_production_environment
    after insert on organizations
    for each row execute function omnion_create_production_environment();

-- The existing organizations get theirs now, so the trigger and the backfill agree on every row.
insert into environments (organization_id, key, name, type, status)
select o.id, 'production', 'Production', 'production', 'active'
from organizations o
on conflict (organization_id, key) do nothing;

-- The `pages` trigger is defined below; these are the rest.

-- The organization a row reaches its environment through, per table.
--
-- `pages` is the awkward one: it has no `organization_id` of its own and resolves through its
-- site. `translations`, `workflows` and `organization_settings` are organization-scoped rows and
-- name it directly.

create or replace function omnion_default_environment_for_page()
returns trigger
language plpgsql
as $$
begin
    if new.environment_id is null then
        select e.id into new.environment_id
        from sites s
        join environments e
          on e.organization_id = s.organization_id and e.type = 'production'
        where s.id = new.site_id;

        -- No production environment for this site means the organization has none at all,
        -- which 0145's backfill should have prevented. Raising here is deliberate: the NOT
        -- NULL would refuse the row anyway, and a named error says which invariant broke.
        if new.environment_id is null then
            raise exception 'site % has no production environment', new.site_id
                using errcode = 'integrity_constraint_violation';
        end if;
    end if;
    return new;
end;
$$;

create or replace function omnion_default_environment_for_org_row()
returns trigger
language plpgsql
as $$
begin
    if new.environment_id is null then
        select e.id into new.environment_id
        from environments e
        where e.organization_id = new.organization_id and e.type = 'production';

        if new.environment_id is null then
            raise exception 'organization % has no production environment', new.organization_id
                using errcode = 'integrity_constraint_violation';
        end if;
    end if;
    return new;
end;
$$;

create trigger pages_default_environment
    before insert on pages
    for each row execute function omnion_default_environment_for_page();

create trigger translations_default_environment
    before insert on translations
    for each row execute function omnion_default_environment_for_org_row();

create trigger workflows_default_environment
    before insert on workflows
    for each row execute function omnion_default_environment_for_org_row();

create trigger organization_settings_default_environment
    before insert on organization_settings
    for each row execute function omnion_default_environment_for_org_row();
