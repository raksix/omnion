-- REQ-099 · Agent runtime & tool loop — slice 1 (the agent, its runs and its steps).
--
-- The runtime exists to turn a model into an operator, and a runaway loop is the failure mode
-- that costs money. So this schema is built around one idea: **every ceiling the runtime
-- enforces is a column a row can carry**, not a prompt the model is asked to respect. If the
-- step cap, the deadline and the token budget only lived in a system prompt, every one of them
-- would be an instruction the model could decline to follow — and a loop that ignores its own
-- limits is indistinguishable from a loop that never had them.
--
-- Decisions worth stating, because each of them closes a way the record can lie:
--
-- 1. **A run is durable before its first step.** `ai_runs` is inserted `queued` and the runner
--    claims it, so a request that dies between "the user pressed Run" and "the model was called"
--    still leaves a row. A run that exists only in memory is a run whose disappearance nobody
--    can explain.
--
-- 2. **A step row is the idempotency record, and it is written `running` *before* the tool
--    runs.** A step left `running` after a crash is therefore not ambiguous: it means the tool
--    may or may not have executed, and the runtime reports it for a human rather than blindly
--    retrying a side effect. The alternative — write the row `completed` after the tool returns —
--    makes a crashed process look exactly like a tool that never ran, and the retry double-charges
--    a card, sends the mail twice, or writes the row it was told not to write.
--
-- 3. **`current_step` counts steps that *started*, and the unique `(run_id, step_no)` is what
--    makes a resumed run append rather than overwrite.** Two writers racing the same run would
--    otherwise both read `current_step = 3` and both write step 4, and the trace would lose the
--    step that actually ran.
--
-- 4. **Token and cost columns are the run's own totals, and they are the *sum of its steps*.**
--    They are denormalised on purpose: a run list renders a hundred rows and must not aggregate
--    a hundred step rows to do it. The invariant is asserted in a test rather than trusted —
--    `ai_runs.cost_micros` must equal the sum over its `completed` steps, because a cost figure
--    that can drift from the steps it came from is worse than no cost figure at all.
--
-- 5. **The partial indexes exist for the runner's two hot queries and nobody else.** "Which
--    runs are claimable" reads `queued`, and "which runs are alive" reads `running` plus a
--    heartbeat older than the requeue threshold. Indexing the whole status column would pay for
--    a predicate the runner never issues; a partial index on exactly those two states is both
--    smaller and the plan the query actually wants.
--
-- 6. **`agent_id` is `on delete set null`, not cascade.** A deleted agent's runs are the only
--    evidence that it ever spent money, and an operator asking "what did we pay for last month"
--    must still be able to answer it. The run keeps the `model_id` and the token columns for
--    exactly that reason — the trace outlives the definition.
--
-- 7. **`ai_memory` is not created here.** REQ-001's engine spec already owns it, and this
--    request's own table list notes that the shared tables land once. Only the tables this slice
--    actually writes are created.

