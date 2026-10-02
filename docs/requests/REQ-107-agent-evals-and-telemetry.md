# REQ-107 — Agent Evals & Telemetry

> **Status:** in-progress (slice 1: the scoring core, the suite/case tables, the store and the
> validation the first acceptance row demands — `de3a1529` the pure scorer, `ddbd42a3` the store,
> `c439548b` the twelve database walks and the two bugs they found; then the HTTP surface and the
> four catalogue keys — `98e18319`; then the screens and the walkthrough pass that drives them —
> `00755aee`; slice 6a the announcements: `ai.eval.gate.blocked` is announced once with the settled
> rate and `gate.passed` is not, and `regressed_cases` names the cases that regressed —
> `843d3cc9`, falsified against the pre-fix body (`["alpha","beta","gamma"]` vs `["beta"]`). Slice 6b the live seam: `caa4fa5b` — the rubric walk found five production defects (the pin never decided the resolve, a default suite could not be called at all, every judge was unreachable, unpriced eval calls); two walks plus a control, falsified against the pre-fix body. Slice 2: the routes mounted and the run permission split from the read one —
> `e154d732`; the run history, the run detail, the mandatory baseline picker and a `Run now` that
> works — `ed03200f`. Slice 3: the runner that claims, scores and settles — `718a73f9`, then the
> model-under-test reader that makes a suite's pin mean something — `4a1c978a`. Slice 4 (store,
> route and the writer): `0237_ai_tool_stats_daily.sql` and the roll-up — `eae9d344`; the
> histogram read that was silently returning "no failures" — `9572d7db`; `/ai/telemetry/tools`
> behind its own `ai.telemetry.read` — `7e7ec6ec`. Seven of thirteen acceptance rows are ticked,
> each naming its walk. Slice 4's route split: the boot panic from a duplicate `GET
> `/ai/telemetry/tools` — `cf1c80cb` — and the uniqueness tripwire that catches the class, after
> its own first version was proven blind — `c525d9d5`. Then the writer the roll-up never had, and
> the `ai.telemetry.tool.degraded` alert nothing emitted — `ab029fe9`: `refresh_day`'s only caller
> was its own walk, so the table this slice's screen reads was empty and would have stayed empty
> forever. **Slice 5 (the screen, and the two panels its spec promised the API could not answer) —
> `aaa0b4a2` and `4807b518`: `step_histogram` and `cost_per_solved` over live `ai_runs`, the
> `/ai/telemetry` screen, its page in the walkthrough routes, and a depth pass that seeds rows,
> rolls the day and reads the numbers back.** Four acceptance rows remain open and are named below
> slice 6c the two acceptance rows that needed a caller rather than a row: `no_pii` against the
> seeded guard at the run level, and `ai.evals.run`'s split read over HTTP from one session — whose
> counter-case found a `model_key_of` that asked sqlx for two columns out of one expression, so
> every model-targeted run answered 500.
> **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
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

