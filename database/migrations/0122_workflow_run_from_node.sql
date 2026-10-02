-- Omnion · 0122 · "Run from here" (REQ-004 slice 3)
--
-- REQ-004's criterion: pressing *Run from here* on a mid-graph node starts a run whose
-- FIRST step is that node, the earlier nodes stay `skipped`, and the trace says why.
--
-- None of that is possible today, and the reason is a one-word hole in the schema. A run's
-- steps are all `pending` when it is created and the engine claims them strictly in
-- `step_no` order, so there is no way to say "these two did not run" without inventing a
-- sixth terminal state. `skipped` is that state:
--
--   * it is terminal, so `settle_execution` already counts it as closed — a run whose
--     earlier steps were skipped and whose tail succeeded settles `completed`, not
--     `failed`. The counter it reads (`status in ('pending','running','waiting')`) is left
--     alone on purpose: a skipped step must never be mistaken for open work.
--   * it is never claimed, because `claim_due_step` only reads `('pending','waiting')`.
--   * it is never a failure, because the two failure counters filter on `= 'failed'`.
--
-- So adding the state is genuinely a constraint change and nothing else: no engine branch,
-- no settle change, no claim change. The three queries that already exist keep working
-- unchanged, and the criterion becomes implementable rather than approximated.
--
-- The other two columns record the *why*, which is the part of the criterion that is easy
-- to leave implicit and then impossible to explain in a trace:
--
--   * `workflow_executions.started_from_node` — which node the run was launched at, so the
--     trace can say "started from Transform" instead of a reader having to infer it from
--     the first step's position.
--   * `workflow_steps.skip_reason` — why this step did not run, per step, in words an
--     operator reads. It is the same string for the whole prefix today, but it belongs on
--     the step: a future mode that skips one node in the middle needs somewhere to put it.
--
-- `node_id` already exists (0056) and is the join the canvas paints run status from, so
-- "which node did this run start at" resolves the same way a step's node does.

-- The sixth terminal state. Terminal because a skipped step is never picked up again:
-- `claim_due_step` reads only 'pending' and 'waiting', and a retry re-opens exactly
-- 'failed', 'cancelled', 'pending' and 'waiting' — a skipped row stays skipped, which is
-- correct: re-running from a node must not quietly re-run the tail's skipped prefix.
alter table workflow_steps drop constraint if exists workflow_steps_status_valid;
alter table workflow_steps add constraint workflow_steps_status_valid check (
    status in ('pending', 'running', 'waiting', 'succeeded', 'failed', 'cancelled', 'skipped')
);

-- The node a run was started at, `NULL` for a run that started at the trigger.
alter table workflow_executions
    add column if not exists started_from_node text;

-- Why a step did not run. Present on skipped steps; a real failure keeps its `error`.
alter table workflow_steps
    add column if not exists skip_reason text;

-- The canvas reads a run's start and the panel renders a step's reason by node; a partial
-- index on skipped steps only keeps it to the runs that actually have one.
create index if not exists workflow_steps_skipped_idx
    on workflow_steps (execution_id, step_no) where status = 'skipped';

-- Constraint: a skipped step is a step that did not run, so it cannot also be one that
-- started, produced output or recorded a failure message. A row that claims to have been
-- skipped *and* to have run is the kind of contradiction the builder's own validation
-- exists to prevent, and it is cheaper to refuse at the door.
alter table workflow_steps drop constraint if exists workflow_steps_skipped_shape;
alter table workflow_steps add constraint workflow_steps_skipped_shape check (
    status <> 'skipped'
    or (started_at is null and finished_at is null and output is null and error is null)
);

-- A skip always has a reason. This is the column the criterion's "the trace says why"
-- half is read from, and a NULL here is a trace that says "skipped" and stops.
alter table workflow_steps drop constraint if exists workflow_steps_skip_reason_present;
alter table workflow_steps add constraint workflow_steps_skip_reason_present check (
    status <> 'skipped' or (skip_reason is not null and length(btrim(skip_reason)) > 0)
);
