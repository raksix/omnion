# REQ-107 — Agent Evals & Telemetry

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Proving the agents actually work, and keep working.

- Eval suites: input sets with expected properties, run on demand and on schedule.
- Scoring (exact match, rubric, LLM-as-judge with a second model), pass rate trend.
- Regression gate: a model/prompt change must not drop the pass rate below a threshold.
- Telemetry: per-tool success rate, step counts, latency percentiles, cost per solved task.
- Panel screens for suites, runs, diffs between prompt/model versions.

## Implementation spec

### Scope (in / out)

**In** — a way to answer "is this agent still good?" with numbers, from the same runtime that serves
production traffic.

- **Suites** — a suite targets something concrete: an agent (REQ-001), a copilot (REQ-103), a task
  kind (summarize, seo, translate, …) or a bare model. It carries the run configuration (model or
  agent snapshot, temperature, tool allow-list, knowledge collections), a pass threshold, an
  optional regression tolerance, a blocking flag and an optional schedule.
- **Cases** — one row per input with its expected properties: `exact` match, `contains`, `regex`,
  `json_schema`, `citations_required`, `no_pii` (delegates to REQ-105's detector), `max_steps`,
  `max_cost_micros`, `max_latency_ms`, `rubric` (free text judged by a model). A case may name
  several properties; all must hold. Cases are authored in the panel, imported from a CSV, or
  captured from a real run ("save this run as a case" — the prompt, context and tool set are
  snapshotted, the expected properties are filled in by hand).
- **Scoring** — deterministic checks run in-process; `rubric` and free-form quality scoring use
  LLM-as-judge with a *second* model: the judge model must differ from the model under test (the API
  refuses the combination), the judge prompt is versioned and pinned on the run, and the judge's own
  tokens and cost are recorded under feature `eval:judge` so eval spend is visible in REQ-104.
- **Runs** — one run per execution, storing the exact snapshot of what was tested (model, prompt,
  tool list, temperature, judge model, judge prompt) so a result can be reproduced; per-case results
  with status, score, output, judge reasoning, latency, tokens, cost and tool calls.
- **Regression gate** — a suite can be marked blocking with `threshold_percent` and
  `max_regression_points`. The gate runs on demand from the panel and is what a prompt or model
  change should call before promotion; a failing gate writes a run with `gate = 'block'`, emits
  `ai.eval.regression.detected` and shows red on the suite. The gate is advisory for humans and a
  hard stop for automation that calls it.
- **Telemetry** — per-tool success rate, denial rate and latency percentiles (p50/p95/p99) over
  agent and copilot runs, step counts per solved task, cost and tokens per solved task, and a
  "solved" definition stated on the screen (a run that completed with no failed step and produced a
  result the caller accepted).

**Out**

- Training, fine-tuning and dataset labelling workflows.
- Traffic-split A/B testing in production; the eval run is the comparison mechanism in v1, with the
  same snapshot fields available for a later splitter.
- Simulated environments beyond real tools with a dry-run flag (REQ-108's sandbox supplies that for
  MCP calls).
- Human preference collection at scale: per-message feedback (REQ-103) is the input, a labelling
  queue is not part of this request.

### Screens (UI)

- **`/ai/evals`** — suite cards/rows: Key, Name, Target, Cases, Last run, Pass rate, Threshold, Gate,
  Schedule, Status. Header stat cards (suites, runs 7d, average pass rate, cost 7d). Filters target,
  status, blocking, free text. Actions Run now, Duplicate, Enable/Disable, Delete (confirm by key),
  New suite.
- **`/ai/evals/[key]`** — tabs **Cases**, **Config**, **History**. Cases table: Name, Input preview
  (≤60 chars), Properties (chips), Weight, Tags, Enabled, Last result. Row actions Edit, Run this
  case, Duplicate, Delete. Case form: Name (1–80), Input (prompt/context jsonb editor with a
  plain-text mode, ≤16000 chars), Expected properties (checkbox group revealing the matching field
  per property), Weight (0.1–10), Tags, Enabled. Config tab: target picker, model or agent select,
  temperature, tools multi-select, collections multi-select, threshold (1–100), regression tolerance
  (0–50 points), blocking switch, schedule (cron presets: hourly, daily, weekly, custom), judge model
  (must differ from the model under test — the select hides the tested model and the API refuses it),
  judge prompt (≤8000). Validation is field-level with the API repeating every rule.
- **`/ai/evals/runs`** — table: Started, Suite, Kind (manual/scheduled/gate), Model, Passed/Total, Pass
  rate, Threshold, Gate, Cost, Duration, Status. Filters suite, status, kind, range, user.
- **`/ai/evals/runs/[id]`** — run header (snapshot: model, prompt version, tools, judge) with a gate
  verdict badge, then the case table: Case, Status, Score, Judge reason (expandable), Latency,
  Tokens, Cost, Tool calls, Output (expandable). A "Diff against…" control picks a baseline run and
  marks rows as improved, unchanged or regressed, with a summary line (x improved, y regressed,
  z unchanged). Row actions Re-run this case, Save as case, Copy output.
- **`/ai/telemetry`** — panels: tool table (Tool, Calls, Success %, Denied, Error codes, p50, p95,
  p99), a step-count histogram, a scatter of cost per solved task over time, and a table of the
  costliest failing tool per day. Range picker, filters organization/site/agent/copilot, and a link
  from each row to `/ai/logs` pre-filtered by that tool's requests.
- **Keyboard** — `/` focuses search, `N` new suite/case, `R` runs the focused suite, `D` opens the
  diff picker, `G` then `E` goes to evals, `↑/↓` + `Enter` move and open a row, `Esc` closes.
- **Mobile (<1024px)** — suite rows become cards with the pass rate and gate badge leading, the case
  editor becomes a single-column stack with the properties as a sheet, the run table becomes cards,
  the diff view becomes a vertical list with before/after blocks, and the telemetry tables become
  labelled cards per tool.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/ai/evals/suites` | List / create suites | `ai.evals.read` / `ai.evals.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/evals/suites/{key}` | Read / change / remove a suite | `ai.evals.read` / `ai.evals.manage` |
| GET/POST | `/api/v1/ai/evals/suites/{key}/cases` | List / create cases | `ai.evals.read` / `ai.evals.manage` |
| PATCH/DELETE | `/api/v1/ai/evals/cases/{id}` | Change / remove a case | `ai.evals.manage` |
| POST | `/api/v1/ai/evals/suites/{key}/import` | Import cases from CSV | `ai.evals.manage` |
| POST | `/api/v1/ai/evals/suites/{key}/run` | Start a run (kind `manual` or `gate`) | `ai.evals.run` |
| GET | `/api/v1/ai/evals/runs` | Run history with filters | `ai.evals.read` |
| GET | `/api/v1/ai/evals/runs/{id}` | One run with per-case results | `ai.evals.read` |
| GET | `/api/v1/ai/evals/runs/{id}/diff` | Diff against a baseline run (`base`) | `ai.evals.read` |
| POST | `/api/v1/ai/evals/runs/{id}/cancel` | Stop a running suite | `ai.evals.run` |
| GET | `/api/v1/ai/telemetry/tools` | Tool success, denial and latency stats | `ai.telemetry.read` |

New catalogue keys: `ai.evals.read`, `ai.evals.manage`, `ai.evals.run`, `ai.telemetry.read`. A suite
that needs a tool the caller lacks still runs — the *run* uses the suite owner's granted tool set,
which is recorded on the snapshot and visible in the run header, and the panel says whose authority
was used.

### Data model

Migration `database/migrations/00NN_ai_evals.sql` (00NN = next free integer at land time; 0021 was
free when this was written). Runs write their own `ai_usage` rows with `feature = 'eval'` /
`'eval:judge'`, so eval cost never hides inside another feature's total.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_eval_suites` | id uuid pk, organization_id uuid → organizations cascade, key text, name text, description text default '', target text ('agent','copilot','task','model'), agent_id uuid null → ai_agents set null, copilot_key text null, task text null, model_id uuid null → ai_models set null, temperature numeric(3,2) null, tools jsonb default '[]', collections jsonb default '[]', threshold_percent int default 90 check (1–100), max_regression_points numeric(5,2) default 5, blocking bool default false, schedule text null, judge_model_id uuid null → ai_models set null, judge_prompt text null, judge_prompt_version int default 1, enabled bool default true, created_by uuid null → users set null, created_at, updated_at | unique `(organization_id, key)`; `(organization_id, enabled, blocking)` |
| `ai_eval_cases` | id uuid pk, suite_id uuid → cascade, organization_id, name text, input jsonb, expected jsonb, weight numeric(4,2) default 1.00 check (0.1–10), tags text[] default '{}', enabled bool default true, source text default 'manual' ('manual','import','run'), source_run_id uuid null, created_at, updated_at | `(suite_id, enabled)`; `(suite_id, name)`; `(tags)` gin |
| `ai_eval_runs` | id uuid pk, suite_id uuid → ai_eval_suites cascade, organization_id, kind text ('manual','scheduled','gate'), status text ('queued','running','passed','failed','error','cancelled'), snapshot jsonb (model, prompt, prompt version, tools, temperature, judge model, judge prompt, authority user), model_id uuid null, judge_model_id uuid null, total_cases int, passed_cases int, failed_cases int, error_cases int, pass_rate numeric(5,2), threshold_percent int, gate text ('none','pass','block'), base_run_id uuid null, cost_micros bigint default 0, duration_ms int null, triggered_by uuid null → users set null, error text null, started_at, finished_at | `(suite_id, started_at desc)`; `(organization_id, started_at desc)`; `(status)` where status in ('queued','running'); `(organization_id, gate)` where gate = 'block' |
| `ai_eval_case_results` | id bigserial pk, run_id uuid → cascade, case_id uuid null → ai_eval_cases set null, case_name text, status text ('pass','fail','error','skipped'), score numeric(5,2) null, checks jsonb default '[]' (`[{property, passed, detail}]`), judge_reason text null, output text null, latency_ms int null, prompt_tokens int, completion_tokens int, cost_micros bigint, tool_calls jsonb default '[]', error text null | `(run_id, status)`; `(run_id, case_name)`; `(case_id, id desc)` |
| `ai_tool_stats_daily` | day date, organization_id uuid, tool text, calls int, successes int, failures int, denials int, p50_ms int, p95_ms int, p99_ms int, cost_micros bigint, refreshed_at | pk `(day, organization_id, tool)`; `(organization_id, day desc)` |
| `ai_eval_baselines` | suite_id uuid pk → ai_eval_suites cascade, run_id uuid → ai_eval_runs cascade, pass_rate numeric(5,2), set_by uuid null → users set null, set_at | pk only |

Suite runs go through the existing runner pattern in `apps/api`: the run row is created and claimed,
cases execute with a bounded concurrency, results are written in batches, and a run stuck in
`running` past a timeout is failed by the runner with a reason rather than left as a ghost.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.eval.run.started` / `.completed` / `.failed` | emitted | suite, kind, pass rate, duration — a scheduled run can drive notifications |
| `ai.eval.gate.passed` / `.blocked` | emitted | suite, pass rate, threshold, baseline — the promotion signal for automation |
| `ai.eval.regression.detected` | emitted | suite, baseline run, regressed case names — the alert that matters |
| `ai.telemetry.tool.degraded` | emitted | tool, success rate below its trailing baseline — an early warning to pair with REQ-021 |

### Acceptance criteria

- [ ] Creating a suite with `blocking = true` requires a threshold and a judge model when any case carries a `rubric` property; the API refuses the incomplete combination naming the field.
- [ ] A run executes every enabled case, writes one `ai_eval_case_results` row per case and computes `pass_rate` as the weighted pass share (asserted by a fixture with unequal weights).
- [ ] A `rubric` case records the judge model, the judge prompt version and the judge's reasoning, and its cost appears under `eval:judge` on `/ai/costs`.
- [ ] `no_pii` fails a case whose output contains a value REQ-105's detector would mask (stub output), and passes a clean one.
- [ ] A gate run whose pass rate is below the threshold writes `gate = 'block'`, shows red on the suite and emits `ai.eval.gate.blocked`.
- [ ] A run that drops more than the tolerance against its baseline marks the regressed cases in the diff view and emits `ai.eval.regression.detected` with their names.
- [ ] A scheduled suite runs on its schedule without an open browser session (verified by advancing the clock in a test harness), and a second run is not started while one is `running`.
- [ ] A run stuck beyond the timeout is failed by the runner with a reason and a `status = 'failed'` row, not left `running`.
- [ ] Cancelling a running suite stops remaining cases and marks the run `cancelled` with the partial results kept.
- [ ] `/ai/telemetry` tool stats match the underlying run steps for the same range (asserted against SQL for successes and denials).
- [ ] Organization A cannot read organization B's suites, runs or results (404 on a direct id).
- [ ] A caller without `ai.evals.run` sees Run now disabled with the permission named; the API answers 403 for the same call.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: create a suite targeting a copilot, add three cases (one exact, one
`json_schema`, one `rubric` with a judge model), try to save a blocking suite without a judge (field
error), then save it valid; run it and watch the run row move from queued to running to a verdict;
open the run, expand a failing case's judge reasoning and checks, and re-run that case; save a
failing case as a new case and confirm it appears with `source = 'run'`; run the suite again with a
baseline selected and read the diff summary; lower the threshold to force a `block` gate and read the
red state plus the notification; cancel a third run mid-flight; import three cases from a CSV
(including one malformed row, which must be reported by line); open `/ai/telemetry` and compare one
tool's success count with the filtered `/ai/logs` view; check the scheduled run appears after the
scheduler fires.

The visual check must see: pass-rate bars with the threshold line visible, gate badges readable
without colour alone, the diff view's before/after columns aligned and scrollable, no clipped judge
reasoning, the telemetry percentiles right-aligned, no raw i18n keys, and a mobile pass (390×844)
over the suite list, the case editor, a run and the telemetry panels.

### Slices

1. **Suites, cases and scoring** — `ai_eval_suites`, `ai_eval_cases`, the eight deterministic
   properties, the suite and case screens, CSV import and save-as-case.
   *Done when:* a suite with cases runs on demand and every property is provable by a test fixture.
2. **Judge scoring, runs and results** — the judge path with model-difference enforcement and
   versioned prompts, `ai_eval_runs` and `ai_eval_case_results`, run list and run detail screens.
   *Done when:* a rubric case produces a reasoned verdict and its cost lands under `eval:judge`.
3. **Gates, diffs and scheduling** — blocking gates, tolerances, baselines, the diff view,
   `ai.eval_baselines`, the scheduler and runner timeouts, gate events.
   *Done when:* a regression is caught before promotion, quoted by case name, and a stuck run is
   failed rather than abandoned.
4. **Telemetry and polish** — `ai_tool_stats_daily`, the roll-up runner, `/ai/telemetry`, degraded
   tool events, empty/loading/error states, mobile layouts.
   *Done when:* tool stats reconcile with the log table and both widths pass the visual check.

### Risks / notes

- Eval results are only as good as the cases: a suite of ten easy cases passes everything. The
  panel should show coverage (cases per tag, last failing case) and the docs should push toward
  capturing real failures as cases.
- LLM judging is itself unreliable: pin the judge model and prompt, record the reasoning, and never
  let a single rubric failure be the only evidence for a regression claim.
- Gates must not block a hotfix path: the gate returns a verdict and an operator can override it with
  a recorded reason; an override is an event, not a silent bypass.
