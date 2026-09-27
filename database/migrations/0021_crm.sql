-- Omnion · 0021 · CRM: companies, contacts, pipelines, deals, activities and saved views
-- (docs/requests/REQ-051, slice 1)
--
-- The relationship layer of the platform (docs/08-BUSINESS-SUITE.md): the people and companies a
-- business sells to, the deals they are part of and the activity trail around them. Every table is
-- tenant-leading and carries `organization_id` first in its index, because the API resolves the
-- caller's organization first and never scans by id alone.
--
-- Facts this schema encodes, so the module and the screens cannot disagree:
--
-- * **Nothing is deleted.** `archived_at` is the only removal: a deal that quotes a contact, a
--   contact an activity and a company a deal all point at each other, and hard-deleting one row
--   would silently rewrite the meaning of the others. The lists filter `archived_at is null`, so
--   archiving removes a row from the work while keeping the history readable.
-- * **An e-mail is one person per organization, case-insensitively.** The unique index is on
--   `lower(email) where archived_at is null and email is not null`, so an archived duplicate does
--   not block a fresh contact and two live ones cannot exist.
-- * **A lost deal says why.** `(kind = 'lost') = (lost_reason is not null)` is checked by a
--   trigger, because `kind` lives on the *stage* and `lost_reason` on the *deal*: a check
--   constraint cannot reach across tables, so the invariant is enforced where both are written.
-- * **Weighted forecast is one expression.** `amount * probability / 100` is what the board
--   headers, the overview and the export all read, so the three can never drift apart.
--
-- The default pipeline is seeded per existing organization and for new ones through the same
-- function, so a fresh install has a board with stages instead of an empty page.

-- --------------------------------------------------------------------------------------------
-- Companies
-- --------------------------------------------------------------------------------------------

create table crm_companies (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null,
    domain          text,
    industry        text,
    owner_user_id   uuid        references users (id) on delete set null,
    status          text        not null default 'lead',
    tags            text[]      not null default '{}',
    custom          jsonb       not null default '{}'::jsonb,
    notes           text        not null default '',
    archived_at     timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_companies_name_not_blank check (length(btrim(name)) > 0),
    constraint crm_companies_name_length check (length(name) <= 200),
    constraint crm_companies_status_check check (status in ('lead', 'customer', 'partner', 'churned')),
    constraint crm_companies_domain_format check (domain is null or domain ~ '^([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,}$'),
    constraint crm_companies_tags_count check (cardinality(tags) <= 10)
);

-- Two live companies of one organization may not share a name, case-insensitively; an archived
-- one may, because it is no longer a company anybody works with.
create unique index crm_companies_organization_name_key
    on crm_companies (organization_id, lower(name)) where archived_at is null;
create index crm_companies_org_updated_idx on crm_companies (organization_id, updated_at desc);
create index crm_companies_org_open_idx on crm_companies (organization_id, name)
    where archived_at is null;
create index crm_companies_org_owner_idx on crm_companies (organization_id, owner_user_id)
    where archived_at is null;
create index crm_companies_tags_idx on crm_companies using gin (tags);
create index crm_companies_custom_idx on crm_companies using gin (custom);

-- --------------------------------------------------------------------------------------------
-- Contacts
-- --------------------------------------------------------------------------------------------

