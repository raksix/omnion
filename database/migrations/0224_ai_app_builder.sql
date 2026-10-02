-- Omnion · 0224 · AI app builder: the plan store
--
-- "Create an app to manage employees' leave requests" produces typed artifacts — entities,
-- fields, screens, permissions, a role, a workflow, notification templates and a report —
-- and "and the app is actually created" is a second, separate act (docs/requests/REQ-045).
-- These tables are that separation made physical: a generation writes **drafts**, and only
-- the apply runner ever writes to live tables.
--
-- The shape follows 0174 (the AI workflow builder's draft store) on purpose — a reviewer
-- should recognise it, and a rejected attempt must stay comparable to the one that replaced
-- it, which is why versions are kept rather than overwritten. Released migrations are
-- append-only (docs/05-VERSIONING.md).

-- One generation: the request, the plan it became, and where that plan stands.
create table app_builder_plans (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        references organizations (id) on delete cascade,
    site_id          uuid        references sites (id) on delete set null,
    prompt           text        not null,
    title            text        not null default '',
    status           text        not null default 'generating',
    -- Bumped by an explicit "keep this attempt, start another" (slice 4) and by every
    -- regeneration, so a rejected attempt can be compared against its replacement rather
    -- than being overwritten by it.
    plan_version     integer     not null default 1,
    -- Frozen at generation for the same reason as 0174's `model_key`: a registry change
    -- later must not silently rewrite what a decision was made about.
    model_label      text        not null default '',
    tokens_in        integer,
    tokens_out       integer,
    cost_cents       integer     not null default 0,
    error            text,
    created_by       uuid        references users (id) on delete set null,
    applied_at       timestamptz,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    -- What this plan is a retry of. `null` for a first attempt; self-referential so a chain
    -- of attempts stays walkable without a second table.
    supersedes_id    uuid        references app_builder_plans (id) on delete set null,
    constraint app_builder_plans_status_check
        check (status in ('generating', 'draft', 'approved', 'applying', 'applied',
                          'rejected', 'failed')),
    constraint app_builder_plans_prompt_check
        check (length(btrim(prompt)) between 3 and 4000),
    -- `applied_at` belongs to an applied plan and to nothing else: an audit line that reads
    -- "applied" beside a null timestamp is a question nobody can answer.
    constraint app_builder_plans_applied_check
        check (applied_at is null or status = 'applied'),
    constraint app_builder_plans_version_check check (plan_version >= 1),
    -- Token counts are never negative — a provider reporting one would otherwise be summed
    -- into a cost budget as if it were a spend. `null` (did not report) stays distinct from
    -- `0` (reported nothing).
    constraint app_builder_plans_tokens_check
        check ((tokens_in is null or tokens_in >= 0)
           and (tokens_out is null or tokens_out >= 0)),
    constraint app_builder_plans_cost_check check (cost_cents >= 0)
);

-- The console's own list: newest first, one organization.
create index app_builder_plans_org_idx
    on app_builder_plans (organization_id, created_at desc);

-- The plans an operator still has to do something about. Partial, because the list asks for
-- open plans far more often than for finished ones and `rejected` grows without bound.
create index app_builder_plans_open_idx
    on app_builder_plans (status, updated_at desc)
    where status in ('generating', 'draft', 'approved', 'applying');

-- One live chain of attempts per plan identity: an applied plan may never be superseded,
-- because something downstream points at its artifacts.
create unique index app_builder_plans_supersedes_key
    on app_builder_plans (supersedes_id)
    where supersedes_id is not null;

-- One artifact of one plan. `spec` is the artifact itself — entity, field, screen,
-- permission, role, workflow, notification or report — and `validation` is what the platform
-- said about it, so a reviewer reads the model's answer and the referee's answer side by side.
create table app_builder_artifacts (
    id           uuid        primary key default gen_random_uuid(),
    plan_id      uuid        not null references app_builder_plans (id) on delete cascade,
    kind         text        not null,
    -- Stable within (plan, kind): a field belongs to an entity, a screen to an entity, so a
    -- bare key can repeat across kinds without being ambiguous.
    key          text        not null,
    -- The artifact this one hangs off (a field's entity, a screen's entity); `null` at the
    -- top of the tree.
    parent_key   text,
    ordinal      integer     not null default 0,
    status       text        not null default 'pending',
    spec         jsonb       not null default '{}'::jsonb,
    -- The model's own explanation. Empty string rather than null: "no rationale" is a defect
    -- the reviewer must see, not an absence that reads like an omission.
    rationale    text        not null default '',
    validation   jsonb       not null default '[]'::jsonb,
    -- The attempt this artifact replaced, kept so a rejected version stays comparable.
    supersedes_id uuid       references app_builder_artifacts (id) on delete set null,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    constraint app_builder_artifacts_kind_check
        check (kind in ('entity', 'field', 'ui', 'permission', 'role',
                        'workflow', 'notification', 'report')),
    constraint app_builder_artifacts_status_check
        check (status in ('pending', 'accepted', 'rejected', 'edited', 'invalid')),
    -- A spec is an object or it is nothing: the validators read named fields out of it, and
    -- a scalar would fail there with a message about JSON's shape rather than about the
    -- artifact that is wrong.
    constraint app_builder_artifacts_spec_check check (jsonb_typeof(spec) = 'object'),
    -- Validation results are a list of findings: `{ path, message }`. An object would make
    -- "the first thing that is wrong with this artifact" unanswerable in SQL.
    constraint app_builder_artifacts_validation_check
        check (jsonb_typeof(validation) = 'array'),
    -- An `edited` artifact carries the edit: without a revision a reviewer's inline change
    -- and the model's proposal would be indistinguishable in the apply log.
    constraint app_builder_artifacts_edited_check
        check (status <> 'edited' or supersedes_id is not null),
    -- One artifact per (plan, kind, key). The regeneration flow writes the new row first and
    -- retires the old one, so a second live row for the same key is a bug, not a version.
    constraint app_builder_artifacts_unique_key unique (plan_id, kind, key)
);

create index app_builder_artifacts_plan_idx
    on app_builder_artifacts (plan_id, ordinal);

-- One apply run. Its steps are what the SSE stream narrates and what a rollback walks back in
-- reverse, so the set of things it created has to be recorded rather than inferred.
create table app_builder_applications (
    id          uuid        primary key default gen_random_uuid(),
    plan_id     uuid        not null references app_builder_plans (id) on delete cascade,
    status      text        not null default 'running',
    summary     jsonb       not null default '{}'::jsonb,
    -- Keys of the entities this run created. A rollback removes **only** these: an entity
    -- that existed before must survive a failed application untouched.
    created_entity_keys jsonb not null default '[]'::jsonb,
    applied_by  uuid        references users (id) on delete set null,
    started_at  timestamptz not null default now(),
    finished_at timestamptz,
    constraint app_builder_applications_status_check
        check (status in ('running', 'completed', 'failed', 'rolled_back')),
    constraint app_builder_applications_summary_check check (jsonb_typeof(summary) = 'object'),
    constraint app_builder_applications_entities_check
        check (jsonb_typeof(created_entity_keys) = 'array'),
    -- A finished run is one that stopped, and it stopped for a reason the reader can see.
    constraint app_builder_applications_finished_check
        check (finished_at is null or status in ('completed', 'failed', 'rolled_back'))
);

-- One application per plan. A second apply while one is running would race on the same
-- entity keys, and the row that lost would leave half of what it created behind.
create unique index app_builder_applications_plan_key
    on app_builder_applications (plan_id);

-- The step list the progress panel renders. `ordinal` is unique per application so the order
-- is the table's order rather than something a reader has to re-derive from a timestamp.
create table app_builder_application_steps (
    id             bigint generated always as identity primary key,
    application_id uuid        not null references app_builder_applications (id) on delete cascade,
    ordinal        integer     not null,
    kind           text        not null,
    label          text        not null,
    status         text        not null default 'queued',
    detail         jsonb       not null default '{}'::jsonb,
    started_at     timestamptz,
    finished_at    timestamptz,
    constraint app_builder_application_steps_ordinal_key unique (application_id, ordinal),
    constraint app_builder_application_steps_status_check
        check (status in ('queued', 'running', 'done', 'failed', 'skipped')),
    constraint app_builder_application_steps_detail_check check (jsonb_typeof(detail) = 'object'),
    -- A step that finished carries both ends of its life; a `queued` row with a finish time
    -- would make the progress list claim work that never ran.
    constraint app_builder_application_steps_life_check
        check ((finished_at is null or started_at is not null)
           and (status <> 'done' or finished_at is not null))
);

create index app_builder_application_steps_app_idx
    on app_builder_application_steps (application_id, ordinal);