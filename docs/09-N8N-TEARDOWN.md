# Omnion — n8n Engine Teardown (v2.41.0)

> Owner directive (2026-09-25): **"examine n8n's code in extreme detail."** n8n is the reference
> point for Omnion's Automation Engine (REQ-003), visual Workflow Builder (REQ-004), approval
> flows, and the AI agent runtime (docs/06-AI-HUB.md).
>
> Method: real sparse checkout of `github.com/n8n-io/n8n` (default branch @ **v2.41.0**,
> 2026-09-25) into `/mnt/apopic/n8n-research/repo`, followed by direct source reading. Every
> claim below cites actual files, classes and constants from that checkout (index in §14).

## 0. Repo & scale facts

- Version **2.41.0** — monorepo: `pnpm@12.4.2` + turbo; `engines: node >= 24`.
- **21,512 `.ts` files.**
- Top-level packages: `cli`, `core`, `workflow`, `nodes-base`, `frontend/editor-ui`,
  `extensions`, `modules`, `node-dev`, `testing` — plus **57 packages under `packages/@n8n/*`**
  (`db`, `config`, `di`, `decorators`, `api-types`, `engine`, `agents`, `expression-runtime`,
  `mcp-*`, `chat-hub`, `ai-*`, `computer-use`, `blob-storage`, …).
- Integration library: **308 node directories**, **412 credential definitions** in `nodes-base`.
- **License: Sustainable Use License** (fair-code — *not* OSI open source). `.ee.` files are
  gated behind an enterprise license; content of non-master branches is "not licensed".
  `nodes-base/package.json` declares `LicenseRef-n8n-sustainable-use`. See §13.15.

## 1. Layering

| Layer | Package(s) | Role |
|---|---|---|
| Data model | `packages/workflow` | Workflow/Node types, connections, expressions + sandboxing, run data |
| Engine v1 | `packages/core/src/execution-engine` | `WorkflowExecute` — the classic stack-based executor (still primary) |
| Engine v2 | `packages/@n8n/engine` (+ `@n8n/node-engine-compatibility`) | New **durable, step-based** engine; runs v1 node code via a compatibility executor |
| App/server | `packages/cli` | Express app, DI (`@n8n/di`), config (`@n8n/config`), modules, scaling, webhooks, REST API |
| DB | `packages/@n8n/db` | TypeORM entities + migrations |
| Editor | `packages/frontend/editor-ui` | Vue 3 + Vite editor UI (separate package) |
| AI | `packages/@n8n/agents`, `ai-node-sdk`, `ai-utilities`, `mcp*`, `computer-use`, `chat-hub` | Agent runtime + AI tooling |

The **57 `@n8n/*` packages are the key scaling trick**: with 21.5k TS files, package boundaries
(not folder conventions) are what keep the repo workable. Omnion's `crates/` + `packages/` split
(docs/04-MONOREPO.md) is the same discipline with a stricter language boundary.

## 2. Engine v1 — `WorkflowExecute` (`packages/core/src/execution-engine/workflow-execute.ts`, 3168 lines)

**Run entry.** `run({ workflow, startNode, destinationNode, pinData, triggerToStartFrom, additionalRunFilterNodes })`
returns a **`PCancelable<IRun>`** — the file carries an explicit comment: *"Do NOT add `async` to
this function, it will then convert the PCancelable to a regular Promise and [thus] not allow
canceling active executions anymore."* Cancellation is a first-class primitive, not an afterthought.

**Execution stack.** `runExecutionData.executionData.nodeExecutionStack: IExecuteData[]` seeded
with the start node + `{ main: [[{ json: {} }]] }`. The start node is chosen via
`workflow.getStartNode(destinationNode?.nodeName)`.

**Partial execution (run-to-node).** When a `destinationNode` is given, a `runNodeFilter`
is computed from `getParentNodes(...)` — **plus `ALL_NON_MAIN` parents** — because
*"Agents use the engine to run tools through non-main connections."* This is the mechanism
behind n8n's "execute step / execute to here" and behind AI-agent tool execution.

**Main loop.** `processRunExecutionData()` wraps everything in a `PCancelable` and runs an
`executionLoop: while (this.isExecutionStackNotEmpty())`:
- pops the next `executionData` (`popExecutionStack()`),
- computes `runIndex` and a per-node try key `` `${node}:${runIndex}` ``,
- **endless-loop guard**: if the same try key repeats consecutively →
  `UserError('Stopped execution because it seems to be in an endless loop')`,
