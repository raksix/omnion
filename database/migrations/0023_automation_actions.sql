-- Omnion · 0023 · automation action library and error paths
--
-- Slice 2 of the automation depth pass (docs/requests/REQ-003). Migration 0020 added the
-- inbound hook, the condition groups and the test-fire rows; this one adds the *other* half
-- of the machine — what a step may do, how a step may end, and what an operator does with a
-- run that went wrong:
--
--   * `workflow_steps.kind` widens to `('task','wait','branch','stop')`. A `branch` step
--     reads one field of the resolved inputs and ends the run when the comparison does not
--     hold; a `stop` step always ends it. Both are engine kinds, so they live in the engine
--     that already owns waits — not in the automation layer that only supplies actions.
--   * `workflow_steps.on_error` — what a failure of *this* step does: `stop` ends the run,
--     `continue` records the failure and lets the run go on. `inherit` takes the rule's own
--     policy, which is what an editor writes by default.
--   * `workflow_steps.timeout_ms` — a step that outlives it is failed with the limit named.
--   * `workflow_steps.ignored` — the trace of a failure the run deliberately outlived. Kept
--     as a real `failed` row (the run's own history must not pretend it succeeded) with a
--     flag that decides whether the run may settle as completed.
--   * `workflows.hook_secret` — the per-rule key that signs an outbound `http_request`.
--     Minted on first use, stored in the clear only because it is a signing key of a
--     *signature*, never returned by the API, and never an inbound credential.
--   * `workflows.on_error` — the *rule's* error policy, which a step inherits when it does
--     not set its own. `stop` is the v0 behaviour, so every existing rule keeps it.
--   * `workflow_executions.event_payload` — the payload the run started from. A branch step
--     reads `event.<field>` from it, and the run detail's sidebar shows it, which is why it
--     belongs on the run rather than in a side table the panel has to join for.
--   * `automation_settings` — the single row an organization reads for its outbound host
--     allow-list. An allow-list that lives in a table (not an environment variable) is one
--     an administrator can change in the panel and one the QA stack can seed per run.
--
-- Released migrations are append-only (docs/05-VERSIONING.md). Nothing here rewrites a
-- row: every column has a default that reproduces what v0 did.

-- ---------------------------------------------------------------------------------------------
-- workflow_steps: kinds, error policy, timeout
-- ---------------------------------------------------------------------------------------------

-- v0 knew `task` and `wait`. A branch and a stop are the same kind of thing the engine
-- already understands — a write that parks or settles a run — so the check widens rather
-- than the engine growing a second, parallel state machine.
alter table workflow_steps drop constraint workflow_steps_kind_valid;
alter table workflow_steps add constraint workflow_steps_kind_valid
    check (kind in ('task', 'wait', 'branch', 'stop'));

-- A branch reads a comparison; a stop reads nothing. Both are parameters like a wait's.
alter table workflow_steps drop constraint workflow_steps_action_shape;
alter table workflow_steps add constraint workflow_steps_action_shape check (
    (kind in ('task', 'branch') and action is not null) or (kind in ('wait', 'stop') and action is null)
);

-- A branch is claimed exactly once and a stop exactly once, like a control step: a
-- comparison is not retried and a decision is not re-made. The attempts check widens
-- alongside the kind check so a control step cannot loop on its own retries.
alter table workflow_steps drop constraint workflow_steps_attempts_shape;
alter table workflow_steps add constraint workflow_steps_attempts_shape check (
    (kind = 'task' and attempts between 0 and max_attempts)
    or (kind in ('wait', 'branch', 'stop') and attempts between 0 and 2)
);

-- `inherit` is the v0 behaviour (a failure ends the run), so every existing row keeps it.
alter table workflow_steps add column on_error text not null default 'inherit';
alter table workflow_steps add constraint workflow_steps_on_error_valid check (
    on_error in ('inherit', 'stop', 'continue')
);

-- 30 s is the v0 default; 120 s is the ceiling the plan names. A wait parks rather than
-- blocking, so it is exempt — parking is bounded by `MAX_WAIT_SECONDS`, not by this.
alter table workflow_steps add column timeout_ms integer not null default 30000;
alter table workflow_steps add constraint workflow_steps_timeout_bounded check (
    (kind = 'wait') or (timeout_ms between 1 and 120000)
);

-- A step whose failure the run deliberately outlived. The row stays `failed` — the history is
-- the history — and this flag is what lets the run settle as completed.
alter table workflow_steps add column ignored boolean not null default false;

create index workflow_steps_ignored_idx
    on workflow_steps (execution_id, step_no) where status = 'failed' and ignored;

-- ---------------------------------------------------------------------------------------------
-- workflows: the rule's own error policy and its outbound signing key
-- ---------------------------------------------------------------------------------------------

-- The rule's half of the policy. A step that says `inherit` takes this, read at run time
-- so a change to the rule reaches the runs that have not reached the step yet.
alter table workflows add column on_error text not null default 'stop';
alter table workflows add constraint workflows_on_error_valid
    check (on_error in ('stop', 'continue'));

-- The inbound token is hashed because it is a *credential* (it authorises a call). This key
-- is not: it signs a request this platform makes, and the signature can only be verified by
-- a receiver, never replayed against us. It is still never returned by the API, never
-- audited and never rendered in the panel.
alter table workflows add column hook_secret text;

-- ---------------------------------------------------------------------------------------------
-- workflow_executions.event_payload: what the run started from
-- ---------------------------------------------------------------------------------------------

-- An event trigger's payload, and null for a manual run or a schedule. A branch step reads
-- `event.<field>` out of it, and the run detail shows it beside the trace — the raw payload
-- is the first thing anyone debugging a rule wants, and the one thing a step table cannot
-- answer.
alter table workflow_executions add column event_payload jsonb;

-- ---------------------------------------------------------------------------------------------
-- automation_settings: the outbound host allow-list
-- ---------------------------------------------------------------------------------------------

-- One row, `id = 1`, the shape a later multi-organization world widens without moving the
-- primary key. The allow-list is *empty* by default: an installation that has not decided
-- which hosts its rules may call makes no outbound calls at all, which is the safe default
-- for a feature that leaves the process.
create table automation_settings (
    id                  smallint   primary key default 1,
    http_allowed_hosts  text[]     not null default '{}',
    approval_ttl_hours  integer    not null default 72,
    updated_at          timestamptz not null default now(),
    constraint automation_settings_single_row check (id = 1),
    constraint automation_settings_approval_ttl check (approval_ttl_hours between 1 and 720)
);

insert into automation_settings (id) values (1);