create table if not exists ai_agents (
    id                uuid primary key default gen_random_uuid(),
    organization_id   uuid        not null references organizations (id) on delete cascade,
    site_id           uuid        references sites (id) on delete set null,
    key               text        not null,
    name              text        not null,
    description       text        not null default '',
    system_prompt     text        not null default '',
    model_id          uuid        references ai_models (id) on delete set null,
    temperature       numeric(3, 2) not null default 0.20,
    max_steps         int         not null default 8,
    deadline_seconds  int         not null default 300,
    token_budget      bigint      not null default 200000,
    tools             jsonb       not null default '[]'::jsonb,
    approvals         jsonb       not null default '[]'::jsonb,
    memory_scope      text        not null default 'none',
    enabled           boolean     not null default true,
    created_by        uuid        references users (id) on delete set null,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),

    -- The key is an API-visible identifier, so its shape is a promise: it appears in URLs, in
    -- workflow node configuration and in a webhook payload, and a key with a space in it is a
    -- key that has to be quoted in three places forever.
    constraint ai_agents_key_format check (key ~ '^[a-z][a-z0-9_-]{0,63}$'),
    -- The same bounds the runtime's own `RunLimits::clamped` enforces, so a row written by
    -- anything other than the agent form still cannot ask for a loop the runtime must refuse.
    constraint ai_agents_temperature_range check (temperature >= 0 and temperature <= 1),
    constraint ai_agents_max_steps_range check (max_steps >= 1 and max_steps <= 50),
    constraint ai_agents_deadline_range check (deadline_seconds >= 30 and deadline_seconds <= 3600),
    constraint ai_agents_token_budget_range check (token_budget >= 1000 and token_budget <= 2000000),
    -- An ordered list of tool keys, or the empty list. A JSON object here would mean "a tool
    -- called `order` with these arguments", which is a different feature and is not this one.
    constraint ai_agents_tools_is_array check (jsonb_typeof(tools) = 'array'),
    constraint ai_agents_approvals_is_array check (jsonb_typeof(approvals) = 'array'),
    constraint ai_agents_memory_scope_known check (memory_scope in ('none', 'organization', 'site', 'user')),
    -- The form bounds these and so does the runtime, but a name is the one column a list renders
    -- on every row, and an unbounded name is a layout bug that only shows up in production data.
    constraint ai_agents_name_length check (char_length(name) between 1 and 80),
    constraint ai_agents_description_length check (char_length(description) <= 400),
    constraint ai_agents_system_prompt_length check (char_length(system_prompt) <= 8000)
);

-- One key per organization: two agents answering to the same key would make every workflow
-- node and every webhook payload ambiguous about which one it meant.
create unique index ai_agents_org_key_uidx on ai_agents (organization_id, key);

-- The list screen's own ordering, so the index serves the query rather than the sort.
create index ai_agents_org_created_idx on ai_agents (organization_id, created_at desc);

-- The run store. `trigger` is not free text: a run started by a person and a run started by a
-- workflow node are different things to an operator reading the history, and a misspelled
-- trigger is a row that sorts nowhere and filters out of every view.
create table if not exists ai_runs (
    id                 uuid primary key default gen_random_uuid(),
    organization_id    uuid        not null references organizations (id) on delete cascade,
    site_id            uuid        references sites (id) on delete set null,
    agent_id           uuid        references ai_agents (id) on delete set null,
    user_id            uuid        references users (id) on delete set null,
    trigger            text        not null default 'chat',
    goal               text        not null,
    status             text        not null default 'queued',
    stop_reason        text,
    model_id           uuid        references ai_models (id) on delete set null,
    current_step       int         not null default 0,
    resume_count       int         not null default 0,
    cancel_requested_at timestamptz,
    deadline_at        timestamptz,
    token_budget       bigint,
    prompt_tokens      int         not null default 0,
    completion_tokens  int         not null default 0,
    cost_micros        bigint      not null default 0,
    heartbeat_at       timestamptz,
    started_at         timestamptz,
    finished_at        timestamptz,
    error              text,

    constraint ai_runs_trigger_known check (trigger in ('chat', 'agent', 'workflow', 'schedule')),
    constraint ai_runs_status_known check (
        status in ('queued', 'running', 'awaiting_approval', 'completed', 'failed', 'cancelled')
    ),
    -- `loop_detected` is a *status* on the list screen's filter, but it is not a state a run is
    -- parked in — the run is over when it loops, so it is recorded as `cancelled` with this stop
    -- reason. The check keeps a run from claiming to be running with a stop reason it reached.
    constraint ai_runs_stop_reason_known check (
        stop_reason is null or stop_reason in (
            'final_answer', 'max_steps', 'deadline', 'token_budget',
            'cancelled', 'loop_detected', 'error'
        )
    ),
    -- A finished run is exactly the one with `finished_at`, and a run still going has none.
    -- Without this the history list has to guess from `status` and a half-written row reads as
    -- a run that is still going.
    constraint ai_runs_finished_has_stamp check (
        (finished_at is null) or (status in ('completed', 'failed', 'cancelled'))
    ),
    constraint ai_runs_current_step_non_negative check (current_step >= 0),
    constraint ai_runs_resume_count_non_negative check (resume_count >= 0),
    constraint ai_runs_token_counters_non_negative check (
        prompt_tokens >= 0 and completion_tokens >= 0 and cost_micros >= 0
    ),
    -- A run that finished must say why, and a run that is still going must not claim a reason:
    -- the stop reason is what the run list's "why did this end" column renders, and a blank one
    -- on a completed row is a row nobody can triage. The second half matters as much: a row that
    -- is still `running` and already carries `max_steps` is claiming an ending that has not
    -- happened, and the runtime sets the reason in the same statement that sets the terminal
    -- status — so a reason without a terminal status can only be a bug or a hand edit.
    constraint ai_runs_completed_names_a_reason check (
        status <> 'completed' or stop_reason is not null
    ),
    constraint ai_runs_reason_implies_finished check (
        stop_reason is null or status in ('completed', 'failed', 'cancelled')
    ),
    constraint ai_runs_goal_length check (char_length(goal) between 1 and 2000)
);

