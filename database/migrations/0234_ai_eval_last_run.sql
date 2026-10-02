-- REQ-107 · Agent evals and telemetry — slice 3: the denormalised last-run columns.
--
-- Slice 1 created `ai_eval_suites` with the fields on `SuiteRow` that read `last_pass_rate`,
-- `last_run_at` and `last_gate` as `null::numeric`, `null::timestamptz` and `null::text` in every
-- projection, and slice 2 created `ai_eval_runs`. This migration is what lets those projections
-- become real: three columns on the suite, one on the case, and nothing else.
--
-- ## Why the suite carries a copy of its last run's verdict
--
-- The honest answer to "what did this suite score last?" is a join against `ai_eval_runs`. That
-- join is what the suite *list* would need, and the suite list is the panel's home screen: it
-- renders every suite an organization has, on every load. The join is a lateral `order by
-- started_at desc limit 1` per row, so an installation with 200 suites runs 200 correlated
-- subqueries to fill a column that is, by construction, one row of a table that already has an
-- index on `(suite_id, started_at)`.
--
-- The alternative — a view — has the same cost and adds a second definition of "the last run"
-- that can drift from the one the detail screen uses.
--
-- So the values are **written when a run settles** and read as columns. The cost of that choice
-- is a denormalisation that can be stale, and the only thing that makes it safe is that the
-- writer is the runner: `eval_run::settle_run` is the single statement that moves a run out of
-- `queued`/`running`, and slice 3's executor calls it once, at the end, with the verdict the run
-- earned. A caller that writes a run row directly is already outside the contract (the store
-- refuses to settle a run twice), and the three columns are not read anywhere the detail screen
-- could disagree with — the detail screen reads the *run row itself*.
--
-- ## Why the suite's last gate is stored as text rather than derived
--
-- `gate` is `none` / `pass` / `block`. The suite list renders a badge from the last run's gate,
-- and the request's acceptance row is about a suite going **red** when a gate blocks. A blocked
-- gate that had to be recomputed from the baseline would need the baseline row *and* the run's
-- own rate to still be present; a baseline that was re-pointed at a newer run would then quietly
-- re-colour a suite that blocked against a different number. Storing the verdict as it was
-- concluded keeps the badge a fact about a run rather than a function of two rows that can move.
--
-- ## Why the case's last status is a column too
--
-- Same argument at one row's depth: the Cases tab shows a "last result" chip per case, and
-- `ai_eval_case_results` has `(case_id, id desc) where case_id is not null` — the index is
-- already there, but a per-row `limit 1` over *every run in history* is not a lookup a table
-- renders in 200 rows. `last_status` is therefore written by the same settle path, and the
-- Cases tab's chip is a stored fact about the most recent execution of that case.
--
-- ## What this migration deliberately does NOT add
--
-- No `last_cost_micros`. The suite list's cost-per-run column (if the panel later grows one) can
-- read the run row's own `cost_micros`, and a suite-level cost that is a copy of a run's number
-- is a second thing to keep in step for no reading the run row cannot answer.
--
-- No trigger. The runner is the only writer and it writes them in the same transaction as the
-- settle, so the invariant "a suite's last run is its last run" holds without a database rule
-- that would have to re-implement the settle's gate arithmetic to know what to copy.

alter table ai_eval_suites
    add column if not exists last_pass_rate numeric (5, 2),
    add column if not exists last_run_at timestamptz,
    add column if not exists last_gate text,
    add column if not exists last_run_id uuid references ai_eval_runs (id) on delete set null;

-- The gate vocabulary, constrained here rather than in a check inside the runner: a column that
-- only the runner writes is a column nobody validates, and `null` means "this suite has never
-- run" while `none` means "its last run asked for no gate". The two are different answers and
-- the column has to be able to hold both.
alter table ai_eval_suites
    add constraint ai_eval_suites_last_gate_known
    check (last_gate is null or last_gate in ('none', 'pass', 'block'));

alter table ai_eval_cases
    add column if not exists last_status text,
    add column if not exists last_run_at timestamptz;

alter table ai_eval_cases
    add constraint ai_eval_cases_last_status_known
    check (last_status is null or last_status in ('pass', 'fail', 'error', 'skipped'));

-- The suite list orders by recency and the "never run" suites sort last. With no index this is
-- a sort of every suite on every load, and the column being a `timestamptz` that is null for the
-- suites that have never run is exactly the case a partial index does not help with — so this is
-- a full index, and it is small: one entry per suite, not per run.
create index if not exists ai_eval_suites_last_run_idx
    on ai_eval_suites (organization_id, last_run_at desc nulls last);

-- The Cases tab reads a case's own last result, so the lookup is by case and by recency. The
-- index on `ai_eval_case_results (case_id, id desc)` already exists from slice 2; this one is on
-- the suite's cases so the tab's "group by last result" filter stays index-backed.
create index if not exists ai_eval_cases_last_status_idx
    on ai_eval_cases (suite_id, last_status)
    where last_status is not null;