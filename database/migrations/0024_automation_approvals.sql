-- Omnion · 0024 · automation approvals and run-as authority
--
-- Slice 3 of the automation depth pass (docs/requests/REQ-003). Slice 2 gave a step a
-- policy for its own failure and three actions that leave the process. This migration adds
-- the two things a rule needs before it may act on somebody's behalf:
--
--   * `workflows.run_as_user_id` — whose authority the rule runs with. The author by
--     default; every host action resolves that account's effective permissions *at run
--     time*, so a rule can never keep a power its owner has since lost. This is the
--     request's own "a rule is never a permission back door", and the column is nullable on
--     purpose: a rule whose author has been deleted runs as nobody, and the run stops with
--     `automation.rule.permission_revoked` rather than as the deleted author.
--   * `workflow_approvals` — the human-in-the-loop gate. A `wait_for_approval` step parks
--     the run and writes a row here; deciding the row resumes the run (approve) or ends it
--     (reject). The decision is a single-use, expiring, hashed token rather than a link,
--     because an approval *is* a credential: the person who holds it can let an e-mail
--     leave the process.
--   * `workflow_executions.status` gains `awaiting_approval` — a run parked on a person is
--     neither running (nothing is progressing) nor terminal (a decision reopens it), and a
--     status that lies about one of those two things is worse than a new value.
--   * `workflow_executions.approval_id` / `workflow_steps.approval_id` — the run and its
--     gate step both point at the row, so a list of runs can answer "is this waiting on
--     somebody?" without a join into the steps table, and a trace can show *which* decision
--     it is parked on without a reverse lookup.
--
-- The permission key that guards the decision is `workflows.approve`, added to
-- `crates/permissions::catalogue` in the same commit: deciding an approval is deliberately
-- not `workflows.run`, so a role that may start a rule cannot wave through everything that
-- rule parks.
--
-- Released migrations are append-only (docs/05-VERSIONING.md). Every column has a default
-- that reproduces what v0 did: a run still starts, still claims steps and still settles.

-- ---------------------------------------------------------------------------------------------
-- workflow_executions: a run parked on a person
-- ---------------------------------------------------------------------------------------------

-- `awaiting_approval` sits beside `running`: the run is open and a decision reopens it, but
-- no step is due — the engine must not claim anything while a person is thinking. The
-- `finished_at` check widens with it (a parked run is not finished) rather than being
-- replaced, because the check's job is "a terminal state has a finish time and an open one
-- does not", and that stays true for the new value.
alter table workflow_executions drop constraint workflow_executions_status_valid;
alter table workflow_executions add constraint workflow_executions_status_valid
    check (status in ('running', 'awaiting_approval', 'completed', 'failed', 'cancelled'));

alter table workflow_executions drop constraint workflow_executions_finished_shape;
alter table workflow_executions add constraint workflow_executions_finished_shape
    check ((status in ('running', 'awaiting_approval')) = (finished_at is null));

-- The open-runs index is what the runner's reconciler and the panel's "still going" list
-- both read, so a parked run has to be in it — otherwise a run nobody ever decides would
-- look finished in every listing that filters on it.
create index workflow_executions_awaiting_idx
    on workflow_executions (organization_id, started_at desc) where status = 'awaiting_approval';

-- ---------------------------------------------------------------------------------------------
-- workflows: whose authority a rule runs with
-- ---------------------------------------------------------------------------------------------

-- Null means "the author", resolved at run time from `workflows.created_by`. It is a
-- separate column rather than a copy of the author so an operator can hand a rule to
-- another account deliberately — and so a *deleted* author is a visible state (the column
-- stays null and the run stops) instead of a dangling foreign key.
alter table workflows add column run_as_user_id uuid references users (id) on delete set null;

-- ---------------------------------------------------------------------------------------------
-- workflow_approvals: the gate itself
-- ---------------------------------------------------------------------------------------------

