-- REQ-107 · Agent evals and telemetry — slice 2: runs, per-case results and baselines.
--
-- Slice 1 landed `ai_eval_suites` and `ai_eval_cases` (0230). This one lands the three tables a
-- run *is*: the run row with the snapshot that makes it reproducible, one result row per case,
-- and the baseline a gate compares against.
--
-- ## Why the snapshot is a column and not a join
--
-- The request is explicit: "storing the exact snapshot of what was tested (model, prompt, tool
-- list, temperature, judge model, judge prompt) so a result can be reproduced". Every one of
-- those values exists in another table today, so the cheap design is to join them at read time
-- and the design that survives an edit is to copy them into the run. Joining means a run from
-- last month re-reads today's model row: the panel would show a pass rate earned by one prompt
-- next to a snapshot describing another, and the whole point of an eval is that the number can be
-- traced to the thing that produced it. The copy is one `jsonb` column and no update path.
--
-- ## Why `pass_rate` is a stored number and not a view
--
-- It is the weighted pass share (the request says so, and the acceptance row demands a fixture
-- with unequal weights). Computing it in a view would be correct and would also be
-- unreproducible: the weights live on the case rows, so editing a case weight would restate a
-- run that already happened. The number is computed once, at settle time, and never again.
--
-- ## Why `gate` defaults to 'none' rather than NULL
--
-- A `gate` is a verdict, and the three states are "no gate was asked for", "the gate passed" and
-- "the gate blocked". `NULL` would mean the first and `not_run` would mean the same thing, and a
-- partial index on `gate = 'block'` is the alert query. The run's own `kind` already says
-- whether a gate was asked for; `gate` says what it concluded, and 'none' is a real answer.
--
-- ## The two partial indexes are the queries that must not go sequential
--
-- The runner claims a queued run with `for update skip locked` over the whole table, and the
-- alert query is "every run that ever blocked, newest first". Both would degrade with history:
-- a suite with ten thousand `passed` runs would put ten thousand rows in front of the one that
-- is queued. The statuses are few and known, so the index can be partial and stay small.

