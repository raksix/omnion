-- 0051_crm_lead_intake.sql — the intake sources, the leads they produce and the trail.
--
-- REQ-117, slice 1 (capture and inbox). The documented flow starts outside the CRM: a form on
-- somebody's website is submitted, and *that* is where a lead comes from. This migration is the
-- landing place — the sources that bind a capture surface to a pipeline, the lead row itself
-- (with its raw payload, its attribution and its dedupe verdict) and the event trail the lead
-- detail renders.
--
-- Five choices are worth stating, because each is a place the obvious table is wrong.
--
-- 1. **A rejected submission is a row too.** `status` carries `spam` and `rejected`, and the
--    row keeps `rejection_reason` and `spam_score`. The alternative — dropping it — means the
--    inbox can never show an operator what was discarded, so a form that starts rejecting
--    everything looks exactly like a form nobody submitted.
--
-- 2. **`crm_lead_events` is a separate table rather than an audit row.** The audit trail is
--    for privileged work and is written by the API layer; a lead's assignment history has to be
--    readable by the same query that reads the lead, with the actor and the reason in one row,
--    so it is its own append-only table. Nothing here ever updates or deletes an event.
--
-- 3. **The status and decision vocabularies are check constraints *and* Rust lists**, and
--    `modules/crm-intake/src/vocabulary.rs` carries the test that names this file. Same trap
--    as `0050_notifications.sql`: a status the panel offers and the database refuses is a
--    filter that silently returns nothing, and a status the database accepts and the panel
--    cannot render is a row with no way to see it.
--
-- 4. **`payload` is bounded by a check, not by the writer.** `crm_leads_payload_bytes_check`
--    refuses anything over 256 KiB, so an over-large submission is *rejected* rather than
--    silently truncated: a truncated payload produces a lead whose fields do not match what
--    the visitor actually sent, which is worse than a visible refusal.
--
-- 5. **The endpoint key is stored hashed and the clear key is shown once.** A source that
--    stores its own key can be read back by anyone with database access and can be replayed
--    forever; `endpoint_key_hash` is what authenticates the intake call, and
--    `endpoint_key_hint` (the last four characters) is what the panel shows afterwards so an
--    operator can tell *which* key is live without the key ever being recoverable.

create table crm_intake_sources (
    id                  uuid        primary key default gen_random_uuid(),
    organization_id     uuid        not null references organizations (id) on delete cascade,
    site_id             uuid        references sites (id) on delete set null,
    name                text        not null,
    -- `form` binds a REQ-064 form by key, `endpoint` is the hand-written/keyed inbound URL,
    -- `import` is a manual paste. The three differ only in what a submission must carry.
    kind                text        not null,
    form_key            text,
    -- SHA-256 of the issued key. The clear key is returned exactly once, at creation and at
    -- rotation; it is never stored, never logged and never recoverable.
    endpoint_key_hash   text,
    endpoint_key_hint   text,
    -- Ordered list of {target, source_key, transform, required, fallback}.
    mapping             jsonb       not null default '[]'::jsonb,
    required_targets    text[]      not null default '{}',
    consent_required    boolean     not null default true,
    consent_text        text,
    dedupe_policy       text        not null default 'link',
    pipeline_id         uuid,
    stage_id            uuid,
    auto_tags           text[]      not null default '{}',
    autoresponder       jsonb       not null default '{}'::jsonb,
    active              boolean     not null default true,
    rate_limit_per_hour integer     not null default 30,
    last_received_at    timestamptz,
    last_error          text,
    -- Source keys the mapping expects that the bound form no longer has. Set by the health
    -- check so the editor can say *which* key broke instead of writing a lead with empty
    -- fields.
    broken_mappings     text[]      not null default '{}',
    created_by          uuid        references users (id) on delete set null,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint crm_intake_sources_kind_check
        check (kind in ('form', 'endpoint', 'import')),
    constraint crm_intake_sources_dedupe_policy_check
        check (dedupe_policy in ('link', 'create_anyway', 'reject_duplicate')),
    constraint crm_intake_sources_rate_limit_check
        check (rate_limit_per_hour between 1 and 10000),
    constraint crm_intake_sources_organization_name_key unique (organization_id, name)
);

-- One form feeds one source. Partial, because most sources are keyed endpoints with no form
-- and the unique constraint would otherwise collide on the nulls.
create unique index crm_intake_sources_form_key
    on crm_intake_sources (organization_id, form_key) where form_key is not null;

create index crm_intake_sources_organization_idx
    on crm_intake_sources (organization_id, name);

