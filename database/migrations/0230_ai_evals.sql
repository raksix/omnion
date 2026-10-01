-- REQ-107 · Agent evals and telemetry — slice 1: suites and cases.
--
-- Four of the request's six tables land here (suites, cases and their baselines are slice 1/3;
-- runs and case results are slice 2). The telemetry roll-up is slice 4 and is not here.
--
-- ## Why the suite row carries a target *and* nullable references
--
-- A suite targets "an agent, a copilot, a task kind or a bare model" and the request models that
-- as one `target` discriminator plus up to four nullable columns. The alternative — a single
-- `target_key` text column — is the shape that lets a suite name something that does not exist:
-- there is no foreign key to break, so a typo is a suite that fails at run time with "unknown
-- target" and no way for the editor to have caught it. The nullable references are what let the
-- database say it, so the constraint below is not decoration.
--
-- The constraint is the request's own rule made checkable: exactly the column the `target`
-- names is the one that is set, and nothing else is. `num_nonnulls` is the primitive that
-- expresses "exactly one of these four, and it is this one" without a trigger.

create table if not exists ai_eval_suites (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null,
    name text not null,
    description text not null default '',
    target text not null,
    agent_id uuid references ai_agents (id) on delete set null,
    copilot_key text,
    task text,
    model_id uuid references ai_models (id) on delete set null,
    temperature numeric (3, 2),
    tools jsonb not null default '[]'::jsonb,
    collections jsonb not null default '[]'::jsonb,
    threshold_percent integer not null default 90,
    max_regression_points numeric (5, 2) not null default 5,
    blocking boolean not null default false,
    schedule text,
    judge_model_id uuid references ai_models (id) on delete set null,
    judge_prompt text,
    judge_prompt_version integer not null default 1,
    enabled boolean not null default true,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now (),
    updated_at timestamptz not null default now (),
    constraint ai_eval_suites_target_known check (
        target in ('agent', 'copilot', 'task', 'model')
    ),
    -- The target is exactly the reference it claims, and the other three are empty. Without the
    -- second half a suite can be `target = 'agent'` with a model pinned as well, and the run
    -- layer has to guess which one the snapshot records.
    constraint ai_eval_suites_target_is_one check (
        (target = 'agent' and agent_id is not null and copilot_key is null and task is null and model_id is null)
        or (target = 'copilot' and copilot_key is not null and agent_id is null and task is null and model_id is null)
        or (target = 'task' and task is not null and agent_id is null and copilot_key is null and model_id is null)
        or (target = 'model' and model_id is not null and agent_id is null and copilot_key is null and task is null)
    ),
    constraint ai_eval_suites_threshold_range check (threshold_percent between 1 and 100),
    constraint ai_eval_suites_tolerance_range check (
        max_regression_points between 0 and 50
    ),
    -- A suite that is `model`-targeted carries its own model, and a judge must never be the
    -- model under test: the whole point of a second model is that it is not the thing being
    -- graded. A `blocking` suite with a rubric case cannot be verified here (a rubric lives on
    -- a case, not the suite), so the API owns that rule; what the column rule can own is the
    -- one that is a property of the row alone.
    constraint ai_eval_suites_judge_differs check (judge_model_id is null or judge_model_id is distinct from model_id),
    constraint ai_eval_suites_key_shape check (
        key ~ '^[a-z0-9][a-z0-9_-]{1,62}[a-z0-9]$'
    )
);

-- The key is the URL segment and the thing an operator types at the gate, so it is unique per
-- tenant rather than globally: two tenants both owning `support-replies` is normal, and a
-- globally unique key would make the second suite's save fail with a constraint violation that
-- names no field.
create unique index ai_eval_suites_org_key on ai_eval_suites (organization_id, key);
-- The suite list filters on the two things its status filter column carries, and both are in
-- one predicate so the list query and the schedule runner's "which enabled blocking suites
-- exist" question are the same index.
create index ai_eval_suites_org_enabled_blocking
    on ai_eval_suites (organization_id, enabled, blocking);

-- The request says a case is authored in the panel, imported from CSV, or captured from a real
-- run. The three sources are one column rather than a flag per source, because there is no
-- question the answer to which needs more than one of them.
create table if not exists ai_eval_cases (
    id uuid primary key default gen_random_uuid(),
    suite_id uuid not null references ai_eval_suites (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    -- The prompt / context the run replays. `input` rather than `prompt` because a case may
    -- carry a context document as well as a turn.
    input jsonb not null,
    -- The properties this case asserts. Validated by the API on the way in; the column is jsonb
    -- because the property set grows with the request's editor and a column per property would
    -- be a migration per property.
    expected jsonb not null,
    weight numeric (4, 2) not null default 1.00,
    tags text[] not null default '{}'::text[],
    enabled boolean not null default true,
    source text not null default 'manual',
    source_run_id uuid,
    created_at timestamptz not null default now (),
    updated_at timestamptz not null default now (),
    constraint ai_eval_cases_weight_range check (weight between 0.1 and 10),
    constraint ai_eval_cases_source_known check (
        source in ('manual', 'import', 'run')
    ),
    constraint ai_eval_cases_name_length check (char_length(name) between 1 and 80),
    -- A case that asserts nothing passes every model including a broken one, so the database
    -- refuses the empty document too. `jsonb_typeof` rather than a length check on the text,
    -- because `{"exact": ""}` is non-empty text and asserts something false.
    constraint ai_eval_cases_expects_something check (
        expected is not null and expected <> '{}'::jsonb
    )
);

-- The cases tab lists a suite's cases and only the enabled ones for a run; the run's per-case
-- rows join on name for the screen, which is why name is indexed too.
create index ai_eval_cases_suite_enabled on ai_eval_cases (suite_id, enabled);
create index ai_eval_cases_suite_name on ai_eval_cases (suite_id, name);
-- The request's risks section asks the panel to show coverage (cases per tag), and a GIN index
-- on the array is what makes "which cases carry this tag" an index scan rather than a scan of
-- every case in the installation.
create index ai_eval_cases_tags on ai_eval_cases using gin (tags);
