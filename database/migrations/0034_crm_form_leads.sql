-- The CRM's half of "a submitted form becomes a lead" (docs/requests/REQ-051 slice 4 part
-- seven, REQ-117, REQ-064).
--
-- The producer is the **forms** module (REQ-064), which is not in this build yet. What ships
-- here is the consumer, and it is written against the event contract rather than against the
-- producer's tables, so the day the form builder lands it only has to emit the documented name.
--
-- Three tables:
--
-- * `crm_form_leads` is the **ingress ledger**: one row per `form.submitted` event the CRM has
--   read. The event id is the primary key, so a drain that is retried — or a second API process
--   reading the same bus — cannot turn one submission into two leads. This is the difference
--   between a lead pipeline and a lead *factory*.
-- * `crm_lead_settings` is the per-organization routing policy. A submission is not always a
--   deal: a support address and a "quote me" address are the same event with different meanings,
--   and which one is which is the operator's call, not the module's.
-- * The default row is seeded in the migration so a **fresh** installation already routes a
--   submission into the first open stage. An installation that upgrades and has never opened the
--   screen still gets the row on first write (the read path is upsert-shaped), because a feature
--   that only works after the operator has visited a settings page is a hidden feature.
--
-- Note on the org: the event carries an `organization_id` that is **nullable** (a submission can
-- arrive from a public endpoint on a site with no owning organization). A lead belongs to a
-- tenant or it belongs to nobody, so a `null` organization is recorded in the ledger and *not*
-- turned into a record — see `leads.rs`, which counts those as `orphaned` rather than dropping
-- them silently.

create table crm_lead_settings (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        not null unique references organizations (id) on delete cascade,
    -- `true` while a submission becomes a contact; an operator who only wants the audit trail
    -- can turn it off without turning off the deal.
    create_contact   boolean     not null default true,
    create_deal      boolean     not null default true,
    -- Which stage of the default pipeline a new deal lands in. `null` = the pipeline's first
    -- open stage, which is what `create_deal` already does when the caller says nothing.
    stage_id         uuid        references crm_pipeline_stages (id) on delete set null,
    -- The stage a *repeat* submission is parked in. A person who fills the form twice is not a
    -- second opportunity, they are the same one; defaulting to the first open stage would put two
    -- rows on the board for one interest.
    repeat_stage_id  uuid        references crm_pipeline_stages (id) on delete set null,
    -- Written to the contact's notes so a person reading the record learns where it came from
    -- without the CRM needing a foreign key into a module that may not exist.
    source_label     text        not null default 'form',
    -- A submission with no usable name is parked in the ledger as `rejected` rather than
    -- becoming a contact called "Unknown": the inbox screen has to be able to say why.
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    constraint crm_lead_settings_label_check
        check (source_label ~ '^[a-z0-9][a-z0-9._-]{0,63}$')
);

create table crm_form_leads (
    -- The bus identity of the submission. Primary key because exactly-once is the whole
    -- contract: the drain is retried on failure, and two API processes may read the same bus.
    event_id         bigint      primary key,
    -- Nullable, and deliberately so: a submission from a public endpoint on a site with no
    -- owning organization belongs to nobody. The column said `not null` while the file's own
    -- header said otherwise, so the orphan path -- the one that records such a submission --
    -- raised a not-null violation and every orphaned row came back as a drain failure.
    organization_id  uuid        references organizations (id) on delete cascade,
    site_id          uuid,
    form_id          uuid,
    form_key         text,
    -- What the person wrote, as the form delivered it. `payload` is the raw answers so a
    -- re-mapping later does not need the original event to still be in the bus.
    payload          jsonb       not null default '{}'::jsonb,
    -- The extracted identity. `email` is lowercased by the extractor, which is what makes the
    -- repeat check work across the casing a person types.
    contact_id       uuid        references crm_contacts (id) on delete set null,
    deal_id          uuid        references crm_deals (id) on delete set null,
    company_id       uuid        references crm_companies (id) on delete set null,
    email            text,
    -- `created` · `merged` (the person already existed) · `rejected` (nothing usable) ·
    -- `orphaned` (no organization to put it in) · `disabled` (the settings say no).
    outcome          text        not null,
    detail           text,
    occurred_at      timestamptz not null,
    created_at       timestamptz not null default now(),
    constraint crm_form_leads_outcome_check
        check (outcome in ('created', 'merged', 'rejected', 'orphaned', 'disabled'))
);

-- The inbox reads the newest first, and the repeat check reads one email per organization; both
-- are the queries the screen actually makes.
create index crm_form_leads_org_created_idx
    on crm_form_leads (organization_id, created_at desc);
create index crm_form_leads_org_email_idx
    on crm_form_leads (organization_id, email)
    where email is not null;

-- The drain's cursor. One row, the same shape the automation matcher uses
-- (`automation_cursor`), so a process that starts today does not replay the whole history of
-- the bus into a pipeline of leads. `last_event_id = 0` means "never advanced", which the
-- seeder turns into "start at the end of the bus".
create table crm_lead_cursor (
    id             integer     primary key default 1,
    last_event_id  bigint      not null default 0,
    updated_at     timestamptz not null default now(),
    constraint crm_lead_cursor_singleton_check check (id = 1)
);

insert into crm_lead_cursor (id, last_event_id)
values (1, (select coalesce(max(id), 0) from events));

-- Every organization that already has a pipeline gets its default routing row. The ones that do
-- not (a tenant created before the CRM was installed) get theirs the first time a submission
-- arrives, so the feature works without anyone visiting a screen.
insert into crm_lead_settings (organization_id)
select distinct p.organization_id
from crm_pipelines p;