create table crm_leads (
    id                   uuid        primary key default gen_random_uuid(),
    organization_id      uuid        not null references organizations (id) on delete cascade,
    site_id              uuid        references sites (id) on delete set null,
    source_id            uuid        references crm_intake_sources (id) on delete set null,
    status               text        not null default 'new',
    contact_id           uuid,
    company_id           uuid,
    deal_id              uuid,
    quote_id             uuid,
    owner_user_id        uuid        references users (id) on delete set null,
    first_name           text,
    last_name            text,
    email                text,
    phone                text,
    company_name         text,
    job_title            text,
    product_interest     text,
    message              text,
    -- The consent text *as accepted*, quoted. "The visitor agreed to be contacted" and "the
    -- visitor agreed to these words" are different claims and only the second is provable.
    consent_text         text,
    consent_given        boolean     not null default false,
    utm_source           text,
    utm_medium           text,
    utm_campaign         text,
    utm_term             text,
    utm_content          text,
    click_id             text,
    referrer_host        text,
    landing_path         text,
    source_path          text,
    -- Every answer as submitted, bounded by the check below.
    payload              jsonb       not null default '{}'::jsonb,
    payload_bytes        integer     not null default 0,
    -- The normalized key that produced the dedupe verdict, stored so the duplicate queue can
    -- be rebuilt without re-scanning the contact table.
    dedupe_key           text,
    duplicate_of         uuid        references crm_leads (id) on delete set null,
    decision             text,
    assignment_rule_id   uuid,
    assignment_reason    text,
    sla_policy_id        uuid,
    first_response_due_at timestamptz,
    first_response_at    timestamptz,
    escalated_at         timestamptz,
    spam_score           integer     not null default 0,
    rejection_reason     text,
    received_at          timestamptz not null default now(),
    converted_at         timestamptz,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    constraint crm_leads_status_check
        check (status in ('new', 'assigned', 'contacted', 'qualified', 'converted', 'duplicate', 'spam', 'rejected')),
    constraint crm_leads_decision_check
        check (decision is null or decision in ('linked', 'created', 'duplicate', 'rejected', 'spam')),
    -- A lead with neither e-mail nor phone cannot be contacted, so it is not a lead. The
    -- check names the failure: "missing both e-mail and phone" is the message the visitor's
    -- form has to render, and it is a check rather than a handler because a handler can be
    -- routed around and a constraint cannot.
    constraint crm_leads_contactable_check
        check (coalesce(email, '') <> '' or coalesce(phone, '') <> ''),
    constraint crm_leads_payload_bytes_check
        check (payload_bytes >= 0 and payload_bytes <= 262144),
    constraint crm_leads_spam_score_check
        check (spam_score >= 0 and spam_score <= 100)
);

-- The inbox read: organization, then status, then newest. Leads lead with the organization
-- because every read is organization-scoped.
create index crm_leads_inbox_idx on crm_leads (organization_id, status, received_at desc, id desc);

-- The SLA sweep. Partial on the rows that are still waiting for a first response, so the
-- query does not walk every lead ever received on every tick.
create index crm_leads_sla_idx on crm_leads (organization_id, first_response_due_at)
    where first_response_at is null and status not in ('spam', 'rejected', 'duplicate');

create index crm_leads_dedupe_idx on crm_leads (organization_id, dedupe_key)
    where dedupe_key is not null;

create index crm_leads_email_idx on crm_leads (organization_id, lower(email))
    where email is not null;

-- The phone match strips formatting, so the index strips it too: a match that has to read
-- the whole table to find "same digits, different punctuation" is a match that silently stops
-- matching as the table grows.
create index crm_leads_phone_idx on crm_leads
    (organization_id, (regexp_replace(phone, '[^0-9]', '', 'g')))
    where phone is not null;

-- The trail, newest first. Every read of a lead's history is this index and nothing else.
create table crm_lead_events (
    id             bigint      generated always as identity primary key,
    lead_id        uuid        not null references crm_leads (id) on delete cascade,
    kind           text        not null,
    actor_user_id  uuid        references users (id) on delete set null,
    detail         jsonb       not null default '{}'::jsonb,
    created_at     timestamptz not null default now()
);

create index crm_lead_events_lead_idx on crm_lead_events (lead_id, created_at desc, id desc);

-- Slice 1 files every lead as `new`; the SLA and assignment tables arrive with slice 2
-- (`crm_assignment_rules`, `crm_sla_policies`) because a policy seeded here would be a policy
-- no worker reads, and an operator looking at a seeded default would reasonably read it as
-- "the clock is running" when nothing is watching it yet. The `sla_policy_id`,
-- `assignment_rule_id` and `assignment_reason` columns exist from the start, so slice 2 is a
-- migration of *rows*, not a migration of the lead table.
