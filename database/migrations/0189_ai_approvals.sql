-- 0189_ai_approvals.sql — REQ-101 slice 1: the gate, the inbox and the class policy table.
--
-- Three tables, and each one is shaped by a failure it is defending against. The request is
-- unusually explicit about the failure modes ("Nothing dangerous happens without a human"), so
-- the schema is where most of that reasoning has to live: a check constraint is the only part of
-- the design a code review cannot quietly skip.
--
-- 1. **`ai_approvals` is append-once, mutate-once.** A request row is written `pending` with a
--    frozen `preview` and the `preview_hash` that goes with it, and the *first* decision moves it
--    to a terminal state. `(status = 'pending') = (decided_at is null)` makes that a database
--    invariant rather than a convention: a row that is decided but has no timestamp (or vice
--    versa) cannot exist, so "when was this approved" is never answered by a guess. The same
--    reasoning gives `expires_at > created_at` — an approval that expires before it was created
--    is either a clock bug or a hand-written row, and neither should reach the inbox.
--
--    `preview` is `not null` and stored **frozen**, which is the whole point of the request's
--    "previews are stored frozen so the reviewer decides on what they saw". A preview recomputed
--    at decision time is a preview of something the reviewer never looked at.
--
--    The foreign keys are `set null`, not `cascade`, for everything the request points at: a
--    pruned run must not erase the record that somebody approved an action because of it. The
--    only cascade is `organization_id` — a tenant that is gone should not leave approval rows
--    behind, and the rows have no meaning outside their tenant anyway.
--
-- 2. **The single-use guarantee is a conditional update, not a check constraint.** PostgreSQL
--    cannot express "transition only from pending" as a constraint, so it is expressed in the
--    store's `approve`/`reject` as `update … where id = $1 and status = 'pending'`, and the
--    acceptance criterion "a second decision answers `already_decided` and changes nothing" is a
--    walk that asserts the *row* is untouched after the second call. Two `for update` readers
--    would both see `pending` and both write; the `where` clause is the lock.
--
-- 3. **`ai_approval_policies` folds `organization_id` for uniqueness, for the same PostgreSQL
--    reason `ai_identities` does.** `unique (organization_id, tool_class)` does not fire for the
--    NULL platform-default row, so every organization could install a row claiming to be the
--    platform default and the resolver would get whichever the planner returned first. The
--    folded index collapses NULL to the nil uuid — a value the schema already refuses as a
--    foreign key — and the constraint becomes real.
--
--    The six classes are seeded as platform defaults with `mode = 'require'`, so a **fresh
--    installation gates everything dangerous** without any code path having to run first. A
--    missing row means "inherit", and inheriting from a seeded default is the safe direction;
--    the dangerous direction (no row anywhere) is not representable once the seed has run.
--
--    `expires_minutes` is bounded at the column, because an expiry of zero would expire every
--    approval on arrival and an expiry of 100000 would park a run for months. The bound is the
--    request's own 5–1440.
--
-- 4. **`ai_change_sets` is slice 3 and gets its table here anyway.** The migration ledger is
--    append-only and every writer shares one numbering space, so splitting a REQ's tables across
--    two migrations means paying the number-allocation dance twice for no benefit. The table is
--    created with its final shape; slice 3 writes it.
--
--    `operations` is jsonb rather than a child table on purpose: an ordered list of *proposed*
--    operations is one value the editor reads and writes whole, it is never queried by
--    field, and a relational shape would make "reorder" and "drop one" two extra queries for a
--    list that is always fetched in full anyway. The `status` check is closed for the same
--    reason `ai_tool_calls.status` is.

