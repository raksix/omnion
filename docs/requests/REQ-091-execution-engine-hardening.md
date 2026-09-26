# REQ-091 — Execution Engine Hardening

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Making long automation runs trustworthy.

- Durable step store: every step's result is persisted before the next begins.
- Queue-driven step workers with an orchestration worker and batch steps.
- Loop ledger preventing double execution; endless-loop guard; run filters to skip nodes.
- Per-node retry policy with resumed-error exclusion; continueOnFail with per-item error capture.
- Cancellation that reaches running steps; error-workflow routing; wait sweeper (60s cadence, batched).

## Implementation spec

### Scope (in / out)

**In** — the reliability pass over the engine that already keeps durable step rows (migrations `0006`, `0010`: claiming with `for update skip locked`, waits, retries ≤ 5, cancellation):

- **Durable step contract** — a step's status transition and its output are committed in one transaction *before* the next step is enqueued; nothing depends on the process staying alive. A hard kill between steps leaves a run that is recoverable, not half-written.
- **Worker split** — one orchestration worker per organization claims executions, compiles/loads the plan and materialises ready steps; N step workers claim rows from `workflow_steps` by `queue_key` with `for update skip locked`, heartbeat while running, and are safe to run as more than one process (REQ-096 is the deployment of this contract).
- **Batches** — a step may fan out over an upstream array: the item count is stored on the step row, each item's result is written individually, and the step's status is derived (all ok = ok; any failure with stop policy = failed; with continue = `succeeded_with_errors` for the run).
- **Loop ledger** — before a node executes, the worker inserts a ledger row keyed by `(execution, node, iteration, input fingerprint)`. A conflict means the node already ran for that input: the stored result is returned instead of executing the node again. This is what makes re-claim and resume safe.
- **Endless-loop guard** — a per-execution iteration ceiling (default 500 per node, configurable ≤ 5000) plus repeat detection (same node, same fingerprint back to back) ends the run as failed with a guard event naming the node and the reason.
- **Run filters** — an execution can start at a node, stop at a destination node, or skip a node set. Filtering is resolved against the compiled plan and skipped nodes are recorded as `skipped` rows with a reason so the trace stays complete.
- **Per-node retry policy** — `max_attempts` (1–10), backoff (`fixed` | `exponential` with base and cap), and `retry_on` classes. Validation and refusal errors are never retried.
- **Resumed-error exclusion** — on resume or partial re-run, nodes recorded ok in the ledger are not executed again. A node that declares its side effect non-idempotent refuses a resume past it unless the operator explicitly confirms re-running side effects.
- **continueOnFail with per-item errors** — a failing item stores `{index, code, message, at}`, the step continues with the surviving items plus an `$errors` view, and the failure is visible per item rather than as one opaque step error.
- **Cancellation that reaches running steps** — cancel writes a request and a watermark; a running step checks it at every batch boundary and inside long host calls within a two-second window; the orchestrator force-fails a step that ignores the request past the grace window (default 30 s). Child executions started by a call-workflow step cancel with the parent.
- **Error-workflow routing** — a workflow may name an error workflow; a terminal failure starts it once with a sanitized context (ids, node, code) and no step inputs unless the operator opts in.
- **Wait sweeper** — one tick every 60 s, batched (default 200 rows per batch), `for update skip locked`, single-runner safe, promoting due waits and emitting a summary event.
- **Observability** — counters and gauges on the existing metrics surface: steps by status, retries, queue depth per key, sweep promotions, recovery count.

**Out**

- Queue backends, worker pools, lease/leader election and cross-instance stop — REQ-096 (this request defines the worker contract those pools run).
- Human-in-the-loop waits, forms and approvals — REQ-090 (its rows ride on this sweeper).
- Execution history screens, retention settings and partial-execution controls — REQ-093 (the filters exist here; the UI ships there).
- The node canvas — REQ-004.

### Screens (UI)