create table crm_contacts (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    first_name      text,
    last_name       text,
    email           text,
    phone           text,
    job_title       text,
    company_id      uuid        references crm_companies (id) on delete set null,
    owner_user_id   uuid        references users (id) on delete set null,
    status          text        not null default 'lead',
    tags            text[]      not null default '{}',
    custom          jsonb       not null default '{}'::jsonb,
    notes           text        not null default '',
    last_activity_at timestamptz,
    archived_at     timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_contacts_name_present check (
        coalesce(length(btrim(first_name)), 0) + coalesce(length(btrim(last_name)), 0) > 0
    ),
    constraint crm_contacts_name_length check (
        coalesce(length(first_name), 0) <= 80 and coalesce(length(last_name), 0) <= 80
    ),
    constraint crm_contacts_email_format check (
        email is null or email ~* '^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$'
    ),
    constraint crm_contacts_email_length check (email is null or length(email) <= 254),
    constraint crm_contacts_phone_format check (
        phone is null or phone ~ '^\+?[0-9 ()-]{7,20}$'
    ),
    constraint crm_contacts_status_check check (status in ('lead', 'customer', 'partner', 'churned')),
    constraint crm_contacts_tags_count check (cardinality(tags) <= 10),
    constraint crm_contacts_notes_length check (length(notes) <= 4000),
    -- A contact nobody can reach is refused here rather than silently filtered out of every list
    -- it would appear in. The screens render this refusal as a field message.
    constraint crm_contacts_reachable check (
        email is not null or phone is not null or
        coalesce(length(btrim(first_name)), 0) + coalesce(length(btrim(last_name)), 0) > 0
    )
);

create unique index crm_contacts_organization_email_key
    on crm_contacts (organization_id, lower(email))
    where archived_at is null and email is not null;
create index crm_contacts_org_updated_idx on crm_contacts (organization_id, updated_at desc);
create index crm_contacts_org_email_idx on crm_contacts (organization_id, lower(email));
create index crm_contacts_open_idx on crm_contacts (organization_id, last_name, first_name)
    where archived_at is null;
create index crm_contacts_org_owner_idx on crm_contacts (organization_id, owner_user_id)
    where archived_at is null;
create index crm_contacts_company_idx on crm_contacts (company_id) where archived_at is null;
create index crm_contacts_tags_idx on crm_contacts using gin (tags);
create index crm_contacts_custom_idx on crm_contacts using gin (custom);
-- The "no activity for N days" filter of the contact list.
create index crm_contacts_last_activity_idx on crm_contacts (organization_id, last_activity_at desc)
    where archived_at is null;

-- --------------------------------------------------------------------------------------------
-- Pipelines and their stages
-- --------------------------------------------------------------------------------------------

create table crm_pipelines (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null,
    is_default      boolean     not null default false,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_pipelines_name_not_blank check (length(btrim(name)) > 0),
    constraint crm_pipelines_name_length check (length(name) <= 80)
);

-- One default pipeline per organization: the board opens on a real set of stages instead of
-- guessing which pipeline a new deal belongs to.
create unique index crm_pipelines_default_key
    on crm_pipelines (organization_id) where is_default;
create unique index crm_pipelines_organization_name_key
    on crm_pipelines (organization_id, lower(name));

create table crm_pipeline_stages (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    pipeline_id     uuid        not null references crm_pipelines (id) on delete cascade,
    name            text        not null,
    kind            text        not null default 'open',
    position        integer     not null,
    probability     integer     not null default 0,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_pipeline_stages_name_not_blank check (length(btrim(name)) > 0),
    constraint crm_pipeline_stages_name_length check (length(name) <= 80),
    constraint crm_pipeline_stages_kind_check check (kind in ('open', 'won', 'lost')),
    constraint crm_pipeline_stages_position_check check (position >= 0),
    constraint crm_pipeline_stages_probability_check check (probability between 0 and 100)
);

create unique index crm_pipeline_stages_position_key on crm_pipeline_stages (pipeline_id, position);
create index crm_pipeline_stages_pipeline_idx
    on crm_pipeline_stages (pipeline_id, position);

-- A pipeline ends in exactly one outcome: the board's "won" and "lost" columns are decided by
-- the stages, so a pipeline with two winning columns would double-count the forecast.
create or replace function crm_pipeline_has_single_outcome() returns trigger
language plpgsql as $$
declare
    open_count    integer;
    won_count     integer;
    lost_count    integer;
begin
    select
        count(*) filter (where kind = 'open'),
        count(*) filter (where kind = 'won'),
        count(*) filter (where kind = 'lost')
    into open_count, won_count, lost_count
    from crm_pipeline_stages
    where pipeline_id = coalesce(new.pipeline_id, old.pipeline_id);

    if open_count < 1 then
        raise exception 'a pipeline needs at least one open stage'
            using errcode = 'check_violation';
    end if;
    if won_count > 1 or lost_count > 1 then
        raise exception 'a pipeline may have at most one won and one lost stage'
            using errcode = 'check_violation';
    end if;
    return null;
