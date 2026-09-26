-- Omnion · 0006 · workflows: the durable step engine
--
-- v0 of the automation engine (docs/requests/REQ-003, phase P09). Three tables carry the whole
-- machine: `workflows` holds one definition (a trigger plus an ordered list of steps),
-- `workflow_executions` one run of it, and `workflow_steps` the steps of that run, materialised
-- when the run starts. From the moment a run exists its progress is a set of rows — a restart
-- loses nothing (docs/09-N8N-TEARDOWN.md §13, lesson 1: durable steps from day one).
--
-- Reads and writes the engine performs:
--   * the runner claims the oldest due step of the oldest open run as one statement
--     (`for update skip locked`), so several API instances never run the same step twice;
--   * a wait step parks with `available_at` in the future and is resumed after that instant;
--   * a failing step is re-queued with its retry backoff in `available_at` until its attempts
--     run out (at most five, the n8n cap — docs/09 §13 lesson 4).
-- Released migrations are append-only (docs/05-VERSIONING.md).

create table workflows (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    site_id         uuid        references sites (id) on delete cascade,
    name            text        not null,
    description     text        not null default '',
    enabled         boolean     not null default true,
    trigger_kind    text        not null,
    schedule        text,
    next_run_at     timestamptz,
    steps           jsonb       not null,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint workflows_trigger_kind_valid check (trigger_kind in ('manual', 'schedule')),
    constraint workflows_name_not_blank check (length(btrim(name)) > 0),
    constraint workflows_steps_is_array check (jsonb_typeof(steps) = 'array'),
    constraint workflows_schedule_shape check (
        (trigger_kind = 'manual' and schedule is null and next_run_at is null)
        or (trigger_kind = 'schedule' and schedule is not null)
    )
);

create index workflows_organization_idx on workflows (organization_id, created_at desc);
create index workflows_due_idx on workflows (next_run_at)
    where trigger_kind = 'schedule' and enabled;

create table workflow_executions (
    id              uuid        primary key default gen_random_uuid(),
    workflow_id     uuid        not null references workflows (id) on delete cascade,
    organization_id uuid        not null references organizations (id) on delete cascade,
    status          text        not null default 'running',
    trigger_kind    text        not null,
    triggered_by    uuid        references users (id) on delete set null,
    started_at      timestamptz not null default now(),
    finished_at     timestamptz,
    error           text,
    constraint workflow_executions_status_valid
        check (status in ('running', 'completed', 'failed', 'cancelled')),
    constraint workflow_executions_trigger_valid check (trigger_kind in ('manual', 'schedule')),
    -- A run that is still running has no end; a settled run always has one.
    constraint workflow_executions_finished_shape
        check ((status = 'running') = (finished_at is null))
);

create index workflow_executions_workflow_idx
    on workflow_executions (workflow_id, started_at desc);
create index workflow_executions_open_idx
    on workflow_executions (started_at) where status = 'running';

create table workflow_steps (
    id           uuid        primary key default gen_random_uuid(),
    execution_id uuid        not null references workflow_executions (id) on delete cascade,
    step_no      integer     not null,
    name         text        not null,
    kind         text        not null,
    action       text,
    params       jsonb       not null default '{}'::jsonb,
    status       text        not null default 'pending',
    attempts     integer     not null default 0,
    max_attempts integer     not null default 1,
    available_at timestamptz not null default now(),
    started_at   timestamptz,
    finished_at  timestamptz,
    output       jsonb,
    error        text,
    constraint workflow_steps_no_positive check (step_no > 0),
    constraint workflow_steps_name_not_blank check (length(btrim(name)) > 0),
    constraint workflow_steps_kind_valid check (kind in ('task', 'wait')),
    constraint workflow_steps_status_valid check (
        status in ('pending', 'running', 'waiting', 'succeeded', 'failed', 'cancelled')
    ),
    constraint workflow_steps_action_shape check (
        (kind = 'task' and action is not null) or (kind = 'wait' and action is null)
    ),
    constraint workflow_steps_max_attempts_cap check (max_attempts between 1 and 5),
    -- A task step never exceeds the attempts it was given; a wait step is claimed at most twice
    -- (once to park, once to resume), so a corrupted row cannot park itself in a loop.
    constraint workflow_steps_attempts_shape check (
        (kind = 'task' and attempts between 0 and max_attempts)
        or (kind = 'wait' and attempts between 0 and 2)
    ),
    constraint workflow_steps_position_unique unique (execution_id, step_no)
);

create index workflow_steps_due_idx on workflow_steps (status, available_at);
create index workflow_steps_execution_idx on workflow_steps (execution_id, step_no);