create table if not exists ai_eval_runs (
    id uuid primary key default gen_random_uuid(),
    suite_id uuid not null references ai_eval_suites (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    -- manual / scheduled / gate. A check rather than an enum: the request names the three, and a
    -- new kind is a migration anyone can read, not a type change that rewrites the table.
    kind text not null default 'manual',
    -- queued / running / passed / failed / error / cancelled. `passed`/`failed` are the run's
    -- own verdict against its threshold, so a suite with no gate still gets one of the two.
    status text not null default 'queued',
    -- The reproduction data. See the header.
    snapshot jsonb not null default '{}'::jsonb,
    -- The model under test as the router settled on it, which is a *registry row id* and not
    -- the `provider/model` string the snapshot holds: the panel links the row, the snapshot is
    -- what a later reader needs when the row is gone.
    model_id uuid references ai_models (id) on delete set null,
    judge_model_id uuid references ai_models (id) on delete set null,
    total_cases integer not null default 0,
    passed_cases integer not null default 0,
    failed_cases integer not null default 0,
    error_cases integer not null default 0,
    -- The weighted pass share, 0–100. NULL until the run settles: a queued run has no rate, and
    -- writing 0 would put a suite that has never run at 0% rather than at "never run".
    pass_rate numeric (5, 2),
    -- The threshold this run was judged against, copied from the suite so a later threshold
    -- edit cannot restate an old verdict.
    threshold_percent integer not null default 90,
    -- none / pass / block. See the header.
    gate text not null default 'none',
    -- The baseline this run was compared to, when a gate or a diff asked for one.
    base_run_id uuid references ai_eval_runs (id) on delete set null,
    cost_micros bigint not null default 0,
    duration_ms integer,
    triggered_by uuid references users (id) on delete set null,
    error text,
    started_at timestamptz not null default now (),
    finished_at timestamptz,
    created_at timestamptz not null default now (),
    updated_at timestamptz not null default now (),
    constraint ai_eval_runs_kind_known check (kind in ('manual', 'scheduled', 'gate')),
    constraint ai_eval_runs_status_known check (
        status in ('queued', 'running', 'passed', 'failed', 'error', 'cancelled')
    ),
    constraint ai_eval_runs_gate_known check (gate in ('none', 'pass', 'block')),
    -- A run's counters have to add up. Without this a settle that wrote 9 of 10 results and
    -- then died would leave a run claiming 100% over 10 cases, and the pass rate — the one
    -- number a gate reads — would be computed over a set the row does not describe.
    constraint ai_eval_runs_counts_agree check (
        total_cases = passed_cases + failed_cases + error_cases
    ),
    -- The rate and the counters are the same fact, so they are checked against each other
    -- rather than trusted separately. `total_cases = 0` is the run that found nothing to do,
    -- which the suite rules refuse at save time; the guard is `>= 0` so an empty run with a
    -- null rate is still legal.
    constraint ai_eval_runs_rate_in_range check (pass_rate is null or pass_rate >= 0 and pass_rate <= 100),
    -- A settled run has an end. `finished_at` without a terminal status would make the
    -- history list show an elapsed duration on a run that never stopped.
    constraint ai_eval_runs_finished_is_terminal check (
        finished_at is null or status in ('passed', 'failed', 'error', 'cancelled')
    ),
    -- A run cannot be its own baseline. Cheap, and the diff would otherwise divide by itself.
    constraint ai_eval_runs_not_own_baseline check (base_run_id is null or base_run_id <> id)
);

-- The runner's claim query: `where status in ('queued') order by created_at` under
-- `for update skip locked`.
create index if not exists ai_eval_runs_claim_idx
    on ai_eval_runs (created_at)
    where status = 'queued';

-- The alert query: everything that ever blocked, newest first, for the notification centre.
create index if not exists ai_eval_runs_blocked_idx
    on ai_eval_runs (organization_id, started_at desc)
    where gate = 'block';

-- The run list, filtered by suite then newest — the History tab's default query.
create index if not exists ai_eval_runs_suite_idx
    on ai_eval_runs (suite_id, started_at desc);

-- The run list without a suite filter.
create index if not exists ai_eval_runs_org_idx
    on ai_eval_runs (organization_id, started_at desc);

-- One result row per case per run. `case_id` is `set null` rather than cascade: a case deleted
-- between two runs must not delete the *history* of a run that already used it, and the
-- `case_name` below is why the result stays readable.
create table if not exists ai_eval_case_results (
    id bigserial primary key,
    run_id uuid not null references ai_eval_runs (id) on delete cascade,
    case_id uuid references ai_eval_cases (id) on delete set null,
    -- Copied, like the snapshot: a result whose label is a join to a case that has since been
    -- renamed would report the run under a name the run never tested.
    case_name text not null default '',
    status text not null default 'fail',
    -- The unweighted share of checks that held, 0–1. The run's `pass_rate` is the weighted
    -- share over cases; this is the per-case fraction of properties, and mixing the two is
    -- the kind of number that looks like a bug and is.
    score numeric (5, 4),
    -- `[{property, passed, detail}]`, exactly the shape `eval_case::CheckResult` serialises to.
    checks jsonb not null default '[]'::jsonb,
    -- The judge's own sentence, stored verbatim. The request asks for it twice (the case
    -- result and the run header) and a paraphrase of a model's reasoning is not a record.
    judge_reason text,
    -- The output the properties were checked against, clipped by the store.
    output text,
    latency_ms integer,
    prompt_tokens integer,
    completion_tokens integer,
    cost_micros bigint not null default 0,
    -- `[{tool, ok, detail}]` as the run layer recorded it.
    tool_calls jsonb not null default '[]'::jsonb,
    -- Set when the case could not be executed at all, as opposed to failing a check.
    error text,
    created_at timestamptz not null default now (),
    constraint ai_eval_case_results_status_known check (
        status in ('pass', 'fail', 'error', 'skipped')
    ),
    constraint ai_eval_case_results_score_in_range check (score is null or score >= 0 and score <= 1),
    -- A result whose run is gone is a leak, not a history: cascade is right, and it is the only
    -- foreign key in this request that deletes rows the operator did not ask to lose.
    constraint ai_eval_case_results_have_case_or_name check (case_id is not null or case_name <> '')
);

create index if not exists ai_eval_case_results_run_idx on ai_eval_case_results (run_id);
create index if not exists ai_eval_case_results_failed_idx
    on ai_eval_case_results (run_id)
    where status <> 'pass';

-- The "last result" column on a suite's case list is a per-case lookup across every run, and
-- without this the cases tab is one query per row.
create index if not exists ai_eval_case_results_case_idx
    on ai_eval_case_results (case_id, id desc)
    where case_id is not null;

-- The baseline is one row per suite: a suite has at most one baseline, and the row is a
-- pointer plus the rate it was taken at so the regression maths does not have to read the
-- baseline run's own results (which may be pruned).
create table if not exists ai_eval_baselines (
    suite_id uuid primary key references ai_eval_suites (id) on delete cascade,
    run_id uuid not null references ai_eval_runs (id) on delete cascade,
    -- The rate at the moment the baseline was set. See the header note on `pass_rate` above:
    -- the same reason, applied to the number a regression is measured against.
    pass_rate numeric (5, 2) not null,
    set_by uuid references users (id) on delete set null,
    set_at timestamptz not null default now (),
    constraint ai_eval_baselines_rate_in_range check (pass_rate >= 0 and pass_rate <= 100)
);

-- ## What a fresh database still cannot answer
--
-- `/ai/telemetry`'s roll-up (`ai_tool_stats_daily`) is slice 4 and is not here, so the telemetry
-- screen's tool table has no source until then. The telemetry route is deliberately NOT mounted
-- in this slice for the same reason the run route was not mounted in slice 1: a route that
-- answers with an empty table is a screen that measures nothing, which is worse than a 404.