- **`/automations/[id]/runs/[run_id]`** — the trace gains: a batch group per step (item count, per-item status, expandable item rows with input, output and error), an attempts strip (`2/3`, next attempt time, chosen backoff), a Cancel control while the run is running, a recovery banner when a crashed run was re-queued, guard failures rendered as first-class messages, and a link to the error workflow's run when routing fired. Skipped nodes render greyed with their reason (`run filter`, `branch not taken`).
- **`/automations/[id]/runs/[run_id]` → Ledger panel** — collapsible list of `(node, iteration, fingerprint short, status)` so a resumed run can be explained without reading raw JSON.
- **`/settings/workflows/engine`** — organization-level panel: sweep status (last tick, rows promoted, backlog), queue depth per key, in-flight step workers, retry defaults, batch and iteration limits, default error workflow, and a **Reclaim stale runs** action that previews the runs it will re-queue before doing it.
- **States** — empty, loading and error states on both surfaces; the Cancel action and the batch accordion are reachable without a mouse and usable on mobile; long JSON collapses instead of overflowing.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflow-executions/{id}/steps` | Step rows: status, attempts, timings, batch item summary | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/steps/{step_no}` | One step in full (input, output, items, error) | `workflows.read` |
| POST | `/api/v1/workflow-executions/{id}/cancel` | Cooperative cancel; answers accepted or already-settled | `workflows.run` |
| POST | `/api/v1/workflow-executions/{id}/resume` | Resume an interrupted run from its ledger | `workflows.run` |
| POST | `/api/v1/workflow-executions/{id}/steps/{step_no}/retry` | Retry under the node policy (`{include_side_effects: bool}`) | `workflows.run` |
| GET | `/api/v1/workflow-executions/{id}/ledger` | Loop ledger rows for one run | `workflows.read` |
| GET | `/api/v1/workflows/{id}/executions?status=&node=&from=&to=` | Existing list, extended filters used by debugging | `workflows.read` |
| POST | `/api/v1/workflows/{id}/executions` | Start a run; body adds `start_node_id`, `destination_node_id`, `skip_node_ids` | `workflows.run` |
| GET | `/api/v1/settings/workflows/engine` | Engine settings plus live counters | `workflows.read` |
| PATCH | `/api/v1/settings/workflows/engine` | Retry defaults, batch size, iteration ceiling, error workflow | `workflows.manage` |
| POST | `/api/v1/settings/workflows/engine/reclaim` | Preview then re-queue stale running executions | `workflows.manage` |

No new permission keys in this request; REQ-096 adds the operations key for worker and queue surfaces.

### Data model

Migration `database/migrations/0015_execution_hardening.sql` (next free number if taken).

Adds to `workflow_steps`: `queue_key text`, `attempt int default 0`, `max_attempts int default 3`, `next_attempt_at timestamptz`, `heartbeat_at timestamptz`, `item_count int default 0`, `error_class text`, `skip_reason text`, `input_fingerprint text`, `policy jsonb default '{}'`. Adds to `workflow_executions`: `destination_node_id text`, `skip_node_ids jsonb default '[]'`, `cancel_requested_at timestamptz`, `heartbeat_at timestamptz`, `ledger_size int default 0`, `item_errors int default 0`, `error_workflow_execution_id uuid`.

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_step_items` | id uuid pk, execution_id uuid → workflow_executions cascade, step_no int, item_index int, status text ('ok','failed','skipped'), input jsonb, output jsonb, error jsonb, started_at, finished_at | unique `(execution_id, step_no, item_index)`; index `(execution_id, step_no)`; pruned with the execution |
| `workflow_step_ledger` | id bigserial pk, execution_id uuid cascade, node_id text, iteration int, fingerprint text, status text, step_no int, created_at | unique `(execution_id, node_id, iteration, fingerprint)`; index `(execution_id, created_at)` |
| `workflow_engine_settings` | id smallint pk default 1, iteration_ceiling int default 500, batch_size int default 200, sweep_seconds int default 60, default_max_attempts int default 3, backoff_base_ms int default 1000, backoff_cap_ms int default 60000, error_workflow_id uuid → workflows set null, updated_at | check `(id = 1)`; range checks on every numeric column |

The migration also extends the `workflow_steps.kind` vocabulary with the step kinds this request's policies apply to and widens the execution status check with `succeeded_with_errors`.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.step.started` / `.completed` / `.failed` / `.skipped` | emitted | one per step; batch steps also emit item-level failures |
| `workflow.step.retrying` | emitted | extends the existing event with attempt, next attempt time and error class |
| `workflow.step.item.failed` | emitted | index and error code only, never the item body |
| `workflow.guard.tripped` | emitted | iteration ceiling or repeat detection, names node and reason |
| `workflow.execution.recovered` | emitted | crash recovery re-queued a stale run |
| `workflow.execution.cancelling` / `.cancelled` | emitted | request accepted, then terminal state |
| `workflow.wait.due` / `workflow.wait.swept` | emitted | promotion and per-batch sweep summary |
| `workflow.execution.routed_to_error_workflow` | emitted | links failing and error executions |
| `workflow.execution.started` / `.completed` / `.failed` | emitted | unchanged |