create table if not exists ai_approvals (
    id                     uuid primary key default gen_random_uuid(),
    organization_id        uuid        not null references organizations (id) on delete cascade,
    site_id                uuid        references sites (id) on delete cascade,
    -- A pruned run keeps its approvals: the record that a human approved an action is older than
    -- the run that asked for it, and `set null` is what says so.
    run_id                 uuid        references ai_runs (id) on delete set null,
    step_id                uuid        references ai_run_steps (id) on delete set null,
    agent_id               uuid        references ai_agents (id) on delete set null,
    identity_id            uuid        references ai_identities (id) on delete set null,
    change_set_id          uuid,
    tool_key               text        not null,
    tool_class             text        not null,
    resource_type          text,
    resource_id            text,
    -- The human-readable name the confirmation phrase is checked against, stored because the
    -- resource may be renamed (or deleted) between the request and the decision.
    resource_label         text,
    risk                   text        not null default 'medium',
    title                  text        not null,
    summary                text        not null default '',
    operation_count        integer     not null default 1,
    -- Irreversible operations (delete, deployment, database operation) are the ones the typed
    -- confirmation exists for; the flag is stored so the danger zone is read from the row
    -- rather than re-derived from the class at render time.
    irreversible           boolean     not null default false,
    requires_confirmation  boolean     not null default false,
    confirmation_phrase    text,
    -- Frozen at request time. `not null` because a review screen with nothing to review is a
    -- dead end, and the request is explicit that the reviewer decides on what they saw.
    preview                jsonb       not null,
    preview_hash           text        not null,
    -- The revision each operation was computed against; `stale` is this string disagreeing with
    -- the resource's current one.
    base_revision          text,
    status                 text        not null default 'pending',
    requested_by           uuid        references users (id) on delete set null,
    model_id               uuid        references ai_models (id) on delete set null,
    expires_at             timestamptz not null,
    decided_by             uuid        references users (id) on delete set null,
    decided_at             timestamptz,
    decision_note          text,
    applied_at             timestamptz,
    error                  text,
    created_at             timestamptz not null default now(),

    constraint ai_approvals_tool_class_known
        check (tool_class in ('content_publish', 'content_delete', 'plugin_install',
                               'theme_change', 'deployment', 'database_operation')),
    constraint ai_approvals_risk_known check (risk in ('low', 'medium', 'high')),
    constraint ai_approvals_status_known
        check (status in ('pending', 'approved', 'rejected', 'expired', 'stale', 'applied', 'failed')),
    constraint ai_approvals_operation_count_positive check (operation_count >= 1),
    -- The decision invariant. A row that is `pending` and carries a `decided_at` would make
    -- "waiting for you" show a decided request, which is the one lie an inbox cannot carry.
    constraint ai_approvals_pending_iff_undecided check ((status = 'pending') = (decided_at is null)),
    -- An approval that expires before it was created is not a tight expiry, it is a bug.
    constraint ai_approvals_expiry_after_creation check (expires_at > created_at),
    -- A rejected request carries its reason; the request's criterion is "rejecting requires a
    -- reason", and a rejection with an empty note is a decision nobody can audit.
    constraint ai_approvals_rejection_has_reason
        check (status <> 'rejected' or (decision_note is not null and char_length(btrim(decision_note)) > 0)),
    -- A confirmation phrase only exists where one is required, and where it is required the
    -- phrase is the resource's label. A phrase that is blank cannot be typed back, so the
    -- column check is the same rule the API enforces.
    constraint ai_approvals_confirmation_phrase_shape
        check (not requires_confirmation or (confirmation_phrase is not null and char_length(btrim(confirmation_phrase)) > 0)),
    constraint ai_approvals_preview_is_object check (jsonb_typeof(preview) = 'object')
);

-- The inbox's own query: one organization's rows in one status, soonest expiry first.
create index ai_approvals_org_status_expiry_idx
    on ai_approvals (organization_id, status, expires_at);
-- The pending count badge and the sweeper's due rows, both of which are "everything pending"
-- across every organization and would otherwise sequential-scan.
create index ai_approvals_pending_idx
    on ai_approvals (expires_at) where status = 'pending';
-- "which run is this waiting on" — the resume path after a decision, and the run detail's link.
create index ai_approvals_run_idx on ai_approvals (run_id) where run_id is not null;
-- The inbox's default ordering when no status is chosen, and the retention sweep.
create index ai_approvals_created_at_idx on ai_approvals (created_at desc);