create table workflow_approvals (
    id                    uuid        primary key default gen_random_uuid(),
    execution_id          uuid        not null references workflow_executions (id) on delete cascade,
    step_id               uuid        not null references workflow_steps (id) on delete cascade,
    step_no               integer     not null,
    organization_id       uuid        not null references organizations (id) on delete cascade,
    rule_id               uuid        references workflows (id) on delete cascade,
    requested_by_user_id  uuid        references users (id) on delete set null,
    requested_at          timestamptz not null default now(),
    expires_at            timestamptz not null,
    decision              text,
    decided_by            uuid        references users (id) on delete set null,
    decided_at            timestamptz,
    note                  text,
    -- Only the hash is stored. The token is shown once, expires, and is single-use: an
    -- approval link is a credential that can let a message leave the process, so a database
    -- read must not be able to mint one.
    decision_token_hash   text        not null,
    constraint workflow_approvals_decision_valid
        check (decision is null or decision in ('approved', 'rejected')),
    -- A decided row always has a *time*, but not always a *person*: an approval that
    -- expired was decided by nobody, and `decided_by` is exactly null for it. The old
    -- pairing (`decided_at is null` iff `decided_by is null`) made the sweeper's own write
    -- fail its own constraint — a shape where "the gate timed out" could not be recorded.
    -- The two are separate facts: when, and by whom.
    constraint workflow_approvals_decided_shape
        check ((decision is null) = (decided_at is null)),
    constraint workflow_approvals_step_no_positive check (step_no >= 1),
    constraint workflow_approvals_note_bounded check (note is null or length(note) <= 2000)
    -- No `expires_at > requested_at` check on purpose. The deadline is set once from the
    -- step's own `expires_in_hours`, which the engine validates to 1..=720 before it gets
    -- here, so a constraint would restate a rule the code already enforces — and it would
    -- forbid the one legitimate write that follows: an administrator shortening a long
    -- gate, or a test that shortens one to prove the sweeper ends its run. A gate that is
    -- already expired is expired forever either way, because the decision is a moment in
    -- the run's life, not a permission.
);

-- The token is unique — two rows can never share a credential — and the panel's pending list
-- is `organization_id` + "still undecided", oldest first.
create unique index workflow_approvals_token_idx
    on workflow_approvals (decision_token_hash);

create index workflow_approvals_pending_idx
    on workflow_approvals (organization_id, requested_at desc)
    where decision is null;

-- The sweep reads the same list, and it filters on expiry too.
create index workflow_approvals_expiry_idx
    on workflow_approvals (expires_at) where decision is null;

-- One step of one run waits for at most one approval. Without this a retried
-- `wait_for_approval` step could park a second row beside the first, and the panel would
-- offer two decisions for one gate.
create unique index workflow_approvals_step_idx
    on workflow_approvals (step_id);

-- ---------------------------------------------------------------------------------------------
-- workflow_steps: the step points at the row that gates it
-- ---------------------------------------------------------------------------------------------

alter table workflow_steps add column approval_id uuid
    references workflow_approvals (id) on delete set null;

-- The run points at the gate that parks it, a copy of the step's own value rather than a
-- join. Both surfaces ask "is this run waiting on somebody?" about *every* row they draw —
-- the pending panel and the run history's status chip — and a join back to `workflow_steps`
-- would turn that into a second query per row. `on delete set null` because the gate is the
-- step's fact, not the run's: a run that outlived a deleted gate is still a run.
--
-- It is written *here*, after `workflow_approvals` exists, because the foreign key names
-- that table — an `alter` above the `create` would be a forward reference, and the failure
-- it produces ("relation does not exist") reads like a typo rather than an ordering mistake.
alter table workflow_executions add column approval_id uuid
    references workflow_approvals (id) on delete set null;

-- An approval step is claimed twice, like a wait: once to park it, once to let the decision
-- resume it. Two attempts is the same budget a wait gets, and a gate is not retried.
alter table workflow_steps drop constraint workflow_steps_kind_valid;
alter table workflow_steps add constraint workflow_steps_kind_valid
    check (kind in ('task', 'wait', 'branch', 'stop', 'approval'));

-- A parked approval claims no action: the engine decides it, like a branch and a stop.
alter table workflow_steps drop constraint workflow_steps_action_shape;
alter table workflow_steps add constraint workflow_steps_action_shape check (
    (kind in ('task', 'branch') and action is not null)
    or (kind in ('wait', 'stop', 'approval') and action is null)
);

alter table workflow_steps drop constraint workflow_steps_attempts_shape;
alter table workflow_steps add constraint workflow_steps_attempts_shape check (
    (kind = 'task' and attempts between 0 and max_attempts)
    or (kind in ('wait', 'branch', 'stop', 'approval') and attempts between 0 and 2)
);