- skips nodes outside the run filter, ensures input data exists (else skips the node),
- emits `nodeExecuteBefore` / `nodeExecuteAfter` lifecycle hooks (`hooks.runHook(...)`),
- **agent pause/resume marker**: nodes about to resume after an agent tool round carry
  `executionData.metadata.nodeWasResumed`, and `nodeExecuteBefore` is suppressed for them
  (*"Without this check, the agent would emit nodeExecuteBefore twice (initial + resume) but only
  one nodeExecuteAfter, causing frontend spinner state to become stuck"* — ticket AI-1414).

**Node-level retry.** `getRetryParams()` returns `[maxTries, waitBetweenTries]` — only when
`node.retryOnFail === true` and the failure isn't a resumed error ("there is nothing to re-run,
so don't apply retryOnFail"); `maxTries` is clamped to
`Math.min(5, Math.max(2, node.maxTries || 3))`. The loop runs `for (tryIndex = 0; tryIndex < maxTries; tryIndex++)`.
`node.continueOnFail === true` converts failures into per-item `error` outputs instead of
aborting the run. Fatal errors route through an **error workflow**
(`cli/src/execution-lifecycle/execute-error-workflow.ts`).

**Node invocation.** `executeNode()` builds an `ExecuteContext` and calls:
- `nodeType.execute(context, subNodeExecutionResults)` (class) — or `.call(context, …)` (object form),
- or a `customOperation` path.
Close handlers are collected into `closeFunctions` and flushed after the node finishes.
The second argument — **`EngineResponse`** — is how a node hands sub-node results back
(agent tool calls); `requests-response.ts` provides `handleRequest` / `isEngineRequest` /
`makeEngineResponse` for nodes that *request* sub-executions (the "engine request" protocol).

**Context object zoo.** `execution-engine/node-execution-context/` implements typed contexts per
node role: `base-execute`, `execute`, `execute-single`, `poll`, `trigger`, `webhook`, `hook`,
`load-options`, `supply-data`, `credentials-test` — plus ~25 helper modules (`binary`,
`deduplication`, `file-system`, `request` (+ `pagination`, `oauth`, `authentication`),
`scheduling`, `ssh-tunnel`, `data-table`, `get-secrets-proxy` (external secrets!), …).

**Partial-execution utils** (`partial-execution-utils/`): `directed-graph`, `find-start-nodes`,
`filter-disabled-nodes`, `clean-run-data` — graph analysis helpers reused by the editor for
"execute step" and by the engine.

## 3. Node model & the item data model

- A node is `INodeType` (+ `INodeTypeDescription`); execution units are **arrays of items**:
  `INodeExecutionData { json, binary?, pairedItem?, error? }` (`packages/workflow/src/interfaces.ts`,
  4559 lines of types).
- Nodes consume `INodeExecutionData[]` per input and return `INodeExecutionData[][]` per output
  — the "item array" model (vs. a single envelope). This gives natural fan-out/fan-in and
  per-item error isolation (`continueOnFail`).
- **`pairedItem` lineage**: `addPairedItemLineage()` in the executor plus
  `workflow-data-proxy-paired-item-memo.ts` track which input item produced each output item —
  enabling error attribution back through the chain ("this item failed because item 3 of node X").
- Flow control is implemented by a `RoutingNode` (`execution-engine/routing-node.ts`) that reads
  the node's routing rules and distributes items across outputs (If/Switch/Filter family).
- Sub-workflows: the ExecuteWorkflow node family; workflow call sites are load-options contexts.
- Triggers: `TriggersAndPollers` (`execution-engine/triggers-and-pollers.ts`) runs `trigger()`
  or `poll()` implementations; polling triggers use the **deduplication service**
  (`data-deduplication-service.ts` + `processed-data` entity + `poller-state` entity).
- Binary data is out-of-band: `core/src/binary-data/` + `@n8n/blob-storage` with multiple
  stores (filesystem byte store, S3-style blob storage, DB, memory).

## 4. Expressions & the sandbox stack

Two independent layers, both worth copying conceptually:

**a) Data proxy** — `workflow/src/workflow-data-proxy.ts`: builds the `$json`, `$node`,
`$items()`, `$runIndex`, `$binary`, `$now`… context; picks only the data a node may legitimately
see (scoped access), with a paired-item memoization layer for performance.