Event payloads carry ids, node names and error codes; step inputs and outputs stay behind the API permission check.

### Acceptance criteria

- [ ] Killing the process between two steps leaves a run that `POST /resume` finishes without repeating a step already recorded ok.
- [ ] Two worker processes over one queue key never execute the same step twice; a sink-based test asserts exactly one side effect.
- [ ] The loop ledger answers a re-claim with the stored result instead of re-running the node.
- [ ] The iteration ceiling ends a runaway loop with a guard event naming the node; changing the ceiling in settings changes the threshold without a redeploy.
- [ ] Repeat detection (same node, same fingerprint back to back) trips with a human-readable trace reason.
- [ ] A batch step over a 500-item array stores per-item results; one failing item with continue policy leaves the run `succeeded_with_errors` and the surviving items reach downstream nodes.
- [ ] Retry policy is visible and honoured: attempts, chosen backoff and next attempt time match the configuration; a validation error is never retried.
- [ ] Resume and retry never re-run a node recorded ok; a non-idempotent node is refused without the explicit confirmation flag.
- [ ] Cancel reaches a running step within two seconds and a step that ignores it is force-failed after the grace window.
- [ ] Cancelling a parent cancels child executions started by a call-workflow step.
- [ ] Skipped nodes render with a reason; running to a destination node stops there and reports what did not run.
- [ ] Error-workflow routing on terminal failure starts the error workflow exactly once and carries a sanitized context.
- [ ] The sweeper promotes due waits within one tick, respects batch size, and never double-promotes when two instances run.
- [ ] `/settings/workflows/engine` shows counters that move while a test run executes.
- [ ] Reclaim previews affected executions and re-queues exactly those on confirm.
- [ ] Metrics for steps, retries and queue depth are exported and move during a test run.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the crash, double-claim and loop tests each fail when their guard is removed.

### QA plan

The walkthrough must: build a workflow with one slow step, one batch step over a seeded array and one deliberately failing step; run it and open the trace to read the batch group, attempts strip and item errors; cancel a run mid-step and confirm the terminal state and event; resume a run interrupted by a stopped process and read the recovery banner plus the ledger panel; trip the loop guard with a self-referencing node and read the guard message; route a failure to an error workflow and follow the cross-link; change a retry default and a batch limit on the engine panel and re-run; run the reclaim preview against a hand-made stale run and apply it.

The visual check must see: batch rows aligned inside the step card, error text wrapping rather than clipping, the attempt strip readable at 1024px, the ledger panel collapsed by default, and no dead controls while a run is running.

### Slices

1. **Durable steps, ledger and recovery** — committed step transitions, heartbeat column, ledger insert-before-execute, boot recovery of stale runs, resume endpoint, ledger inspector.
   *Done when:* a killed process resumes to completion with no repeated side effect and the ledger explains which node was skipped.
2. **Worker contract, batches and retry policy** — `queue_key` claiming, batch item storage, per-node policy resolution, attempts and backoff, non-idempotent declaration.
   *Done when:* two workers over one key produce one effect per step and a 500-item batch stores 500 item rows.
3. **Guards, cancellation and error routing** — iteration ceiling, repeat detection, cancel watermark checks inside host calls, child cancellation, error-workflow routing.
   *Done when:* cancel lands inside two seconds on a running host call and a terminal failure starts its error workflow once.
4. **Sweeper, settings and observability** — batched single-tick sweeper, engine settings screen and endpoints, reclaim action, metrics.
   *Done when:* two sweepers promote each due wait once and the engine panel counters track a live run.

### Risks / notes

- At-least-once is the contract: any step may run again after a crash, so every host action must be idempotent or declare otherwise.
- Input fingerprints must be canonical (sorted keys, normalized numbers) or every resume looks like a new iteration and the ledger stops protecting anything.
- The ledger grows with iterations; prune it with the execution and keep the size counter honest.
- Cancellation checks must stay cheap: one indexed read per batch boundary, not per item.
- Run filters must not become an authority bypass — every node reached still evaluates the run-as account's permissions.
- Keep the sweep implementation in one shared function so all instances behave identically; two code paths drift within a release.
