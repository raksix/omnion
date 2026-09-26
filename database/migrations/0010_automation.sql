-- Omnion · 0010 · automation: event triggers, conditions and revision comments
--
-- v0 of the automation layer (docs/requests/REQ-003, phase P13) on top of the P09 step engine.
-- An automation is a workflow whose trigger is a recorded platform event: the `workflows` row
-- gains the event it listens for (`trigger_event`) and the conditions its payload must satisfy
-- (`conditions`). Nothing else about the engine changes — a match starts a run, and the run is
-- the same durable set of step rows the engine has been advancing since P09.
--
-- Reads and writes this migration adds:
--   * `workflows.trigger_event` — the event name an event-triggered workflow listens for;
--   * `workflows.conditions` — the ordered conditions evaluated against the event payload;
--   * `workflows.last_triggered_at` / `trigger_count` — what the matcher leaves behind;
--   * `automation_cursor` — the single row that says which events have been evaluated, so a
--     restart resumes instead of replaying the whole bus;
--   * `page_revision_comments` — the annotation the `comment_revision` action writes.
--
-- Released migrations are append-only (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- workflows: an event trigger carries the event it listens for, and conditions belong to it
-- ---------------------------------------------------------------------------------------------

alter table workflows drop constraint workflows_trigger_kind_valid;
alter table workflows add constraint workflows_trigger_kind_valid
    check (trigger_kind in ('manual', 'schedule', 'event'));

alter table workflows add column trigger_event text;
alter table workflows add column conditions jsonb not null default '[]'::jsonb;

-- The trigger shape, one rule per kind: a schedule carries a cron and no event, an event
-- trigger carries an event and no cron, a manual trigger carries neither. This replaces the
-- two-kind shape the engine shipped with (a check constraint is not additive: `event` would
-- fall through both of its branches), and keeps the "no next run without a schedule" rule.
alter table workflows drop constraint workflows_schedule_shape;
alter table workflows add constraint workflows_schedule_shape check (
    (trigger_kind = 'manual' and schedule is null and next_run_at is null)
    or (trigger_kind = 'schedule' and schedule is not null)
    or (trigger_kind = 'event' and schedule is null and next_run_at is null)
);
alter table workflows add constraint workflows_trigger_event_shape check (
    (trigger_kind = 'event' and trigger_event is not null and schedule is null)
    or (trigger_kind <> 'event' and trigger_event is null)
);
alter table workflows add constraint workflows_conditions_is_array
    check (jsonb_typeof(conditions) = 'array');
-- Conditions are the event trigger's own rule: a workflow that cannot be triggered by an event
-- has nothing to evaluate them against, so it may not carry any.
alter table workflows add constraint workflows_conditions_need_event check (
    trigger_kind = 'event' or jsonb_array_length(conditions) = 0
);

-- What the matcher writes when a rule fires: the run is the record of the work, these two are
-- the record of the trigger (the panel shows "fired 12 times, last 09:41").
alter table workflows add column last_triggered_at timestamptz;
alter table workflows add column trigger_count integer not null default 0;
alter table workflows add constraint workflows_trigger_count_non_negative
    check (trigger_count >= 0);

-- Matching walks the event-triggered, armed workflows of one organization; the index keeps that
-- lookup off a sequential scan once the table grows.
create index workflows_event_idx on workflows (trigger_event, organization_id)
    where trigger_kind = 'event' and enabled;

-- A run of an event-triggered workflow records how it started.
alter table workflow_executions drop constraint workflow_executions_trigger_valid;
alter table workflow_executions add constraint workflow_executions_trigger_valid
    check (trigger_kind in ('manual', 'schedule', 'event'));

-- ---------------------------------------------------------------------------------------------
-- automation_cursor: how far the matcher has read the bus
-- ---------------------------------------------------------------------------------------------

-- One row, always: the highest event id the matcher has evaluated. The matcher locks it
-- (`for update skip locked`) and advances it in the same transaction that starts the runs of
-- that batch, so two instances never evaluate the same event twice and a restart resumes
-- exactly where the previous process stopped. A fresh installation seeds the cursor to the
-- current end of the bus on its first tick — a new rule watches forward, it does not fire for
-- everything the platform ever recorded.
create table automation_cursor (
    id            smallint    primary key default 1,
    last_event_id bigint      not null default 0,
    updated_at    timestamptz not null default now(),
    constraint automation_cursor_singleton check (id = 1),
    constraint automation_cursor_non_negative check (last_event_id >= 0)
);

insert into automation_cursor (id, last_event_id) values (1, 0);

-- ---------------------------------------------------------------------------------------------
-- page_revision_comments: the note an automation (or later a person) leaves on a revision
-- ---------------------------------------------------------------------------------------------

create table page_revision_comments (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    revision_id     uuid        not null references page_revisions (id) on delete cascade,
    author_user_id  uuid        references users (id) on delete set null,
    source          text        not null default 'user',
    body            text        not null,
    created_at      timestamptz not null default now(),
    constraint page_revision_comments_source_valid check (source in ('user', 'automation')),
    constraint page_revision_comments_body_not_blank check (length(btrim(body)) > 0),
    -- A comment written by an automation has no account behind it; one written by an account
    -- has one. The shape makes "who wrote this" answerable from the row itself.
    constraint page_revision_comments_author_shape check (
        (source = 'automation' and author_user_id is null)
        or (source = 'user' and author_user_id is not null)
    )
);

create index page_revision_comments_revision_idx
    on page_revision_comments (revision_id, created_at);
create index page_revision_comments_organization_idx
    on page_revision_comments (organization_id, created_at desc);
