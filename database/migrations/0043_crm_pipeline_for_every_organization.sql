-- A new organization gets a sales pipeline the moment it is created (REQ-051, deals/board).
--
-- `0022_crm.sql` already had the right idea — `crm_seed_default_pipeline(organization)` builds a
-- pipeline and its six stages — but it was called exactly once, in the same statement that created
-- the function, and never again. Every organization created *after* the migration therefore owns
-- no pipeline at all, and `GET /api/v1/crm/deals?view=board` answers `404 NotFound("pipeline")`
-- for it: the board screen is dead on any tenant created through the product, and the only reason
-- the CRM integration suite never noticed is that its fixture calls the seed function by hand
-- (`apps/api/tests/crm.rs`). A test that seeds the world the product does not seed is a test of
-- the fixture.
--
-- The fix is in the database rather than in a Rust call site, on purpose:
--
--   * `organizations` is written by several unrelated paths — `identity::create_organization` from
--     the tenancy route, the onboarding steps, SCIM, provisioning — and a rule that has to be
--     remembered at each of them is a rule that will be forgotten at the fifth.
--   * A trigger is the one place that observes the row, so a pipeline exists for every
--     organization whether it arrived through the panel, an import or a test fixture.
--
-- The trigger is deliberately AFTER INSERT and fires per row, so a bulk insert of organizations
-- seeds each one. It is also written to stay silent when the pipeline already exists: the
-- function's `on conflict do nothing` branch returns the existing id, so a retrying insert cannot
-- produce a second pipeline or an error the insert did not cause.

create or replace function crm_pipeline_for_new_organization() returns trigger
language plpgsql as $$
begin
    perform crm_seed_default_pipeline(new.id);
    return new;
end;
$$;

drop trigger if exists crm_pipeline_on_organization_insert on organizations;
create trigger crm_pipeline_on_organization_insert
    after insert on organizations
    for each row execute function crm_pipeline_for_new_organization();

-- Repair the organizations that already exist and were created without a pipeline. This is
-- written as a repair rather than left to the next read for two reasons: a backfill keeps the
-- board working for a tenant created before this migration without anyone opening the deals
-- screen first, and it is the same call the trigger makes, so the two cannot disagree about what
-- a seeded pipeline looks like.
select crm_seed_default_pipeline(id)
from organizations
where not exists (
    select 1 from crm_pipelines p where p.organization_id = organizations.id
);
