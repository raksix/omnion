# REQ-099 — Agent Runtime & Tool Loop

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Agents that actually do things.

- Agent loop with streaming sinks, tool-call execution, step counter and stop conditions.
- Agent state and conversation persistence; resumable runs; workspace per agent.
- Skills registry (validated skill definitions) and skills runtime.
- Guardrails: untrusted-content handling, tool allow-lists, output verification helper.
- Telemetry per run (steps, tokens, cost, tools used) and an SDK for embedding the runtime.

## Implementation spec

### Scope (in / out)

**In** — the loop that turns a model into an operator (docs/06-AI-HUB.md §5):

- **The loop** — `run(agent, goal, sinks)` in `crates/ai-hub::agent`: assemble the prompt from the agent's system prompt, the goal, the conversation so far and the memory scope; call the router (REQ-098) for a model; stream the answer; execute any tool calls through the tool system (REQ-100); append results; repeat. Every iteration is a **step**, written durably before the next one starts.
- **Streaming sinks** — the loop never talks to a transport: it publishes typed events (`step_started`, `text`, `tool_call`, `tool_result`, `usage`, `awaiting_approval`, `done`, `error`) to a sink. The API provides an SSE sink, tests an in-memory sink, and a workflow step (REQ-003) a blocking sink that collects the result. One loop, three consumers, no branch on transport.
- **Stop conditions** — max steps (default 8, cap 50), wall-clock deadline (default 300 s), token budget (default 200 000), cancellation, a repeated-identical-tool-call guard (three identical calls end the run with `loop_detected`), and a final-answer condition. The first condition reached wins and the run's `stop_reason` names it.
- **Persistence and resume** — `ai_runs` and `ai_run_steps` are the record; a run parked on an approval or interrupted by a process restart resumes from the first step that is not `completed`, and a step whose tool already ran is never re-executed (the step row is the idempotency record).
- **Workspace per agent** — a scratch area per agent in object storage with an `ai_agent_files` index: upload, list, read, delete, size caps (100 MB per agent, 10 MB per file) and path rules (relative, no `..`, no absolute, no control characters). Inputs a run is told to read, outputs it wants to keep.
- **Skills registry and runtime** — a skill is a validated, versioned definition (key, name, description, when-to-use, instructions, optional attached tool keys), not code: `validate` checks the shape, length bounds, that every attached tool exists, and that the checksum matches; the runtime injects enabled skills into the agent's prompt in attached order. `ai_skills` + `ai_agent_skills`, plus a small built-in set seeded on boot.
- **Guardrails** — untrusted content (tool results, uploaded files, retrieved documents) is wrapped in a delimited block the system prompt names as data, never instructions; the tool allow-list is enforced per call (a denied tool is refused, not merely hidden); an output-verification helper checks a final answer against a required shape (non-empty, length cap, optional JSON schema) and allows one repair turn before failing.
- **Telemetry** — per run: steps, tool calls by key, tokens, cost, duration, stop reason, error code; shown on the run detail and rolled up per agent. `ai_usage` rows are written through the same resolve path, so a run's cost equals the sum of its usage rows.
- **SDK** — a documented Rust entry point (`omnion_ai_hub::agent::run` plus a builder) and the internal HTTP surface (`POST /ai/agents/{id}/runs`, SSE) so a workflow node, a CLI or a future integration embeds the runtime instead of reimplementing it; a doc example lives in the crate README.

**Out**

- Multi-agent handoff, agent marketplaces and scheduled agents (docs/06 §14/§15 — later requests).
- Computer use, browser control and remote-desktop actions (REQ-108).
- Approval semantics and previews — the loop *parks*; REQ-101 owns the decision UX.
- Any path where model output becomes executed code: skills are data and can never carry a script.
- Long-term semantic memory beyond the existing scoped key/value memory (`ai_memory`, REQ-001).

### Screens (UI)