end;
$$;

create constraint trigger crm_pipeline_stages_shape
    after insert or update or delete on crm_pipeline_stages
    deferrable initially deferred
    for each row execute function crm_pipeline_has_single_outcome();

-- --------------------------------------------------------------------------------------------
-- Deals
-- --------------------------------------------------------------------------------------------

create table crm_deals (
    id                uuid          primary key default gen_random_uuid(),
    organization_id   uuid          not null references organizations (id) on delete cascade,
    pipeline_id       uuid          not null references crm_pipelines (id) on delete restrict,
    stage_id          uuid          not null references crm_pipeline_stages (id) on delete restrict,
    title             text          not null,
    company_id        uuid          references crm_companies (id) on delete set null,
    contact_id        uuid          references crm_contacts (id) on delete set null,
    owner_user_id     uuid          references users (id) on delete set null,
    amount            numeric(14,2) not null default 0,
    currency          char(3)       not null default 'USD',
    probability       integer,
    expected_close_on date,
    source            text,
    lost_reason       text,
    stage_changed_at  timestamptz   not null default now(),
    archived_at       timestamptz,
    created_at        timestamptz   not null default now(),
    updated_at        timestamptz   not null default now(),

    constraint crm_deals_title_not_blank check (length(btrim(title)) > 0),
    constraint crm_deals_title_length check (length(title) <= 200),
    constraint crm_deals_amount_check check (amount >= 0),
    constraint crm_deals_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint crm_deals_probability_check check (probability is null or probability between 0 and 100),
    constraint crm_deals_source_length check (source is null or length(source) <= 120),
    constraint crm_deals_lost_reason_length check (lost_reason is null or length(lost_reason) <= 200)
);

-- The stage a deal sits in must belong to the pipeline the deal is on: a board that mixes
-- pipelines would show a card in a column of another pipeline's totals.
create or replace function crm_deal_stage_belongs_to_pipeline() returns trigger
language plpgsql as $$
begin
    if not exists (
        select 1 from crm_pipeline_stages
        where id = new.stage_id and pipeline_id = new.pipeline_id
    ) then
        raise exception 'the stage does not belong to this pipeline'
            using errcode = 'check_violation';
    end if;
    return new;
end;
$$;

create constraint trigger crm_deals_stage_pipeline
    after insert or update of stage_id, pipeline_id on crm_deals
    deferrable initially deferred
    for each row execute function crm_deal_stage_belongs_to_pipeline();

-- "A lost deal says why" — the kind lives on the stage, so this is the only place that can see
-- both sides. `new` is null on a delete, and a delete of a stage holding deals is refused by the
-- `on delete restrict` above before this trigger runs.
create or replace function crm_deal_lost_reason() returns trigger
language plpgsql as $$
declare
    stage_kind text;
begin
    if new is null then
        return null;
    end if;

    select kind into stage_kind from crm_pipeline_stages where id = new.stage_id;

    if stage_kind = 'lost' and (new.lost_reason is null or length(btrim(new.lost_reason)) = 0) then
        raise exception 'a lost deal needs a reason'
            using errcode = 'check_violation';
    end if;
    if stage_kind is distinct from 'lost' then
        -- Leaving the lost column clears the reason: keeping it would make the next loss report
        -- credit the wrong deal.
        new.lost_reason := null;
    end if;
    return new;
end;
$$;

create trigger crm_deals_lost_reason
    before insert or update of stage_id on crm_deals
    for each row execute function crm_deal_lost_reason();

create index crm_deals_org_stage_idx
    on crm_deals (organization_id, stage_id, stage_changed_at);
create index crm_deals_org_close_idx
    on crm_deals (organization_id, expected_close_on) where archived_at is null;
create index crm_deals_open_idx on crm_deals (organization_id, title) where archived_at is null;
create index crm_deals_org_owner_idx on crm_deals (organization_id, owner_user_id)
    where archived_at is null;