create index ai_runs_org_started_idx on ai_runs (organization_id, started_at desc);
create index ai_runs_agent_started_idx on ai_runs (agent_id, started_at desc);

-- The runner's claim query reads exactly these two states, and the reaper reads them again
-- against `heartbeat_at`. A full index on `status` would carry every historical row to serve a
-- predicate that is nearly always false.
create index ai_runs_claimable_idx on ai_runs (status, heartbeat_at)
    where status in ('queued', 'running');

-- The approval inbox (REQ-101) reads this state and nothing else, and it must find a parked run
-- in constant time however long the run history grows.
create index ai_runs_awaiting_approval_idx on ai_runs (status)
    where status = 'awaiting_approval';

-- One row per step. `status` is the idempotency record: `completed` means the step's effect
-- definitely happened and must never happen again, and a `running` row after a crash is
-- inspected rather than retried.
create table if not exists ai_run_steps (
    id                uuid primary key default gen_random_uuid(),
    run_id            uuid        not null references ai_runs (id) on delete cascade,
    step_no           int         not null,
    kind              text        not null,
    tool              text,
    arguments         jsonb,
    result            jsonb,
    status            text        not null default 'running',
    prompt_tokens     int         not null default 0,
    completion_tokens int         not null default 0,
    duration_ms       int,
    error             text,
    started_at        timestamptz not null default now(),
    finished_at       timestamptz,

    constraint ai_run_steps_kind_known check (
        kind in ('message', 'tool_call', 'tool_result', 'approval', 'note', 'error')
    ),
    constraint ai_run_steps_status_known check (
        status in ('running', 'completed', 'failed', 'skipped')
    ),
    constraint ai_run_steps_tool_only_for_tool_kinds check (
        kind not in ('tool_call', 'tool_result') or tool is not null
    ),
    constraint ai_run_steps_step_no_positive check (step_no > 0),
    constraint ai_run_steps_token_counters_non_negative check (
        prompt_tokens >= 0 and completion_tokens >= 0
    ),
    constraint ai_run_steps_duration_non_negative check (duration_ms is null or duration_ms >= 0)
);

-- The resume rule *is* this constraint: a run appends steps, and a resumed run must never
-- rewrite the step that already completed. Two writers racing the same run would otherwise both
-- read `current_step = 3` and both write step 4, and the trace would lose the step that ran.
create unique index ai_run_steps_run_step_uidx on ai_run_steps (run_id, step_no);

-- The trace renders "the next step that is not completed" on every poll, and the reaper reads
-- the same predicate when it inspects a run whose worker died.
create index ai_run_steps_run_status_idx on ai_run_steps (run_id, status);

-- Two writers racing the same agent is prevented by the runtime's advisory lock, but the lock
-- is a process-local thing and a second API process must not be able to start a duplicate run
-- for the same agent either. The uniqueness that matters is on the *active* run, and a partial
-- unique index is the only way to say "at most one row with this status" without a trigger.
create unique index ai_runs_one_active_per_agent_uidx on ai_runs (agent_id)
    where agent_id is not null and status in ('queued', 'running', 'awaiting_approval');