- **`/ai/agents`** — table: Name, Model (`provider/model` link), Tools (count + "approvals: n"), Skills (count), Runs 30 d, Success rate, Last run, Status. Search by name; filters status, model, tool; bulk Enable/Disable; row actions Run, Duplicate, Disable, Delete (type the name to confirm). Empty state "No agent yet" with Create agent and a link to the example skills.
- **`/ai/agents/new`, `/ai/agents/[id]`** — tabs **Config · Skills · Runs · Workspace**. Form: Name (1–80), Key (1–64, `[a-z][a-z0-9_-]*`, immutable after create), Description (≤ 400), Model (required, filtered to `tools` capability when the tool list is not empty), System prompt (≤ 8000, live counter), Temperature (0.00–1.00 step 0.05, default 0.20), Max steps (1–50, default 8), Deadline seconds (30–3600, default 300), Token budget (1000–2 000 000, default 200 000), Memory scope (none/organization/site/user), Tools (grouped multi-select, each row naming the permission it needs and disabled when the editor lacks it), Approvals (multi-select; rows the runtime can never auto-run are pre-checked and not removable). Field-level validation; the API message lands under its field.
- **Skills tab** — attached skills in runtime order (drag to reorder), each with version, when-to-use and the tools it can reach; Add skill opens a searchable picker of enabled registry skills; Detach with confirm; a disabled or stale-checksum skill renders a warning and is not injected (stated on the row).
- **Runs tab** — the agent's recent runs linking into `/ai/runs/[id]`.
- **Workspace tab** — file table (Path, Size, Type, Added by, Created, Last used by run), upload (drag-and-drop plus picker), download, delete with confirm, a usage bar against the 100 MB cap, and an empty state naming what a workspace is for.
- **`/ai/skills`** — registry table: Key, Name, Version, Tools, Used by (agents), Enabled, Updated, Source (`built-in`/`custom`). Detail drawer: definition sections with copy, attached tools, validation result with Run validation, Enable/Disable, Delete (custom only — a built-in can be disabled, never deleted). Create/edit form: Key, Name, Description (≤ 200), When to use (≤ 500), Instructions (≤ 8000, counter), Tools multi-select, Enabled.
- **`/ai/runs`, `/ai/runs/[id]`** — list: Started, Agent or "Chat", Goal (≤ 60 chars, tooltip for the rest), Model, Steps, Tokens, Cost, Duration, Stop reason, Status (running / awaiting_approval / completed / failed / cancelled / loop_detected); filters agent, status, stop reason, date range, user. Detail: header (agent, model, stop reason, tokens, cost), step trace as an accordion — kind, tool, arguments (redacted), result summary, status, tokens, duration — a live tail while running (SSE re-attach), Cancel, Resume (interrupted and approval-parked runs only), Copy transcript.
- **Run start** — a Run agent sheet from a row or the detail screen: Goal (≤ 2000, counter), optional workspace file references, Run streams steps into the sheet before handing off to the run detail.
- **States** — every table has `LoadingTable` skeletons, an `EmptyState` with a real action and an error banner with the API message and Retry; a parked run renders the awaiting badge in the list and the approve/reject control inline on the step in the trace.
- **Keyboard** — `⌘K` palette (with "Run agent"), `⌘⇧A` AI Hub, `G` then `A` agents, `G` then `R` runs, `G` then `K` skills, `/` focus search, `N` new agent, `R` run the focused agent, `Space` expands a step, `Esc` closes drawers, `↑/↓` + `Enter` move/open.
- **Mobile (<1024px)** — tables become label/value cards, the agent form stacks to one column, tabs become a scrollable tab bar, the step trace is a vertical accordion with tool arguments behind a tap, the run sheet is a full-height sheet; reorder offers up/down buttons; no hover-only actions.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/ai/agents` | List / create agents | `ai.agents.read` / `ai.agents.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/agents/{id}` | Read / change / remove an agent | `ai.agents.read` / `ai.agents.manage` |
| POST | `/api/v1/ai/agents/{id}/runs` | Start a run (SSE stream of loop events) | `ai.agents.run` |
| GET | `/api/v1/ai/runs` | Run history (`agent`, `status`, `stop_reason`, `from`, `to`, `user`) | `ai.agents.read` |
| GET | `/api/v1/ai/runs/{id}` | One run with its steps (paginated) | `ai.agents.read` |
| GET | `/api/v1/ai/runs/{id}/events` | SSE re-attach to a live run (replay from `last_event_id`) | `ai.agents.read` |
| GET | `/api/v1/ai/runs/{id}/steps` | Steps only (trace polling fallback) | `ai.agents.read` |
| POST | `/api/v1/ai/runs/{id}/cancel` | Request cancellation; the loop stops at the next step boundary | `ai.agents.run` |
| POST | `/api/v1/ai/runs/{id}/resume` | Resume an interrupted or approval-parked run | `ai.agents.run` |
| GET/POST | `/api/v1/ai/agents/{id}/files` | List / upload workspace files (multipart) | `ai.agents.read` / `ai.agents.manage` |
| GET/DELETE | `/api/v1/ai/agents/{id}/files/{path}` | Download / delete one file | `ai.agents.read` / `ai.agents.manage` |
| GET/PUT/DELETE | `/api/v1/ai/agents/{id}/skills` | Attach, order, detach skills | `ai.agents.manage` |
| GET/POST | `/api/v1/ai/skills` | Skills registry list / create | `ai.skills.read` / `ai.skills.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/skills/{key}` | One skill; delete refuses for a built-in | `ai.skills.read` / `ai.skills.manage` |
| POST | `/api/v1/ai/skills/{key}/validate` | Validate a definition (shape, tools, checksum) and report the result | `ai.skills.manage` |

New catalogue keys: `ai.agents.read`, `ai.agents.manage`, `ai.agents.run`, `ai.skills.read`, `ai.skills.manage` (plus `ai.approvals.act` from REQ-101 for the inline decision). Every route sits behind `guards::require("…")`; an unknown agent or run id answers 404, and a cross-organization id answers 404 too.

### Data model

Migration `database/migrations/0018_agent_runtime.sql` (take the next free number at implementation time). `ai_agents`, `ai_runs`, `ai_run_steps` and `ai_memory` are shared with REQ-001's engine spec — **one definition lands once**; the columns below are the union, and whichever request ships first creates them.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_agents` | id uuid pk, organization_id uuid → organizations cascade, site_id uuid → sites cascade null, key text, name text, description text default '', system_prompt text, model_id uuid → ai_models set null, temperature numeric(3,2) default 0.20, max_steps int default 8, deadline_seconds int default 300, token_budget bigint default 200000, tools jsonb default '[]', approvals jsonb default '[]', memory_scope text default 'none', enabled bool default true, created_by uuid → users set null, created_at, updated_at | unique `(organization_id, key)`; `(organization_id, created_at desc)` |
| `ai_runs` | id uuid pk, organization_id, site_id null, agent_id uuid → ai_agents set null, user_id uuid → users set null, trigger text (`chat`,`agent`,`workflow`,`schedule`), goal text, status text (`queued`,`running`,`awaiting_approval`,`completed`,`failed`,`cancelled`), stop_reason text null (`final_answer`,`max_steps`,`deadline`,`token_budget`,`cancelled`,`loop_detected`,`error`), model_id uuid set null, current_step int default 0, resume_count int default 0, cancel_requested_at, deadline_at, token_budget bigint null, prompt_tokens int, completion_tokens int, cost_micros bigint, heartbeat_at, started_at, finished_at, error text | `(organization_id, started_at desc)`; `(agent_id, started_at desc)`; `(status, heartbeat_at)` where status in ('queued','running'); `(status)` where status = 'awaiting_approval' |
| `ai_run_steps` | id uuid pk, run_id uuid → ai_runs cascade, step_no int, kind text (`message`,`tool_call`,`tool_result`,`approval`,`note`,`error`), tool text null, arguments jsonb null, result jsonb null, status text (`running`,`completed`,`failed`,`skipped`), prompt_tokens int, completion_tokens int, duration_ms int, error text null, started_at, finished_at | unique `(run_id, step_no)`; `(run_id, status)` |
| `ai_skills` | id uuid pk, organization_id uuid cascade null (null = built-in), key text, name text, description text, when_to_use text, instructions text, tools jsonb default '[]', version int default 1, checksum text (sha256 hex), source text (`built_in`,`custom`), enabled bool default true, created_by uuid → users set null, created_at, updated_at | unique folded `(coalesce(organization_id…), key)`; `(enabled, key)` |
| `ai_agent_skills` | agent_id uuid → ai_agents cascade, skill_key text, position int ≥ 0, attached_by uuid → users set null, attached_at | pk `(agent_id, skill_key)`; `(agent_id, position)` |
| `ai_agent_files` | id uuid pk, agent_id uuid → ai_agents cascade, run_id uuid → ai_runs set null, path text, size_bytes bigint, content_type text, storage_key text, checksum text, created_by uuid → users set null, created_at, last_used_at | unique `(agent_id, path)`; `(agent_id, created_at desc)` |