- [x] Creating a suite with `blocking = true` requires a threshold and a judge model when any case carries a `rubric` property; the API refuses the incomplete combination naming the field.  <!-- proved: ai_evals.rs: a_blocking_suite_with_a_rubric_case_needs_a_judge_and_the_rule_lands_on_edit -->
- [x] A run executes every enabled case, writes one `ai_eval_case_results` row per case and computes `pass_rate` as the weighted pass share (asserted by a fixture with unequal weights).  <!-- proved: ai_eval_runner.rs: a_tick_claims_one_run_scores_every_case_and_weighs_the_verdict -->
- [x] A `rubric` case records the judge model, the judge prompt version and the judge's reasoning, and its cost appears under `eval:judge` on `/ai/costs`. <!-- proved, and writing it found five production defects on the seam only `LiveTurn` touches. Every other walk drives `ScriptedTurn`, which writes no usage rows and builds no `ChatRequest`, so this path was implemented, reached by `spawn`, and unproven. (1) THE PIN NEVER DECIDED ANYTHING: `resolve_and_record` takes a model in two places -- `DecisionContext.requested` (stored on the row) and its sixth parameter (which builds `ResolveRequest.explicit`, the field `decide` reads before any map). `build_turn` set the pin in the context and passed `None` for the parameter, so every eval run graded the installation default while its snapshot and `requested` column both recorded the suite's pin: the log agreed with the snapshot and both were wrong about the call. (2) A DEFAULT SUITE COULD NOT BE CALLED AT ALL: `DEFAULT_SYSTEM_PROMPT` is empty by design (the snapshot is the reproduction data), but `case_messages` emitted the message anyway and `validate_request` refuses a blank one, so every suite that pinned no prompt settled `error` with "the model could not be called" -- which reads as a provider outage. (3) EVERY JUDGE WAS UNREACHABLE: `resolve_judge_target` selected six columns into a sixteen-field struct, `query_as` failed, `.ok().flatten()` made it `None`, and the caller's own comment said that makes a rubric case `error`. Now `store::find_provider_by_name`, the same read the model under test uses. (4) The judge read as unconfigured when it was unreachable (a bare `.ok()?`); it logs the reason now. (5) EVERY EVAL CALL WAS UNPRICED: `price_for` is keyed on `model_key` and the runner passed `provider/model`, so the lookup missed and `record_usage` stored a null cost beside real numbers on the run and case rows -- which is why chat was priced and eval was not. Split on the FIRST slash so a local `models/<file>.gguf` survives. Walks: `a_rubric_runs_judge_and_bill_the_grading_to_eval_judge` (judge row carries `eval:judge` under the judge's own provider and model key with a non-zero total) and the control `a_run_with_no_judge_records_no_judge_spend`. Falsified before committing: red on the pre-fix body. --> 
- [x] `no_pii` fails a case whose output contains a value REQ-105's detector would mask (stub output), and passes a clean one.  <!-- proved: ai_eval_runner.rs: a_no_pii_case_fails_on_what_the_data_guard_would_mask_and_passes_a_clean_one + a_no_pii_case_the_guard_never_ran_is_an_error_and_not_a_pass. The walk drives the runner's own `load_guard` seam against the SEEDED platform rules, because the row says "a value REQ-105's detector would mask" — a suite carrying its own regex would drift from the guard production applies to outbound text and could pass while production masked the same output. One provider answers both cases, keyed on the question it was asked, so the rows differ because the model behaved differently. The control breaks the rule set the way production does (an uncompilable pattern) and demands `error`: a property that could not be measured must never read as a satisfied bar in a promotion gate. Falsified against the pre-fix body (`guard_outcome = None`): red with {passed: 0, failed: 0, errors: 2}, green after. -->
- [x] A gate run whose pass rate is below the threshold writes `gate = 'block'`, shows red on the suite and emits `ai.eval.gate.blocked`. <!-- proved: ai_eval_runner.rs: a_blocked_gate_is_announced_with_the_run_it_is_about. The row is about the events table, not the run row: `announce()` returned nothing, so a runner that stopped emitting the promotion signal -- or emitted `gate.passed` for a blocked run -- left every other walk green. The payload is compared against the SETTLED row, not the fixture's inputs, so a runner computing the rate independently is caught disagreeing with the row it announces. `total_cases` is `Verdict::total()` (passed + failed + errors): a payload omitting the errored cases reports a smaller suite than the one that ran, which reads as a clean run rather than a lossy one. --> 
- [x] A run that drops more than the tolerance against its baseline marks the regressed cases in the diff view and emits `ai.eval.regression.detected` with their names. <!-- proved, and the defect was real: `announce()` received `&enabled` -- every case the suite ran -- and published all their names under `regressed_cases`. Structurally that satisfies the field: every name is a real case, so a subscriber re-running them cannot tell it re-ran the whole suite. A suite of 80 with one broken case announced 80 names, and the one real finding is the one somebody filters out. The names now come from `diff_runs` over the STORED rows of the baseline and this run -- the rows the diff view itself reads, so the announcement and the view cannot disagree. Falsified before committing (production file stashed, walks kept): the walk is red on the pre-fix body with left `["alpha","beta","gamma"]` vs right `["beta"]`, green after. The expectation is derived from `diff_runs` rather than hand-written SQL on purpose: the first version used `status = 'passed'` in a schema whose vocabulary is `pass`/`fail`/`error`/`skipped`, matched nothing, and failed on its own fixture. --> 
- [x] A scheduled suite runs on its schedule without an open browser session (verified by advancing the clock in a test harness), and a second run is not started while one is `running`.  <!-- proved: ai_eval_runner.rs: a_schedule_fires_on_its_minute_and_only_once_inside_it + a_suite_with_a_run_in_flight_is_not_queued_again -->
- [x] A run stuck beyond the timeout is failed by the runner with a reason and a `status = 'failed'` row, not left `running`.  <!-- proved: ai_eval_runner.rs: a_stale_run_is_failed_by_the_reaper_with_a_reason -->
- [x] Cancelling a running suite stops remaining cases and marks the run `cancelled` with the partial results kept.  <!-- proved: ai_eval_runner.rs: a_cancelled_run_keeps_the_results_it_already_wrote + ai_eval_runs.rs: a_cancel_keeps_the_partial_results_and_a_second_cancel_is_a_conflict -->
- [x] `/ai/telemetry` tool stats match the underlying run steps for the same range (asserted against SQL for successes and denials).  <!-- proved: tool_stats.rs: the_rollup_reconciles_with_the_calls_it_summarises (counts taken from ai_tool_calls by a separate query); and, from ab029fe9, ai_telemetry_runner.rs: the_tick_writes_the_roll_up_and_re_rolling_it_does_not_double_count -- the store's walk calls refresh_day itself, so it could not see that nothing in production called it -->
- [x] Organization A cannot read organization B's suites, runs or results (404 on a direct id).  <!-- proved: ai_evals.rs: another_tenants_suite_is_not_found_and_never_forbidden + ai_eval_runs.rs: another_tenants_settled_run_is_not_found_and_a_cancel_never_confirms_it_exists + tool_stats.rs: a_tenant_reads_only_its_own_tool_numbers -->
- [x] A caller without `ai.evals.run` sees Run now disabled with the permission named; the API answers 403 for the same call.  <!-- proved: ai_eval_run_permission.rs, a new HTTP suite that drives the router. The existing source-level walk (`the_disabled_reason_names_a_key_the_mount_actually_guards`) proves the two key LISTS agree but cannot prove either list is right: a panel that disables nothing and a mount that checks nothing agree perfectly and both stay green. The fixture is a custom role carrying exactly `ai.evals.read`, because no seeded role reaches the interesting middle — Owner holds every catalogue key and `ai.evals.*` is in no other base role's list. Both halves are read from ONE session: the list answers 200, `viewer_missing` names `ai.evals.run` and NOT the read key it holds, the run answers 403 with `permission_denied` (not `csrf_failed`, not 404) and leaves no run row. The counter-case grants that same role one more key and expects 202 — without it, a viewer that reported no missing keys for anybody would satisfy every assertion above. It is also the walk that found the `model_key_of` 500. -->
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.  <!-- eleven of thirteen rows are now ticked and each names its walk; this one is the only acceptance row left, and it is the only one that was never closeable by a walk. The closing browser pass is RUNNING for this tick behind another writer's live QA slot (holder `/tmp/omnion-qa-slot-holders/3041848-1790924053`, pid 3537824 alive, cwd `/mnt/apopic/omnion-w4` — not mine, waited on, not touched), so its result belongs in the next tick's BUILD-LOG entry rather than in this line. Two rows in the QA plan remain structurally unreachable by a scoped pass and are named there rather than ticked here. -->

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
   *Progress:* the tables, the ten properties, the scorer, the store and the walks are done
   (654 unit + 12 walks). Remaining in this slice: the routes, the suite and case screens, CSV
   import, and the run entry point — which is slice 2's runner, so the slice closes with it.
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
   *Progress:* the table, the roll-up and the window reader are done; the **writer** is
   `ab029fe9`, and finding it late is the slice's one lesson — `refresh_day` had a passing
   reconciliation walk and no production caller, so the whole of what this slice owes a screen
   was a table nothing would ever write to. `ai.telemetry.tool.degraded` ships with it. The screen is
   `4807b518`, with `aaa0b4a2` behind it: the spec's step histogram and cost-per-solved scatter were
   not in the route at all, so they were written as two readers over live `ai_runs` before there
   was anything to draw them on. Each row links into `/ai/tools/{key}`, which is the screen that
   carries a tool's own recent calls — this build has no `/ai/logs` screen, so the link goes to the
   one that exists rather than to the path the spec named. **Left: the closing browser pass** — the
   route and the depth pass are in the inventory and the code is committed, but the pass has not yet
   been *run* against a live stack, and a depth pass that has never executed is a hypothesis, not a
   gate.

### Risks / notes

- Eval results are only as good as the cases: a suite of ten easy cases passes everything. The
  panel should show coverage (cases per tag, last failing case) and the docs should push toward
  capturing real failures as cases.
- LLM judging is itself unreliable: pin the judge model and prompt, record the reasoning, and never
  let a single rubric failure be the only evidence for a regression claim.
- Gates must not block a hotfix path: the gate returns a verdict and an operator can override it with
  a recorded reason; an override is an event, not a silent bypass.