**b) AST sandboxing — `@n8n/tournament`** (`workflow/src/expression-sandboxing.ts`): user
expressions are *rewritten* before evaluation. Hooks (`ASTBeforeHook` / `ASTAfterHook` /
`astVisit`) enforce, with dedicated error types:
`ExpressionReservedVariableError` (e.g. the internal `___n8n_data` and `__sanitize` identifiers),
`ExpressionWithStatementError`, `ExpressionDestructuringError`,
`ExpressionComputedDestructuringError`, `ExpressionClassExtensionError` — i.e. `with` is banned,
computed/destructuring access is constrained, and identifiers are rewritten to run through a
`__sanitize` wrapper. **Blocklists are structural (AST), not string-based.**

**c) Isolated evaluation — `@n8n/expression-runtime`** (`bridge/`, `evaluator/`, `pool/`,
`runtime/`): expressions actually execute in a separate runtime with its own error taxonomy —
`TimeoutError`, `MemoryLimitError`, `SecurityViolationError` — and `workflow/src/expression.ts`
maps VM-side errors back into host-side `ExpressionError`s. Resource limits are explicit:
**timeout + memory + security violations are three distinct failure classes.**

## 5. Code execution (Code node) — `nodes-base/nodes/Code/`

- `JavaScriptSandbox` uses **`vm2` (`NodeVM`)** with a resolver built from
  `NODE_FUNCTION_ALLOW_BUILTIN` / `NODE_FUNCTION_ALLOW_EXTERNAL` (module access is opt-in).
- `JsCodeValidator` statically validates user code (e.g. disallowed methods in Run-Once-For-Each
  modes); `result-validation.ts` checks the returned shape.
- **The interesting move: task runners.** `JsTaskRunnerSandbox` + `PythonTaskRunnerSandbox` can
  push code execution **out of the main process** into dedicated task-runner processes (Python
  is a first-class code language now). n8n keeps vm2 for the in-process path but treats
  out-of-process isolation as the strategic direction — consistent with vm2's history of escapes.

## 6. Engine v2 — the new step-based engine (`@n8n/engine`)

The most important architectural development in 2.x. Structure (`@n8n/engine/src/`):
`execution/`, `queue/`, `database/`, `admittance/`, `auth/`, `runtime/`, `server/`,
`response-channel/`, `lifecycle-events/`, `logging/`, `serve.ts`, `index.ts`.

- **Durable step model.** `execution/` contains `step-store.ts`, `execution-store.ts`,
  `loop-ledger.ts`, `batch-step.ts`, `settlement.ts`, `completion.ts`,
  `start-execution.service.ts`, `iteration-mapping.ts`, `validate-step-context.ts` — executions
  are modeled as **steps persisted in a store**, with a loop ledger for iteration accounting.
- **Queue-driven workers.** `StepWorker` consumes `step:ready` messages from a `WorkQueue`;
  `OrchestrationWorker` consumes `execution:enqueued` and `step:settled`. The split is explicit:
  *"Kept separate from the orchestration worker so a flood of step work can't starve planning."*