- Tools and approvals stay jsonb on the agent (ordered key lists) and a denial always wins (REQ-100).
- `ai_agent_files.storage_key` is an opaque object-storage key; the path rule (`^[^/][^\\]*$`, no `..` segments, ≤ 512 chars) is enforced in code and re-checked before every read.
- The runner (`ai_agent_runner` in `apps/api`, spawned from `main.rs` like `workflow_runner`) claims queued runs with `select … for update skip locked`, runs the loop with a concurrency cap (config `OMNION_AI_RUNNER_CONCURRENCY`, default 4), heartbeats each step and requeues runs whose heartbeat is older than 120 s; `OMNION_AI_RUNNER=false` disables it and the API answers `503 runner_disabled` on start.
- A parked run holds no worker: it is re-claimed after the decision (REQ-101).

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.run.started` | emitted | run, agent, model, user, trigger |
| `ai.run.completed` | emitted | steps, tokens, cost, duration, stop reason — usable as an automation trigger |
| `ai.run.failed` | emitted | error code, failing step |
| `ai.run.cancelled` | emitted | requested_by, step reached |
| `ai.run.resumed` | emitted | run, resume count, resumed from step |
| `ai.run.awaiting_approval` | emitted | run, step, tool — drives REQ-101's inbox and REQ-021 notifications |
| `ai.run.loop_detected` | emitted | run, repeated tool, occurrences |
| `ai.agent.created` / `.updated` / `.removed` | emitted | key, changed fields |
| `ai.skill.registered` / `.updated` / `.disabled` | emitted | key, version, validation result |
| `ai.guardrail.blocked` | emitted | run, rule (`untrusted_instruction`,`output_schema`,`tool_denied`), detail |

All names are dotted lower-case and ride the signed webhook bus; run events carry the organization and site so an org-scoped endpoint receives only its own deliveries. Tool-call events proper belong to REQ-100 (`ai.tool.denied`, `ai.tool.failed`) and are emitted from the same execution point, not duplicated here.

### Acceptance criteria

- [ ] A run started from the panel streams steps into the detail view live, and the same run reloaded after completion shows an identical step list (replay matches SSE).
- [ ] `max_steps` stops a run that never reaches a final answer with `stop_reason = max_steps` and no further provider calls (test counts upstream calls).
- [ ] The deadline stops a run against a deliberately slow stub, the token budget stops an overshooting run, and cancellation stops at the next step boundary with a finished partial trace.
- [ ] Three identical tool calls in a row end the run with `loop_detected` and an `ai.run.loop_detected` event.
- [ ] A tool the agent does not allow-list is refused with a stable code, nothing is executed, and the target row is unchanged (asserted in the test).
- [ ] A tool on the approval list parks the run as `awaiting_approval`; approving from the trace resumes it to completion, rejecting ends it with `stop_reason = cancelled` and no effect.
- [ ] A run interrupted by killing the runner resumes from the first non-completed step, and a step whose tool already ran is not executed twice (test asserts one side effect for one step row).
- [ ] Resume on a run whose steps are all completed is refused with a clear message.
- [ ] Untrusted tool output containing an instruction-shaped string does not change behaviour (fixture test asserts the next step is the one the system prompt asked for) and `ai.guardrail.blocked` fires when the tripwire triggers.
- [ ] The output-verification helper forces exactly one repair turn on a malformed answer and fails the run with `output_schema` on the second failure.
- [ ] A skill attaching an unknown tool key fails validation with the key named, and a disabled skill is absent from the assembled prompt (test asserts the prompt).
- [ ] A skill with a mismatched checksum is refused at run start with the reason shown on the Skills tab.
- [ ] Workspace paths with `..`, an absolute path or a control character are refused; per-file and per-agent caps are enforced with stable codes.
- [ ] Agent A cannot read agent B's workspace files in another organization, and organization A cannot read organization B's runs (404 both).
- [ ] A run's cost equals the sum of its `ai_usage` rows for the same window, asserted against SQL in the test.
- [ ] Run telemetry (steps, tools used, tokens, cost) is visible per run and rolled up per agent for 30 days, and equals the underlying rows.
- [ ] The SDK example runs a two-tool agent against a stub provider inside the workspace test suite without the API layer.
- [ ] Every screen has empty, loading and error states with a real call to action; no dead control and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: create an agent with a model, one permitted tool and one approval-gated tool; run it from the Run sheet and watch the steps land live; cancel a second run mid-flight and read the partial trace with its stop reason; trigger the approval path, decide it from the trace and watch the run resume; restart the API while a run is in flight and confirm it resumes without repeating the completed step; upload a workspace file, reference it in a goal, then delete it; attach a skill, reorder, disable one and confirm it is not injected (via the assembled prompt view); create a custom skill with a bad tool key, read the validation error, fix it; copy a transcript and confirm it matches the screen. The mobile pass (390×844) runs the run sheet, the agent form and the step accordion; the fresh-database empty states run over every screen.

The visual check must see: the step trace readable with arguments collapsed, the awaiting-approval badge distinguishable from a failure badge, no clipped token/cost cells, the run sheet filling the mobile viewport, the workspace usage bar not overlapping its label, no raw i18n keys, and no text overlapping the accordion controls.

### Slices

1. **The loop and its sinks** — `crates/ai-hub::agent` with the step machine, stop conditions, the SSE sink, the in-memory test sink, `POST /ai/agents/{id}/runs`, the run list and trace screens.
   *Done when:* a two-step run streams end to end against a stub, every stop condition has a failing-path test, and the trace equals the persisted steps.
2. **Persistence, resume and workspace** — `ai_runs`/`ai_run_steps` writing, the runner with claiming, heartbeat and requeue, re-attach SSE, resume, cancel, and the workspace (object storage + `ai_agent_files` + the Workspace tab).
   *Done when:* a killed process resumes without repeating a tool call, and workspace path/cap rules are enforced with tests.
3. **Skills** — `ai_skills`/`ai_agent_skills`, built-in seeds, validation, checksums, the registry screens, and prompt injection of enabled skills in order.
   *Done when:* an invalid skill cannot be attached, a disabled one never reaches the prompt, and a reorder changes the assembled prompt in a snapshot test.
4. **Guardrails, telemetry and SDK** — untrusted-content delimiting, the output-verification helper, guardrail events, per-run telemetry and agent roll-ups, the Rust SDK entry point with its doc example.
   *Done when:* the injection fixture and the schema-repair fixture pass, telemetry matches `ai_usage`, and the SDK example runs in CI.

### Risks / notes

- **Runaway loops are the failure mode that costs money.** Every condition is enforced by the runtime, not by prompt text: a step cap the model cannot raise, a token budget checked before each call, and a deadline that survives a hung provider.
- **Resume and side effects are the correctness risk.** A step row is written `running` before the tool runs and `completed` after; a step left `running` after a crash is inspected, and a non-idempotent tool is never blindly retried — it is reported for a human.
- **Prompt injection is assumed, not prevented.** Untrusted content is delimited and labelled, tool output is never treated as instructions, and the allow-list is the real boundary: an injected instruction can only ask for tools the agent already holds, and those tools still check permissions.
- Workspace files are user content in shared storage: path rules are enforced twice (write and read), keys are opaque, and a file is never served by a direct storage link — always through the guarded route.
- Transcripts carry user data: redact arguments for tools that name a secret-shaped argument, cap the stored result size, and never put a raw system prompt in a webhook payload.
- The runner is another background task in the API process and must be disable-able (`OMNION_AI_RUNNER=false`) so a small installation can run without a loop worker, and the panel must say so rather than failing silently.
- Concurrency: one agent should not run twice by accident — a per-agent advisory lock via `ai_runs` status prevents duplicate runs from a panel double-click *and* from the workflow trigger.
