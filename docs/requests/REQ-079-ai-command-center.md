# REQ-079 — AI Command Center (cross-module jobs)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + ai
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

One sentence → a multi-step, approved business action.

- Natural-language job input ("find overdue invoices, find the account owners, create tasks, draft the e-mails").
- Plan preview: each step shown with the tools it will use and the data it will touch.
- Approval gate before execution; per-step approve/reject; edit a step before running.
- Running-job summary UI (steps, status, tokens, cost) and job history.
- Reuses the AI Hub tools + audit; every step is permission-checked as the requesting user.

## Implementation spec

### Scope (in / out)

**In**
- A job runner on top of the AI Hub (REQ-001): one prompt produces a *plan* — an ordered list of steps, each naming a tool from the registered tool catalogue, its arguments, its effect class (`read` / `write` / `destructive` / `external`) and the permission it requires — and nothing executes until a human approves the plan.
- The composer: one input (1–2000 characters) with the site and organization context fixed from the shell, optional constraints as free text ("do not send anything"), example prompts, and recent jobs for a one-click re-run. The composer never runs anything itself; it creates a job in `draft`.
- The planner (in `crates/ai-hub`): resolves the caller's visible tool set, asks the configured model for a plan, validates every step against the tool catalogue (unknown tool, missing required argument, argument outside the schema, effect class not permitted in this installation → the step is dropped or the plan is rejected with a reason), and caps the plan at twelve steps.
- Plan preview: step cards with the tool's human label, what it will do, the data it will touch (resource type, a count and up to five example labels, e.g. "12 overdue invoices · 12 account owners"), the effect badge, the required permission with the caller's own verdict (`you have this` / `needs crm.contacts.update`), the estimated token cost, and the REQ-101 action preview for write steps (the field-level diff of what would change).
- Approval: per-step `Approve`, `Reject`, `Edit`, plus `Approve all remaining` for steps whose effect is `read` or `write`. A `destructive` or `external` step is always approved individually and shows a typed confirmation (type the step's verb, e.g. `delete`), and `external` steps (sending e-mail, calling a third-party API) state exactly what leaves the installation. An edited step loses its approval and must be approved again; an edit that changes the tool resets the preview.
- Execution: steps run in order through the same tool executor the copilots use (REQ-042), each one evaluated against the *requesting user's* effective permissions at run time (organization and site scope included) — never a service account, never the approver's permissions. A step whose permission is missing at run time fails with `permission_denied` and the job stops with a clear state rather than continuing on a half-applied plan.
- Run UI: per-step status (`planned`, `awaiting approval`, `approved`, `rejected`, `running`, `succeeded`, `failed`, `skipped`), live output where the tool streams it, elapsed time, tokens in/out and cost per step, the invocation link (REQ-047's ledger), a `Retry this step` after fixing its arguments, `Skip this step` (only for `read` steps and only before it ran), and `Abort job` with a confirm that states what has already happened ("3 of 6 steps ran; 2 tasks were created; no e-mail was sent").
- Job summary and partial results: what was created or changed, with links to the resulting resources (tasks, drafts, records), the audit entry per step, and an explicit list of steps that did not run and why. A job that stopped halfway never looks finished.
- History: list with filters (status, requester, module touched, date range), free-text over the prompt and the job title, `Re-run` (creates a new job with a fresh plan — never resumes an old one), and `Copy as prompt`.
- Guardrails: the organization's AI budget and limits (REQ-047 `ai_usage_limits`) checked before each step; a wall-clock cap and a step cap; a rate limit on tool calls; a kill switch (REQ-123) that stops new jobs and aborts running ones; and a per-tool `read_only` mode for an installation that wants previews without writes.
- Audit: every job records creation, plan generation, each approval decision, each step execution (tool, effect, argument digest, result digest, duration) and the final outcome, correlated by `request_id` so the activity timeline (REQ-080) shows one thread.
- Entry points: the AI Hub screen's `New job`, the ⌘K palette (`Ask AI to…`), a job list route under the AI section, and deep links to a job from an audit row and from the activity detail.

**Out**
- New tools: this REQ consumes the tool registry; authoring tools is REQ-001's work, and a tool without a declared effect class and permission key is rejected by the planner rather than guessed.
- Autonomous/background execution on a schedule — an automation node that starts an AI job belongs to REQ-003/REQ-046, and it still lands in this screen's job list with a `triggered by automation` badge.
- Approval *chains*, delegation and escalation — REQ-059; this screen offers a single-decision gate and can link a job to an approval request when the installation demands one.
- The general chat/copilot experience (REQ-042) and the AI admin surfaces (usage, limits, investigation, REQ-047) — reused, not re-implemented.
- Prompt engineering tools, evaluation harnesses and fine-tuning — out of product scope here.
- Executive summaries and reporting (REQ-028) — the job summary is operational, not analytical.

### Screens (UI)

| Route | Screen |
|---|---|
| `/ai/jobs` | Job list with filters, status chips, cost per job, re-run |
| `/ai/jobs/new` | Composer: prompt, constraints, examples, `Build plan` |
| `/ai/jobs/<id>/plan` | Plan review: step cards, permission verdicts, action previews, approve/reject/edit |
| `/ai/jobs/<id>` | Run view: step timeline, live output, usage, abort, retry |
| `/ai/jobs/<id>/result` | Result summary: what changed, what did not run, links, audit |
| `/ai/tools` | Tool reference visible to the caller (label, effect, required permission) |

- **Job list.** Columns: job (title or first line of the prompt), status chip, requester, steps (`3/6`), duration, tokens, cost, created. Filters: status, requester, module, date range, and a free-text search over the prompt and title. Row actions: `Open`, `Re-run`, `Abort` (while running), `Copy link`. A `Partial` badge marks jobs that stopped mid-plan, with a tooltip naming the first unfinished step. Default sort: newest first.
- **Composer.** The prompt field with a counter and a `⌘Enter` to build the plan; a constraints box (free text, 0–500) whose contents are passed to the planner as instructions; three example prompts that fill the field; a short note stating that nothing runs until approved and that every step is checked against the user's own permissions; and a `Recent` list of the last five jobs with `Re-run`.
- **Plan review.** Header: the prompt, the plan's summary line ("6 steps: 3 reads, 2 writes, 1 external"), estimated tokens and cost, and the actions `Approve all remaining`, `Edit prompt and rebuild`, `Discard job`. Step cards in order: number, tool label, plain-language description, effect badge (`Read` grey, `Write` blue, `Destructive` red, `External` amber), data-touched chips with counts, required-permission row with the caller's verdict, expandable action preview for writes, and the controls `Approve` / `Reject` / `Edit`. Rejected steps stay visible with a `Rejected` state and a required reason (1–200 characters) that is shown in the summary and the audit. Blocked steps (a permission the caller lacks) are shown as `Blocked — needs <key>` and cannot be approved; the plan offers `Remove step` so the rest can proceed.
- **Run view.** Vertical step timeline with per-step duration and live output for the current step, a sticky summary strip (steps done, elapsed, tokens, cost, `Abort job`), and a log drawer with the raw tool call/result pairs for debugging (collapsed by default, never the default view). On failure: the failing step expands with its error, the input it received and a `Retry` that re-validates and re-runs only that step; the timeline states clearly which later steps did not run.
- **Result.** Grouped by outcome: `Created` (tasks, drafts, records with links), `Updated` (with the field-level diff), `Read only` (what was looked at, counts), `Not run` (steps with their reason), plus the total tokens/cost and a link to the job's audit thread (REQ-080). Drafted e-mails open in their own composer for review rather than being sent by the job.
- **States.** Empty list: an explainer with two example prompts and a link to the tool reference. Draft job abandoned: kept with a `Draft` state and a `Resume planning` action. Budget exhausted: the run stops with `ai_budget_exhausted` and the state names the limit and its reset; nothing partial is presented as done. Model unavailable: plan generation fails with a plain message and the job stays in `draft`. Permission drift (the caller's permissions changed between approval and run): the affected step fails with `permission_denied` and the summary says the permission was lost, not that the tool broke. Loading: skeleton step cards. Any step's output longer than the inline limit is truncated with `Open full output`.
- **Keyboard and mobile.** `g j` jobs, `⌘Enter` build plan / start, `a` approve the focused step, `r` reject, `e` edit, `j`/`k` move between steps, `x` abort with confirm, `⌘/` shortcut sheet, `Esc` closes the drawer. Below 900 px the plan is a single-column card list with the sticky `Approve all remaining` and `Discard` bar, the run view keeps the timeline with a collapsing log drawer, and the result groups become accordions.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/ai/jobs` | Create a job from a prompt and generate its plan (nothing executes) | `ai.jobs.create` |
| GET | `/api/v1/ai/jobs` | Job list (`status`, `requester`, `module`, `q`, `from`, `to`, cursor) | `ai.jobs.read` |
| GET | `/api/v1/ai/jobs/{id}` | Job with steps, approvals, usage and results | `ai.jobs.read` |
| POST | `/api/v1/ai/jobs/{id}/replan` | Rebuild the plan from an edited prompt (only while `draft`/`awaiting_approval`) | `ai.jobs.create` |
| PATCH | `/api/v1/ai/jobs/{id}/steps/{step}` | Edit a step's arguments (re-validates, clears approval, refreshes the preview) | `ai.jobs.create` |
| POST | `/api/v1/ai/jobs/{id}/steps/{step}/approve` | Approve one step (typed confirmation body for `destructive`/`external`) | `ai.jobs.approve` |
| POST | `/api/v1/ai/jobs/{id}/steps/{step}/reject` | Reject one step with a reason | `ai.jobs.approve` |
| POST | `/api/v1/ai/jobs/{id}/steps/{step}/remove` | Remove a blocked or rejected step from the plan | `ai.jobs.create` |
| POST | `/api/v1/ai/jobs/{id}/start` | Begin execution of the approved plan | `ai.jobs.create` |
| POST | `/api/v1/ai/jobs/{id}/steps/{step}/retry` | Retry a failed step after an edit | `ai.jobs.create` |
| POST | `/api/v1/ai/jobs/{id}/steps/{step}/skip` | Skip a not-yet-run `read` step | `ai.jobs.create` |
| POST | `/api/v1/ai/jobs/{id}/abort` | Abort a running job (confirm states what already happened) | `ai.jobs.cancel` |
| POST | `/api/v1/ai/jobs/{id}/rerun` | Create a new job from the same prompt | `ai.jobs.create` |
| GET | `/api/v1/ai/jobs/{id}/stream` | SSE: step transitions, live output, usage deltas (REQ-041 transport) | `ai.jobs.read` |
| GET | `/api/v1/ai/jobs/{id}/usage` | Per-step tokens and cost, joined from the AI ledger (REQ-047) | `ai.jobs.read` |
| GET | `/api/v1/ai/tools` | Tool catalogue visible to the caller (label, effect, required permission) | `ai.jobs.read` |
| POST | `/api/v1/ai/jobs/{id}/preview` | Action preview for one write step (REQ-101 diff payload) | `ai.jobs.create` |

Execution is guarded twice: the route checks the job-level key, and every step re-evaluates its own tool permission against the requester inside the executor, so a job started by someone who later lost `crm.contacts.update` fails that step. Error codes: `job_prompt_length`, `job_plan_empty`, `job_step_cap`, `job_tool_unknown`, `job_tool_arg_invalid`, `job_effect_not_allowed`, `job_permission_denied`, `job_step_not_approved`, `job_already_running`, `job_budget_exhausted`, `job_kill_switch_active`, `job_step_requires_confirmation`.

### Data model

Migrations: `0121_ai_jobs.sql`, `0122_ai_job_steps.sql` (reserved band 0116–0127 for this group of requests; append-only ledger — take the next free number if taken).

```sql
-- 0121_ai_jobs.sql
ai_jobs (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid not null references organizations (id) on delete cascade,
  site_id uuid null references sites (id) on delete set null,
  prompt text not null, constraints text null, title text null,
  status text not null default 'draft',
  approval_policy text not null default 'writes_require_approval',
  steps_total integer not null default 0, steps_done integer not null default 0,
  tokens_in bigint not null default 0, tokens_out bigint not null default 0,
  cost numeric(12,6) not null default 0, model_key text null,
  requested_by uuid not null references users (id) on delete cascade,
  trigger text not null default 'panel',            -- 'panel' | 'palette' | 'automation' | 'api'
  error text null, created_at timestamptz not null default now(),
  started_at timestamptz null, finished_at timestamptz null,
  constraint ai_jobs_prompt_length check (length(btrim(prompt)) between 1 and 2000),
  constraint ai_jobs_constraints_length check (constraints is null or length(constraints) <= 500),
  constraint ai_jobs_status_check check (status in
    ('draft','awaiting_approval','running','succeeded','failed','cancelled','rejected')),
  constraint ai_jobs_policy_check check (approval_policy in
    ('reads_auto','writes_require_approval','every_step','destructive_only'))
);
create index ai_jobs_org_created_idx on ai_jobs (organization_id, created_at desc);
create index ai_jobs_active_idx on ai_jobs (status) where status in ('awaiting_approval','running');
create index ai_jobs_requester_idx on ai_jobs (requested_by, created_at desc);

-- 0122_ai_job_steps.sql
ai_job_steps (
  id uuid primary key default gen_random_uuid(),
  job_id uuid not null references ai_jobs (id) on delete cascade,
  step_no integer not null, title text not null, tool text not null,
  args jsonb not null default '{}', effect text not null, requires_permission text not null,
  status text not null default 'planned', preview jsonb null, result jsonb null,
  invocation_id uuid null, error text null, rejected_reason text null,
  approved_by uuid null references users (id) on delete set null, approved_at timestamptz null,
  started_at timestamptz null, finished_at timestamptz null,
  constraint ai_job_steps_number_positive check (step_no >= 1),
  constraint ai_job_steps_effect_check check (effect in ('read','write','destructive','external')),
  constraint ai_job_steps_status_check check (status in
    ('planned','awaiting_approval','approved','rejected','running','succeeded','failed','skipped','removed')),
  constraint ai_job_steps_job_number_key unique (job_id, step_no)
);
create index ai_job_steps_job_idx on ai_job_steps (job_id, step_no);
create index ai_job_steps_status_idx on ai_job_steps (status) where status in ('running','awaiting_approval');
```

Running output (step transitions, streamed log lines and usage deltas) is not stored as rows: the job's steps carry their final result and the invocation ledger (REQ-047) carries the cost; the SSE stream is a live view. Audit rows carry the argument digest (a hash) rather than the raw argument payload for anything the audit redaction rules consider sensitive, while the step row keeps the arguments the operator must be able to review before approval. Nothing in these tables holds credentials: tool arguments that reference a stored secret carry the secret's *name* and the value is resolved at execution time from the secrets manager (REQ-037).

New permission keys in `crates/permissions/src/catalogue.rs`, category `ai`: `ai.jobs.read`, `ai.jobs.create`, `ai.jobs.approve`, `ai.jobs.cancel`. `ai.jobs.approve` is deliberately separate from `ai.jobs.create` so an installation can let a team draft plans while only leads approve external and destructive steps.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `ai.job.created` | A job (and its plan) was created | `job_id`, `requested_by`, `steps_total`, `effects[]` |
| `ai.job.plan_ready` | Plan generation finished (or was rebuilt) | `job_id`, `steps_total`, `blocked_steps` |
| `ai.job.step_approved` · `.step_rejected` | An approval decision | `job_id`, `step_no`, `tool`, `effect`, `decided_by` |
| `ai.job.step_failed` | A step failed (permission, tool error, budget) | `job_id`, `step_no`, `tool`, `reason` |
| `ai.job.completed` · `.failed` · `.cancelled` | Terminal state | `job_id`, `steps_done`, `tokens`, `cost` |
| `ai.job.budget_stopped` | A step refused because the budget or step cap was hit | `job_id`, `limit`, `used` |
| `ai.job.kill_switch_stopped` | A running job was stopped by the kill switch (REQ-123) | `job_id`, `flag_key`, `stop_reason` |

Consumed: `ai.budget.exhausted` and `ai.limit.changed` (REQ-047 — stop accepting steps and mark the job), `feature.flag.changed` (kill switch and read-only mode), `approvals.request.decided` (REQ-059 — when an installation routes `destructive` steps through an approval chain, the decision releases or rejects the corresponding step), `users.deactivated` (abort running jobs whose requester was deactivated), `crm.record.deleted` and other module deletions (a retry of a step whose target vanished fails cleanly with a reason). Webhook relevance: `ai.job.completed` and `ai.job.step_failed` are worth delivering for an installation's own monitoring; `ai.job.created` is internal. Payloads carry ids and counts, never prompt text or tool arguments.

### Acceptance criteria

- [ ] A prompt creates a `draft` job and a plan; nothing executes before approval — verified by checking that the target records are unchanged after plan generation.
- [ ] Each plan step names its tool, its effect class, its required permission and the data it will touch, with counts and example labels for the resources involved.
- [ ] A write step shows the REQ-101 action preview (field-level diff) before approval; a `destructive` or `external` step additionally requires a typed confirmation and cannot be covered by `Approve all remaining`.
- [ ] A caller lacking a step's permission sees it as `Blocked — needs <key>`; approving it is impossible, and removing it lets the rest of the plan run.
- [ ] Editing a step's arguments clears its approval, re-validates against the tool schema, and refreshes its preview; an invalid argument is refused with a field-level message.
- [ ] Rejecting a step requires a reason, which appears in the run view, the result summary and the audit row.
- [ ] Starting a job executes steps in order under the *requester's* permissions: with the requester's key revoked mid-run, the affected step fails with `permission_denied` and the summary names the lost permission.
- [ ] A job never runs with elevated rights: a step the requester cannot perform is refused even when the approver holds the permission.
- [ ] The run view updates live (step transitions and output) over SSE and recovers the same view after a reload, showing completed steps with their results.
- [ ] `Retry` on a failed step re-runs only that step; `Skip` is offered only for not-yet-run `read` steps; `Abort` mid-run stops after the current step and the confirm names what already happened.
- [ ] The result summary lists created and updated resources with links, the reads with their counts, and every step that did not run with its reason — a partial job is never presented as complete.
- [ ] Tokens and cost are shown per step and per job, matching the AI ledger (REQ-047) for the same window.
- [ ] The budget and step caps stop a runaway plan (`ai_budget_exhausted`, `job_step_cap`), and the kill switch refuses new jobs and stops running ones.
- [ ] Every job writes an audit thread with one row per step (tool, effect, argument digest, result digest, duration) that the activity timeline (REQ-080) shows as a single correlated entry.
- [ ] History filters (status, requester, module, date, free text) work, and `Re-run` creates a new job with a fresh plan rather than resuming the old one.
- [ ] A user without `ai.jobs.approve` can draft and see plans but cannot approve; a user without `ai.jobs.read` cannot list jobs.
- [ ] The job list, plan cards and run view are usable at 390 px with the sticky approval bar, and the walkthrough reports zero high findings on the new routes.

### QA plan

Add `/ai/jobs`, `/ai/jobs/new` and a seeded job's `/ai/jobs/<id>/plan` to the `routes` array in `scripts/qa/walkthrough.cjs`, and reach the section from the sidebar's AI entry. The harness points the AI provider at the stub used for QA (no external model call) and seeds a tool set with one read, one write, one destructive and one external step so every path can be exercised without side effects outside the QA database. The walkthrough must: open `/ai/jobs/new`, enter the example prompt, build the plan and read the six step cards with their effects and permission verdicts; expand the action preview of the write step; edit one step's argument and see the approval reset; reject one step with a reason; attempt to approve the destructive step without the typed confirmation (expect a refusal) and then approve it correctly; approve the remaining steps; start the job and watch the timeline update live; retry a deliberately failing read step after fixing it; open the result summary and follow one created resource link; start a second job and abort it mid-run, confirming the summary says which steps ran; and finally filter the history by status and re-run an older job. Visual check: effect badges are distinguishable and labelled (not colour-only), the permission verdict per step is readable, blocked steps are obvious, the timeline shows real durations and the live output of the current step, the usage figures match the plan's estimate within a stated tolerance, and no screen shows raw JSON as its primary content.

### Slices

1. **Planner, schema and plan review.** Migrations `0121_ai_jobs.sql` and `0122_ai_job_steps.sql`, job and step storage, the planner with tool-schema validation and the step cap, the composer and the plan review screen with permission verdicts, effects and action previews, `ai.tools` reference, and the walkthrough routes. *Done when:* acceptance 1–2, 6, 16 pass and `/ai/jobs` is in the walkthrough inventory.
2. **Approvals and execution.** Per-step approval with typed confirmation for destructive and external steps, `Approve all remaining`, step edit with revalidation and approval reset, the executor running increments under the requester's live permissions, retry/skip/abort, and the SSE run view. *Done when:* acceptance 3–5, 7–10 pass.
3. **Summary, history and cost.** Result summary with resource links and not-run reasons, per-step usage joined from the AI ledger, history filters and re-run, the audit thread with one row per step, and the correlation visible in Activity (REQ-080). *Done when:* acceptance 11–12, 14–15 pass.
4. **Guardrails, polish and mobile.** Budget and step caps, kill switch and read-only mode, permission-limited rendering, empty/loading/error states, mobile layout, event payload verification, and a dry-run pass over all four effect classes in the QA harness. *Done when:* acceptance 13, 17–18 pass with a green walkthrough and no high findings.

### Risks / notes

- The privilege boundary is the whole point: steps execute as the requesting user, evaluated at run time, every time. Approving a step is an editorial decision, never a grant — the single most important test in this REQ is that an approver with more rights cannot make a job do what the requester could not.
- Destructive and external steps are the blast radius: typed confirmations, individual approval, a preview that names the exact objects, and a summary that states what left the installation. Sending e-mail or deleting records must never be reachable through `Approve all remaining`, however convenient that would be.
- Partial execution is normal, so honesty is the feature: the summary must distinguish "created", "updated", "read" and "did not run", and the abort dialog must state what already happened. A job that looks complete when it stopped at step 3 is a trust-destroying bug.
- Prompt injection through tool output is a live threat as soon as a step reads content written by someone else: tool results are data, never instructions, and a job never replans itself from tool output — a changed plan always comes back to a human for a new approval round.
- Cost control is a product requirement, not a nicety: the estimate is shown before approval, the caps are enforced per step, and a budget stop names the limit rather than failing obscurely.
- Tool catalogue drift: a tool that disappears between planning and execution must fail its step with a clear reason, and a tool whose schema changed must fail validation rather than being called with stale arguments.
- Audit completeness includes the negative space: rejections, removals, skips and aborts are recorded, because "who decided not to do this" is as important as what ran.
- The runner must never hold credentials: secrets are resolved at execution from REQ-037 by name, and the plan, the audit row and the SSE stream carry the name only.