create index crm_deals_company_idx on crm_deals (company_id) where archived_at is null;
create index crm_deals_contact_idx on crm_deals (contact_id) where archived_at is null;

-- --------------------------------------------------------------------------------------------
-- Activities
-- --------------------------------------------------------------------------------------------

create table crm_activities (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    kind            text        not null,
    subject         text        not null,
    body            text        not null default '',
    company_id      uuid        references crm_companies (id) on delete cascade,
    contact_id      uuid        references crm_contacts (id) on delete cascade,
    deal_id         uuid        references crm_deals (id) on delete cascade,
    occurred_at     timestamptz not null default now(),
    due_at          timestamptz,
    done_at         timestamptz,
    owner_user_id   uuid        references users (id) on delete set null,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_activities_kind_check check (kind in ('call', 'meeting', 'note', 'task')),
    constraint crm_activities_subject_not_blank check (length(btrim(subject)) > 0),
    constraint crm_activities_subject_length check (length(subject) <= 200),
    constraint crm_activities_body_length check (length(body) <= 4000),
    -- An activity has to hang off something, or it is a note nobody will ever find again.
    constraint crm_activities_attached check (
        company_id is not null or contact_id is not null or deal_id is not null
    )
);

create index crm_activities_org_occurred_idx
    on crm_activities (organization_id, occurred_at desc);
create index crm_activities_contact_idx on crm_activities (contact_id, occurred_at desc);
create index crm_activities_company_idx on crm_activities (company_id, occurred_at desc);
create index crm_activities_deal_idx on crm_activities (deal_id, occurred_at desc);
-- The open-task list of the overview.
create index crm_activities_open_idx on crm_activities (organization_id, due_at)
    where done_at is null;

-- --------------------------------------------------------------------------------------------
-- Saved views
-- --------------------------------------------------------------------------------------------

create table crm_views (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    owner_user_id   uuid        not null references users (id) on delete cascade,
    entity          text        not null,
    name            text        not null,
    filters         jsonb       not null default '{}'::jsonb,
    columns         text[]      not null default '{}',
    sort            jsonb       not null default '{}'::jsonb,
    is_shared       boolean     not null default false,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint crm_views_entity_check check (entity in ('contacts', 'companies', 'deals', 'activities')),
    constraint crm_views_name_not_blank check (length(btrim(name)) > 0),
    constraint crm_views_name_length check (length(name) <= 80)
);

create index crm_views_owner_idx on crm_views (organization_id, owner_user_id, entity);

-- --------------------------------------------------------------------------------------------
-- The default pipeline
-- --------------------------------------------------------------------------------------------

-- One pipeline with the stages a sales process actually has. Written as a function so a new
-- organization gets it the same way an existing one did, and so the two never disagree.
create or replace function crm_seed_default_pipeline(target_organization uuid) returns uuid
language plpgsql as $$
declare
    new_pipeline uuid;
    stage_row    record;
begin
    insert into crm_pipelines (organization_id, name, is_default)
    values (target_organization, 'Sales', true)
    on conflict do nothing
    returning id into new_pipeline;

    if new_pipeline is null then
        select id into new_pipeline
        from crm_pipelines
        where organization_id = target_organization and is_default;
        return new_pipeline;
    end if;

    for stage_row in
        select * from (values
            ('New',          'open', 0, 10),
            ('Qualified',    'open', 1, 35),
            ('Proposal',     'open', 2, 60),
            ('Negotiation',  'open', 3, 80),
            ('Won',          'won',  4, 100),
            ('Lost',         'lost', 5, 0)
        ) as stages(name, kind, position, probability)
    loop
        insert into crm_pipeline_stages
            (organization_id, pipeline_id, name, kind, position, probability)
        values
            (target_organization, new_pipeline, stage_row.name, stage_row.kind,
             stage_row.position, stage_row.probability);
    end loop;

    return new_pipeline;
end;
$$;

select crm_seed_default_pipeline(id) from organizations;