- **WaitSweeper** — the only time-driven transition: fires steps whose deadline passed.
  `DEFAULT_WAIT_SWEEP_INTERVAL_MS = 60_000` (*"v1 resolves waits on a 60-second poll, so timer
  resolution matches it"*), `DEFAULT_WAIT_SWEEP_BATCH_SIZE = 500`, sweeps never overlap.
- **Standalone service.** `serve.ts`: the engine can run as its own process — requires
  `N8N_ENGINE_DATABASE_URL` + `N8N_ENGINE_AUTH_SECRET`, runs its own DB migrations, plugs in
  `AllowAllAdmittance` + `SharedSecretIdentityVerifier`, and can run with
  `noopExecutionResponseSender` (*"the caller reads the run over the API instead"*).
- **Control-plane / data-plane split** (`cli/src/modules/engine-v2/`): `EngineControlPlaneClient`,
  `engine-control-plane-server.ts`, `control-plane-auth.middleware.ts`,
  `engine-data-plane-client.ts`, `remote-credentials-helper.ts`,
  `engine-lifecycle-event-push-relay.ts`, `response-channel/`. The integrated mode
  (`engine-v2.runtime.ts`) is described as *"the integrated-mode composition root… the host
  never sees the workers or the queues."*
- **Compatibility layer** (`@n8n/node-engine-compatibility`): `V1StepExecutor`,
  `v1-workflow-converter`, `engine-step-data-loader` — **runs existing v1 node code on the new
  engine**. Migration strategy: new engine + old node semantics, piece by piece.
- **Auth tokens**: `mintIdentityToken` / `mintActionToken` / `verifyActionToken`
  (`IDENTITY_TOKEN`, `ACTION_TOKEN`) — every engine call is authenticated and scoped.
- **Lifecycle events** are batched with zod schemas (`lifecycleEventBatchSchema`,
  `MAX_LIFECYCLE_EVENTS_PER_BATCH`) — an event stream suitable for UIs and observability.

## 7. Queue mode & horizontal scaling (`cli/src/scaling/`)

- Queue technology: **Bull 4.16.4** (`import type Bull from 'bull'` in `scaling.types.ts`).
- Queue naming: `DEFAULT_QUEUE_NAME = 'jobs'`; **worker pools** get `jobs-<pool>`
  (`pool-config.service.ee.ts`, `worker-pools.service.ee.ts`) — dedicated queues per pool.
- `job-processor.ts` runs executions on workers: builds `WorkflowExecuteAdditionalData`, runs
  `WorkflowExecute`, persists via `ExecutionPersistence`, emits events; it also imports
  `StructuredToolkit` + LangChain `Tool` types from `n8n-core` — evidence of the AI/runtime
  convergence inside the worker path.
- `worker-server.ts`: workers expose an HTTP surface — health endpoint, **credential
  overwrites** endpoint, Prometheus `/metrics` — gated by config flags.
- HA: `leader-election-client.ts`, `multi-main-setup.ee.ts`, `redis-lock.service.ts` (distributed
  locking), `pubsub/` (event fan-out), `webhook-response-relay.ts` (webhook responses arrive on
  one instance but must be answered by another), `execution-stop.service.ts` (cross-instance
  cancellation), `worker-status.service.ee.ts`.
- Concurrency/cancellation are abstracted: `RunningJobSummary` in `@n8n/api-types` feeds the UI.

## 8. Triggers, webhooks & human-in-the-loop (`cli/src/webhooks/`)

- Handler stack: `webhook-server.ts`, `webhook-request-handler.ts`, `webhook-request-sanitizer.ts`,
  `webhook-helpers.ts`, `webhook.service.ts`, response extractors
  (`webhook-last-node-response-extractor.ts`, `webhook-on-received-response-extractor.ts`),
  `webhook-response.ts`, `webhook-response-headers.ts`, `webhook-blank-file-inputs.ts`.
- **Live vs test webhooks**: `live-webhooks.ts` (production URLs) vs `test-webhooks.ts` +
  `test-webhook-registrations.service.ts` + `test-webhooks.controller.ts` — the editor's
  *"listen for test event"* flow is a first-class subsystem. (Big UX lesson, see §13.12.)
- **Waiting/resume**: `waiting-webhooks.ts` and `waiting-forms.ts` resume paused executions via
  signed URLs (Wait node; Form trigger) — this is how a workflow survives a pause and continues
  later (or after a process restart). `pending-webhook-response.ts` holds the HTTP connection.
- **HITL (human-in-the-loop)**: `hitl-interaction-webhooks.ts` — base for Slack/Telegram
  "Send and Wait" nodes: *"The Send and Wait reference travels in the request body (not the URL)
  and is verified here via HMAC"* (`verifyHitlCallbackReference` from `n8n-core`). Approvals in
  n8n are signed resumable webhooks — direct prior art for Omnion's approval flow
  (docs/06-AI-HUB.md §9, docs/07-IAM.md §17).
- `engine-v2-webhooks.ts` + `services/engine-v2-webhook-responder.service.ts` — webhook
  ingress for the new engine.
- Trigger activation lives in `core/execution-engine/active-workflow-triggers.ts`; cron
  scheduling primitives in `workflow/src/cron.ts`.

## 9. Database & execution storage (`@n8n/db/src/entities/`)

- `execution-entity.ts`: indexes on `(workflowId, id)`, `(waitTill, id)`, `(finished, id)`,
  `(workflowId, status, id) WHERE deletedAt IS NULL`; **unique `deduplicationKey`** (WHERE NOT
  NULL); columns include `status`, `mode`, `startedAt`, `stoppedAt`, `waitTill`, `deletedAt`,
  `retryOf`, `retrySuccessId`; storage-location column with transformer,
  `'db' | 'fs' | 's3' | 'az'` (`ExecutionStorageLocation`) — **execution payload storage is
  pluggable**.
- Multi-tenancy: `project.ts` + `project-relation.ts` + `shared-workflow.ts` +
  `shared-credentials.ts` (`ProjectSharingData` type in `workflow/interfaces.ts`).
- Permissions: `role.ts`, `scope.ts`, `role-mapping-rule.ts` (SSO → role mapping) — RBAC is
  DB-modeled, not hardcoded.
- Secrets: `secrets-provider-connection.ts` + `instance-credential-assignment.ts` +
  `project-secrets-provider-access.ts` (external secret managers).
- Others worth noting: `folders.ts`/`folder-tag-mapping.ts` (folders + tags), `variables.ts`,
  `scheduled-task.ts`, `api-key.ts`, `deployment-key.ts`, `auth-identity.ts`,
  `auth-provider-sync-history.ts`, and .ee evaluation entities (`test-run`, `test-case-execution`,
  `evaluation-collection/config`, `agent-eval-*`).

## 10. AI stack in 2.41 (directly comparable to Omnion's AI Hub)

- `@n8n/agents`: **full agent runtime** — `runtime/loop/` (`agent-runtime.ts`, streaming sinks,
  `execution-counter.ts`, file-part hydration), `runtime/{mcp,memory,model,skills,state,streaming,telemetry,tools}`,
  `sdk/` (`agent.ts`, `guardrail(s)`, `mcp-client.ts`, `memory.ts`, `tool.ts`, `vector-store.ts`,
  `telemetry.ts`, `verify.ts`, **`untrusted-content.ts`**), `skills/` (registry, prompt, tools,
  validator), `vector-stores/` (pinecone, postgres, qdrant, supabase), `evals/`, `workspace/`.
- `cli/src/modules/instance-ai/`: a ~7.2k-line service — "AI that operates the instance" —
  with an adapter layer; plus `chat-hub` (chat product surface, also mirrored as
  `@n8n/chat-hub` package), `modules/mcp/` (MCP server with **workflow-builder tools** —
  create/update workflow via MCP), `@n8n/ai-workflow-builder.ee`, `@n8n/computer-use`,
  `@n8n/ai-node-sdk`, `@n8n/ai-utilities`.
- Convergence signal: the **engine executes agent tool calls** (`ALL_NON_MAIN` parents,
  `EngineResponse`), and the worker imports `StructuredToolkit` — n8n is fusing workflow
  execution and agent tool execution into one runtime. Omnion's `AI Agent → Tool → Permission →
  Approval → Audit` chain (docs/06) needs the same fusion, but with the permission layer built
  in from day one (n8n's tool permissions evolved late).

## 11. Frontend / editor (`packages/frontend/editor-ui`)

- **Vue 3 + Vite** (`vue-tsc` typecheck, `VUE_APP_PUBLIC_PATH`), not React — a deliberate
  contrast to Omnion's Next.js/React choice; the engine-side lessons transfer regardless.
- **CodeMirror 6** (`@codemirror/lang-javascript`, `-python`, `-json`, `-sql`, `-html` via
  `@n8n/codemirror-lang-*`) for code/expression editing.
- **`@dagrejs/dagre`** for graph layout of the node canvas (layered DAG layout).
- Frontend modularity mirrors backend: `@n8n/design-system`, `@n8n/frontend-module-sdk`,
  `@n8n/frontend-module-insights`, `@n8n/frontend-module-otel` — even the UI has a module SDK.

## 12. Security model summary

1. **Expressions**: AST-rewritten (tournament) + isolated runtime with timeout/memory limits.
2. **Code**: vm2 (in-process, gated modules) + out-of-process task runners (strategic direction).
3. **Credentials**: encrypted at rest, never exposed to nodes directly — helper-mediated;
   external secret providers supported; credential overwrites pushable to workers.
4. **Engine v2 auth**: identity/action tokens minted and verified per call; shared-secret
   verifier for standalone deployments.
5. **Webhooks**: request sanitizer; HMAC-signed HITL callbacks; signed wait-resume URLs.
6. **Least data exposure**: data proxy only exposes the data a node may see (scoped access).
7. **Multi-instance**: distributed locks, leader election, cross-instance stop.

## 13. Lessons for Omnion (adopt / avoid)

**Adopt:**

1. **Durable steps from day one.** n8n spent years on an in-memory executor and is now
   retrofitting durability (Engine v2: step store, loop ledger, wait sweeper, settlement).
   Omnion's Automation Engine should start durable — every step persisted, resumable, idempotent.
   This validates REQ-003/017 designs and should be written into the engine blueprint.
2. **Explicit cancellation primitive.** `PCancelable` + `AbortController` + close functions;
   cancellation across instances via a stop service. Omnion needs the same from the start.
3. **Endless-loop guard.** Same node+runIndex twice in a row → abort with a clear user error.
   Cheap, catches the most common user-authored infinite loop.
4. **Retry semantics as node-level policy** (`retryOnFail`, capped maxTries ≥ 2, clamped ≤ 5;
   no retry for resumed failures) — simple, per-node, battle-tested. Copy the caps and the
   resumed-error exception.
5. **Partial execution with graph filtering**, including non-main parents for agent tools —
   the blueprint for Omnion's "run from here" and AI tool execution.
6. **Item/array + provenance lineage (`pairedItem`)** — provenance is what makes per-item error
   attribution and agent debugging possible; Omnion should carry equivalent lineage on
   content revisions, workflow steps and AI tool results.
7. **Lifecycle hooks + batched lifecycle events** (`nodeExecuteBefore/After`, zod-validated
   batches) — the UI/telemetry contract Omnion's builder will need; note their bug ticket
   (AI-1414) about duplicate events on agent resume — design resume semantics explicitly.
8. **HITL as signed webhooks.** Approvals = HMAC-signed, resumable callbacks (body-carried
   reference, not URL). Adopt for Omnion approvals (docs/07 §17) and AI action approvals
   (docs/06 §9).
9. **Wait sweeper pattern** (60s sweep, batch 500, non-overlapping) — simple, sufficient for
   scheduled publishing (docs/05 §7) and delayed automations.
10. **Test webhooks + registrations service** — "listen for test event" is the killer onboarding
    UX for a visual builder; budget it as a first-class feature (REQ-004).
11. **Execution storage abstraction** (`db | fs | s3 | az`) — matches Omnion's S3/MinIO plan;
    make storage pluggable in the execution store from the start.
12. **Dedup service + poller state** for polling integrations — needed before Omnion ships any
    polling trigger (REQ-015 integrations).
13. **Module boundaries at the package level** (57 `@n8n/*` packages) — the only way the
    21.5k-file monorepo stays navigable; Omnion's crates/packages split should be enforced by
    CI (dependency graph rules), not convention.

**Avoid / do differently:**

14. **vm2 in-process.** n8n keeps vm2 for Code nodes and is pushing toward task runners;
    vm2's CVE history is a known risk. Omnion's plan (WASM plugin runtime + out-of-process
    runners, docs/02 §Plugin system) should skip the in-process dynamic-code phase entirely:
    no arbitrary JS/Python in the core process, ever. If code execution is needed, run it as a
    sandboxed worker from day one — and prefer the task-runner model (separate process/container,
    no credentials in the sandbox) over library-based sandboxes.
15. **License trap.** n8n is *fair-code*, not OSI open source (Sustainable Use License, `.ee`
    gated). Consequences: community forks can't use `.ee`; ecosystem trust issues; marketplaces
    built on such licenses constrain contributors. Omnion's positioning is "open-source
    enterprise application platform" (docs/00-CONTEXT.md) — pick a real OSI license for the
    core (Apache-2.0/MIT) and keep commercial features in a clearly separated directory/edition
    from day one if a business model is wanted. Decide this **before** the first public release.
16. **TypeScript runtime + Node ≥ 24 for the engine.** n8n's engine is JS/TS. Omnion's chosen
    Rust/Axum core (docs/02) gives better resource control for the durable engine; but note
    n8n's practical lesson: **most nodes are pure TS glue around HTTP** — Omnion's module SDK
    must make the 80% case (REST integration + mapping) trivial, or module authors will suffer.
17. **Item arrays are memory-hungry at scale.** n8n's model clones item arrays between nodes
    (mitigated by binary out-of-band storage and paired-item memos). Omnion should adopt
    **references/streams** (chunked batches + spill-to-S3) for large payloads instead of
    eager full copies; design the step data model with backpressure from the start.
18. **Worker pools / queue naming complexity.** `jobs-<pool>`, .ee-gated pool config, Bull 4
    (legacy). Omnion (Rust) can implement equivalent semantics with Postgres `SKIP LOCKED` +
    LISTEN/NOTIFY or a modern queue, without inventing pool bookkeeping in two places.
19. **60-second wait resolution.** Matching v1's poll interval is pragmatic but coarse; Omnion's
    scheduler should target sub-second wakeups via DB timers/queue delays, with the sweeper as a
    safety net rather than the primary mechanism.
20. **Late-arriving permissions.** n8n's tool/AI permissioning and project scoping matured late
    (projects, role-mapping, tool permissions all post-hoc). Omnion has IAM (docs/07) and the
    AI Agent→Tool→Permission→Approval→Audit chain specified **before** implementation — keep it
    that way: every new tool/step registers its permission scope at definition time.

## 14. Key file index (for future reference)

Engine v1: `core/src/execution-engine/workflow-execute.ts`,
`core/src/execution-engine/routing-node.ts`, `core/src/execution-engine/triggers-and-pollers.ts`,
`core/src/execution-engine/active-workflow-triggers.ts`,
`core/src/execution-engine/partial-execution-utils/*`,
`core/src/execution-engine/node-execution-context/*`,
`core/src/execution-engine/execution-lifecycle-hooks.ts`,
`cli/src/execution-lifecycle/execute-error-workflow.ts`,
`cli/src/execution-lifecycle/save-execution-progress.ts`.

Model: `workflow/src/interfaces.ts`, `workflow/src/workflow-data-proxy.ts`,
`workflow/src/expression.ts`, `workflow/src/expression-sandboxing.ts`,
`workflow/src/run-execution-data-factory.ts`, `workflow/src/cron.ts`.

Engine v2: `@n8n/engine/src/{serve.ts,index.ts,runtime/,execution/,queue/,auth/,admittance/,lifecycle-events/,response-channel/}`,
`@n8n/node-engine-compatibility/src/{v1-step-executor,…}`,
`cli/src/modules/engine-v2/*`.

Scaling: `cli/src/scaling/{job-processor.ts,worker-server.ts,leader-election-client.ts,queue-name.ts,webhook-response-relay.ts,execution-stop.service.ts}`.

Webhooks: `cli/src/webhooks/*` (esp. `waiting-webhooks.ts`, `hitl-interaction-webhooks.ts`,
`test-webhook-registrations.service.ts`, `webhook-request-handler.ts`).

DB: `@n8n/db/src/entities/{execution-entity,project,role,scope,role-mapping-rule,secrets-provider-connection}.ts`.

AI: `@n8n/agents/src/{runtime,sdk,skills,vector-stores}/*`,
`cli/src/modules/{instance-ai,mcp,chat-hub}/*`, `@n8n/ai-workflow-builder.ee`.

Code node: `nodes-base/nodes/Code/{JavaScriptSandbox.ts,JsTaskRunnerSandbox.ts,PythonTaskRunnerSandbox.ts,JsCodeValidator.ts}`.

Frontend: `frontend/editor-ui/package.json` (Vue 3 + Vite + CodeMirror 6 + dagre).

## 15. Verification

- Checkout: `git clone --depth 1 --filter=blob:none --sparse https://github.com/n8n-io/n8n.git`
  then `git sparse-checkout set` on the paths in §14 (workdir `/mnt/apopic/n8n-research/repo`).
- Version pinned: `package.json` → `2.41.0`.
- All file paths, constants and quotes above were read from that checkout on 2026-09-25.
- The checkout is research scratch: **not** part of this repository.
