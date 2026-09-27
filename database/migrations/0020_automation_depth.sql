-- Omnion · 0020 · automation depth: inbound hooks, condition groups, test events
--
-- Slice 1 of the automation depth pass (docs/requests/REQ-003). v0 of the layer
-- (migration 0010) stored one event name and a flat list of comparisons per rule;
-- this migration adds the three things that pass needs and nothing else — later
-- slices (REQ-003 slices 2–4: action library, approvals, operations) add their own
-- tables rather than widening these:
--
--   * `workflows.hook_token_hash` — the credential of an inbound-webhook trigger.
--     The token itself is shown once and never stored: only its hash is, so a
--     database read cannot call the platform's own hooks.
--   * `workflows.conditions` accepts a group tree (`{"all": […]}` / `{"any": […]}`)
--     next to the v0 array. The check constraint widens rather than being replaced;
--     the matcher reads a bare array as `{"all": […]}`, so rows written by 0010 keep
--     firing exactly as they did.
--   * `automation_test_events` — a dry run against a hand-written payload, and a
--     one-shot listener that captures the next real event a rule matches.
--   * `automation_hook_windows` — the per-rule rate window an inbound hook is
--     counted in, keyed by the rule (never by the token, so no credential material
--     lives in the limiter).
--
-- Released migrations are append-only (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- workflows: the inbound hook credential
-- ---------------------------------------------------------------------------------------------

alter table workflows add column hook_token_hash text;

-- One token, one rule. A partial unique index: rules without a hook are unaffected,
-- and two rules can never share a URL.
create unique index workflows_hook_token_idx on workflows (hook_token_hash)
    where hook_token_hash is not null;

-- A hook trigger is still an event-triggered workflow — that is what keeps its runs
-- in the same durable engine — so the trigger shape stays: an event rule carries an
-- event name, and the reserved `automation.hook.received` name is what an inbound
-- hook trigger carries. The token is additive: a rule with none is not a hook.
alter table workflows add constraint workflows_hook_shape check (
    (hook_token_hash is null) or (trigger_kind = 'event')
);

-- ---------------------------------------------------------------------------------------------
-- workflows.conditions: a group tree, or the v0 flat array
-- ---------------------------------------------------------------------------------------------

-- v0 asserted `jsonb_typeof(conditions) = 'array'`. A group tree is an object with
-- exactly one of `all` / `any`; both are accepted from here on and the matcher treats
-- an array as `{"all": […]}`, which is what every row before this migration means.
alter table workflows drop constraint workflows_conditions_is_array;
alter table workflows add constraint workflows_conditions_is_group check (
    jsonb_typeof(conditions) = 'array'
    or (
        jsonb_typeof(conditions) = 'object'
        and jsonb_array_length(conditions) = 1
        and (conditions ? 'all' or conditions ? 'any')
    )
);

-- The depth cap is enforced in the layer that reads the tree (the panel offers at
-- most three levels); the database only guards the shape, because a JSON tree has no
-- column to hang a per-level constraint on.

-- ---------------------------------------------------------------------------------------------
-- automation_test_events: dry runs and one-shot listeners
-- ---------------------------------------------------------------------------------------------

-- A `test` row is a dry run: the panel evaluated a hand-written payload against the
-- rule's conditions and reported what each action *would* do. Nothing is sent, and
-- the row is only the report the panel can re-read.
--
-- A `listen` row is a one-shot listener: it starts with a null payload, and the
-- matcher fills it in with the first real event the rule matches. That is how "show
-- me what actually arrives" is answered without waiting for the next failure.
create table automation_test_events (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        not null references organizations (id) on delete cascade,
    workflow_id      uuid        not null references workflows (id) on delete cascade,
    kind             text        not null,
    payload          jsonb,
    event_id         bigint,
    event_name       text,
    created_by       uuid        references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    captured_at      timestamptz,
    constraint automation_test_events_kind_valid check (kind in ('test', 'listen')),
    -- A dry run always carries the payload it was written against; a listener starts
    -- empty and fills in when the matcher sees a matching event. Exactly one of the
    -- two states, so "armed" and "captured" are answerable from the row itself.
    constraint automation_test_events_state_shape check (
        (kind = 'test' and payload is not null)
        or (kind = 'listen')
    )
);

-- The panel reads a rule's own history newest-first, and the matcher looks a rule's
-- armed listeners up by workflow.
create index automation_test_events_workflow_idx
    on automation_test_events (workflow_id, created_at desc);
create index automation_test_events_armed_idx
    on automation_test_events (workflow_id, created_at desc)
    where kind = 'listen' and payload is null;

-- ---------------------------------------------------------------------------------------------
-- automation_hook_windows: the per-rule inbound rate window
-- ---------------------------------------------------------------------------------------------

-- An inbound hook is a public surface, so it is counted. The window is keyed by the
-- rule rather than by the token: the limiter then holds no credential material, and a
-- rotated token keeps the rule's own history instead of starting a fresh allowance.
create table automation_hook_windows (
    workflow_id  uuid        primary key references workflows (id) on delete cascade,
    window_start timestamptz not null default now(),
    hits         integer     not null default 0,
    constraint automation_hook_windows_non_negative check (hits >= 0)
);