-- The duplicate guard. "Requesting the same tool in a loop produces one pending approval and an
-- `already_pending` refusal, not a flood" is a *database* guarantee and has to survive a
-- restart, so it is a partial unique index rather than a check in application code: two runs
-- racing to request the same tool in the same organization would both read "no pending row" and
-- both write one.
create unique index ai_approvals_one_pending_per_run_step
    on ai_approvals (run_id, step_id)
    where run_id is not null and step_id is not null and status = 'pending';

-- The class policy. NULL organization_id is the platform default row, shared by every tenant.
create table if not exists ai_approval_policies (
    id                  uuid primary key default gen_random_uuid(),
    organization_id     uuid        references organizations (id) on delete cascade,
    tool_class          text        not null,
    mode                text        not null default 'require',
    typed_confirmation  boolean     not null default true,
    expires_minutes     integer     not null default 60,
    updated_by          uuid        references users (id) on delete set null,
    updated_at          timestamptz not null default now(),

    constraint ai_approval_policies_class_known
        check (tool_class in ('content_publish', 'content_delete', 'plugin_install',
                               'theme_change', 'deployment', 'database_operation')),
    -- Two-valued on purpose. A third "warn" state would be a mode the runtime has to interpret,
    -- and the request's whole argument is that approval fatigue comes from states nobody can
    -- act on: a class either requires a decision or it does not.
    constraint ai_approval_policies_mode_known check (mode in ('require', 'allow')),
    -- The request's 5–1440. A zero would expire every approval on arrival; a year would park a
    -- run past every operator's attention span and keep the run's step `running` for the same
    -- time.
    constraint ai_approval_policies_expiry_range check (expires_minutes between 5 and 1440)
);

-- Decision (3): a plain unique (organization_id, tool_class) does not fire for the NULL row.
create unique index ai_approval_policies_org_class_unique
    on ai_approval_policies (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), tool_class);
create index ai_approval_policies_org_idx on ai_approval_policies (organization_id) where organization_id is not null;

-- The six classes, gated from the first boot. `on conflict do nothing` rather than `do update`:
-- a boot must never reset an operator's decision about a dangerous class, which is exactly the
-- rule `registry::seed` follows for `requires_approval` and for the same reason.
insert into ai_approval_policies (organization_id, tool_class, mode, typed_confirmation, expires_minutes)
select null, class, 'require', true, 60
from (values
    ('content_publish'),
    ('content_delete'),
    ('plugin_install'),
    ('theme_change'),
    ('deployment'),
    ('database_operation')
) as classes(class)
on conflict do nothing;

-- Change sets: a conversation's proposed operations, edited and confirmed by a person.
-- Created here for the numbering reason above; slice 3 writes it.
create table if not exists ai_change_sets (
    id                uuid primary key default gen_random_uuid(),
    organization_id   uuid        not null references organizations (id) on delete cascade,
    site_id           uuid        references sites (id) on delete cascade,
    title             text        not null,
    status            text        not null default 'draft',
    operations        jsonb       not null default '[]'::jsonb,
    base_revisions    jsonb       not null default '{}'::jsonb,
    created_by        uuid        references users (id) on delete set null,
    created_by_agent  uuid        references ai_agents (id) on delete set null,
    created_by_run    uuid        references ai_runs (id) on delete set null,
    confirmed_at      timestamptz,
    applied_at        timestamptz,
    discarded_reason  text,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),

    constraint ai_change_sets_status_known
        check (status in ('draft', 'pending', 'confirmed', 'applied', 'discarded', 'expired')),
    constraint ai_change_sets_operations_is_array check (jsonb_typeof(operations) = 'array'),
    constraint ai_change_sets_revisions_is_object check (jsonb_typeof(base_revisions) = 'object'),
    constraint ai_change_sets_title_len check (char_length(btrim(title)) between 1 and 160),
    -- A discarded set says why, for the same reason a rejected approval does: an unexplained
    -- drop is indistinguishable from a bug that lost the work.
    constraint ai_change_sets_discard_has_reason
        check (status <> 'discarded' or (discarded_reason is not null and char_length(btrim(discarded_reason)) > 0))
);

create index ai_change_sets_org_created_idx on ai_change_sets (organization_id, created_at desc);
create index ai_change_sets_live_idx on ai_change_sets (status) where status in ('draft', 'pending');
