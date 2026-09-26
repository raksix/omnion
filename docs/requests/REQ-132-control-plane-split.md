# REQ-132 — Control-Plane / Data-Plane Split

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Scaling the engine separately from the panel.

- Standalone engine service (workflows, automation, webhooks) with its own process and health.
- Control-plane server pushing lifecycle events; batched event relay to the panel.
- Response channel back to the caller for synchronous webhook responses.
- Task-runner isolation (in-process sandbox vs child process per risky task).
- Deployment recipes for split vs single-process topologies.

## Implementation spec

> **Band:** migration `0132` reserved; append-only ledger — take the next free number if taken · **Crates:** `crates/workflows` plus new `crates/engine-protocol` · **Binaries:** control plane (`omnion-api`) and data plane (`omnion-engine`) · **Infra:** `infra/compose`, `infra/kubernetes`.

### Scope (in / out)

**In**

- Engine service: a standalone binary that executes workflow runs, triggers, scheduled jobs, webhook delivery work and wait/resume sweeps. It has its own process, its own liveness and readiness probes (readiness includes a dequeue probe so a replica with a broken queue connection never receives traffic), its own metrics endpoint (REQ-126) and its own log stream with an instance label.
- Topology modes: `embedded` (the default: the engine runs inside the API process with zero configuration, exactly as today), `split` (engine service plus panel) and `split-horizontal` (several engine instances claiming from the same queues). One settings key selects the mode, and single-process stays the documented default so small installations are unaffected and the upgrade path is a setting change, not a reinstall.
- Lifecycle push (control plane → engine): workflow create/update/enable/disable/delete, credential reference updates, trigger and schedule changes, module and capability changes, and settings. Messages carry a monotonically increasing revision; an engine that has been away too long (or restarted) requests a full sync instead of applying an unbounded tail.
- Event relay (engine → control plane): batched, ordered per stream, at-least-once with idempotency keys. Each batch carries `seq_start` and `seq_end`; the panel applies a batch in one transaction and acknowledges the highest applied sequence, so replays are safe and duplicates are absorbed by unique constraints rather than by hope.
- Synchronous response channel: a caller may request a response for work submitted through a webhook or API surface. The engine delivers the run result to a waiter keyed by run id and a single-use token; the caller waits up to a bounded timeout (default 10 s, maximum 30 s) and then receives `202` with the run id instead of a lost response. Waiting never holds a worker thread, and a disconnected caller leaves no orphaned work.
- Task-runner isolation: every task carries an isolation class — `shared` (in-process, the default) or `process` (a child process for risky work such as untrusted code, heavy memory consumers or unstable dependencies). The child runner applies limits (memory, CPU time, wall timeout), speaks a line-delimited protocol over stdio, kills the whole process tree on timeout, contains crashes so the engine survives, and never restarts a task that was stopped by the loop guard.
- Deployment recipes: compose profiles for single and split topologies; Kubernetes manifests and chart values with separate deployments; autoscaling hints driven by queue depth; a drain procedure (mark draining → stop claiming → finish in-flight → exit) for upgrades; and a runbook covering failure modes.
- Protocol versioning: `crates/engine-protocol` holds the shared message types. The handshake carries protocol revision and platform version; a mismatch fails fast with an actionable message rather than misbehaving (docs/05-VERSIONING.md).

**Out**

- Extracting the content or panel API into separate services: only execution moves, everything else stays.
- Multi-region execution routing (REQ-035 territory), queue backends beyond the ones REQ-096 supports, and engine-side plugin loading.
- Redefining wait/resume or retry semantics (REQ-090 and REQ-091 own them); the split must preserve them exactly, not improve on them.
- A managed control plane: this REQ ships deployment recipes and observability, not hosted infrastructure.

### Screens (UI)

Routes under the system section of the admin:

| Route | Screen |
|---|---|
| `/system/engine` | Engine fleet: instances, version, region, heartbeat, claimed queues, in-flight, drained state |
| `/system/engine/queues` | Queue depth, oldest item age, claim and failure rates |
| `/system/engine/events` | Relay lag per instance, last applied batch, replay action |
| `/settings/engine` | Mode (embedded/split), relay batch size and interval, response timeout, isolation defaults |
| `/automations/runs/{id}` (extended) | Run detail gains a placement panel: executing instance, queue, isolation class |

- **Fleet table.** Columns Instance, Version, Mode, Region, Started, Last heartbeat, Queues, In-flight, Status. A stale badge appears after three missed heartbeats, with an action to open that instance's last events. Row actions: Drain (with progress), Inspect queues, Copy instance id.
- **Queues screen.** Depth and trend per queue, oldest item age as the primary health signal (depth alone misleads), claim and failure rates, and a `Pause claiming` action that takes effect within one heartbeat and is audited.
- **Relay screen.** Lag in seconds per instance, last applied sequence, failed-batch list with error and a `Replay from sequence` action behind a confirmation and an audit row; a warning banner when lag exceeds the configured threshold, because a silent relay is a silent outage.
- **Embedded-mode states.** When split mode is not enabled the screens explain that the engine runs in-process with a link to the deployment docs; the fleet table shows the single embedded instance rather than an empty screen.
- **States and mobile.** Skeletons, empty states with the next action, error strips with retry; heartbeat timestamps render as relative age with an absolute tooltip. Tables become cards at 390 px and the drain action moves into the row sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/engine/handshake` | Engine registers version, protocol revision and mode | internal service credential |
| POST | `/api/v1/engine/heartbeat` | Liveness plus queue statistics and in-flight count | internal service credential |
| GET | `/api/v1/engine/config` | Lifecycle snapshot from a revision, or a full sync | internal service credential |
| POST | `/api/v1/engine/events/batch` | Apply a relay batch; idempotent by sequence range | internal service credential |
| POST | `/api/v1/runs/{id}/result-channel/wait` | Submit work and optionally wait for the result up to the timeout | `workflows.run` |
| POST | `/api/v1/runs/{id}/result-channel/complete` | Engine publishes the final result to a waiter | internal service credential |
| GET | `/api/v1/system/engine/instances` | Fleet list with heartbeat and load | `system.health.read` |
| GET | `/api/v1/system/engine/queues` | Queue depth, age and claim statistics | `system.health.read` |
| POST | `/api/v1/system/engine/instances/{id}/drain` | Stop claiming and finish in-flight work | `system.engine.manage` |
| GET | `/api/v1/system/engine/events` | Relay lag and applied batches | `system.health.read` |
| POST | `/api/v1/system/engine/events/replay` | Replay from a sequence (audited) | `system.engine.manage` |
| GET · PUT | `/api/v1/settings/engine` | Read · write mode, batching, timeouts, isolation defaults | `settings.manage` |

Internal service routes are reachable only with a dedicated service credential minted at deploy time; user sessions and API keys are refused on them, and the panel never exposes them in the OpenAPI document's public section. The wait route returns the result when it arrives inside the timeout, and `202` with the run id when it does not — one deterministic contract for callers.

### Data model

Migration `0132_control_plane_split.sql`.

```sql
engine_instances (id uuid pk, name text, kind text in ('embedded','engine'), version text, protocol_rev int, region text null,
  claimed_queues text[] default '{}', in_flight int default 0, status text default 'active' in ('active','draining','stale','stopped'),
  started_at timestamptz, last_heartbeat_at timestamptz, metadata jsonb default '{}')  index (status, last_heartbeat_at)
engine_event_batches (id bigserial pk, instance_id uuid -> engine_instances, seq_start bigint, seq_end bigint, event_count int,
  status text default 'applied' in ('applied','failed'), applied_at timestamptz, error text)  unique (instance_id, seq_start, seq_end)
run_placements (run_id uuid pk -> workflow_runs, instance_id uuid -> engine_instances, queue text, isolation text default 'shared',
  claimed_at timestamptz, finished_at timestamptz, outcome text null)  index (instance_id, finished_at), (queue, claimed_at)
result_waiters (id uuid pk, run_id uuid -> workflow_runs, token_hash text unique, timeout_at timestamptz,
  status text default 'waiting' in ('waiting','delivered','timed_out','cancelled'), result jsonb null, delivered_at timestamptz,
  created_at timestamptz)  index (status, timeout_at)  -- rows removed after delivery or timeout plus one retention day
run_isolation (run_id uuid pk, isolation text default 'shared' in ('shared','process'), reason text, memory_mb int, cpu_ms int,
  wall_ms int, exit_signal text null, created_at timestamptz)
-- engine-owned table (same database, engine role):
engine_outbox (id bigserial pk, stream text, seq bigint, event text, payload jsonb, idempotency_key text unique,
  created_at timestamptz, batched_at timestamptz null)  index (stream, seq) where batched_at is null
```

Notes: relay tables are append-only and engine-owned, panel tables are control-plane-owned, which keeps the two writers off each other's rows. Embedded mode bypasses the relay tables through an in-process transport implementing the same interface, so there is one execution code path and one set of tests. `result_waiters.timeout_at` is enforced against database time, never client clocks. Retention: batches and placements follow the run history retention policy; waiters are swept by the same sweeper that handles expired waits (REQ-090).

### Events

| Event | When | Payload sketch |
|---|---|---|
| `engine.instance.registered` · `.drained` · `.stale` | Fleet lifecycle transitions | `instance_id`, `version`, `region` |
| `engine.batch.applied` · `engine.replay.requested` | Relay state changes | `instance_id`, `seq_start`, `seq_end`, actor for replays |
| `workflow.run.placed` | A run is claimed by an instance | `run_id`, `instance_id`, `queue` |
| `workflow.run.isolated` · `.isolation_violation` | Child-process start, and termination for a limit breach | `run_id`, `node_id`, `reason`, `limit` |
| `workflow.run.completed` (relayed) | Run result applied in the panel, same contract as embedded | `run_id`, `status`, `duration_ms` |

Consumed: `workflow.updated` · `.enabled` · `.disabled` (REQ-003, REQ-095) bump the lifecycle revision; `credential.updated` and secret rotation (REQ-087, REQ-125) invalidate engine caches; `settings.updated` (REQ-112) refreshes engine configuration; `system.update.applied` (REQ-078) triggers a drain check before restart.

Webhook relevance: yes — `engine.instance.stale` and `engine.batch.applied` lag metrics are what an operations team subscribes to, and `workflow.run.completed` keeps an identical shape in both topologies so downstream automations never learn about the split. Payloads carry ids, counts and codes, never task payloads or credential material.

### Acceptance criteria

- [ ] A workflow runs end to end in split mode: trigger in the panel, execution on the engine, result visible in the run history with the executing instance recorded.
- [ ] Embedded mode is byte-for-byte behaviourally unchanged: the existing run test suite passes without modification in the default topology.
- [ ] The engine survives being killed mid-run: on restart it recovers per REQ-091 policy, resumes or retries correctly, and no side effect is applied twice (proved by a counted test workflow).
- [ ] Duplicate relay batches are absorbed by idempotency: replaying an applied batch changes nothing and reports success.
- [ ] Relay ordering holds per stream: batches applied out of order are refused with a clear error, and the next expected sequence is reported.
- [ ] After a simulated panel outage, relay lag recovers to zero and no event is lost — the engine buffers or refuses new claims rather than dropping events.
- [ ] `Replay from sequence` reproduces the same projections it produced originally, and the action is audited.
- [ ] A caller submitting work with a response channel receives the result inside the timeout, and receives `202` plus the run id when the timeout expires — never a hung connection.
- [ ] A caller that disconnects mid-wait leaves no orphaned waiter: the row expires and the run still completes and is recorded.
- [ ] Response-channel waits do not hold worker slots: a load test with many waiting callers keeps the engine's claim rate unaffected.
- [ ] An isolated task exceeding its memory or wall limit is killed with its process tree, the run records `isolation_violation` with the limit named, and the engine's own health is unaffected.
- [ ] An isolated task that crashes the child process is retried per policy but never restarts a loop-guarded task.
- [ ] Drain then upgrade loses no runs: after drain the instance claims nothing new, in-flight runs finish, and the new version registers with a heartbeat.
- [ ] A protocol revision mismatch refuses the handshake with a message naming both revisions, and the panel's fleet screen shows the mismatched instance as unhealthy instead of unknown.
- [ ] Stale detection marks an instance stale after three missed heartbeats and fires the event; a recovered instance returns to active without operator action.
- [ ] Both deployment recipes (compose profile and Kubernetes manifests) start the split topology from a clean checkout and pass the same smoke run.

### QA plan

Bring up the split topology with the compose profile and run the smoke workflow from the panel; repeat in embedded mode to prove parity; kill the engine process mid-run and verify recovery without duplicated side effects (a counter workflow proves it); stop the panel for two minutes, restart it and watch relay lag drain to zero, then prove replay idempotency by replaying an applied batch; exercise the response channel with an immediate result, a timeout result and a client disconnect; run the isolation scenarios (memory cap, wall-timeout, crash) and confirm the engine stays healthy and the run record is honest; drain an instance and restart it on a newer protocol fixture to see the mismatch path. Visual check: the fleet table shows live heartbeat age, the queues screen shows the oldest-item-age metric, the relay screen shows lag counting down, and the run detail placement panel names the executing instance.

### Slices

1. **Engine binary and fleet plumbing.** Handshake, heartbeat, run placement, drain, health and metrics endpoints, embedded-vs-split setting. *Done when:* acceptance 1–2 and 15 pass and the fleet screens render live data.
2. **Lifecycle push and event relay.** Revisioned config sync, engine outbox, batched application with idempotency, lag surfaces and replay. *Done when:* acceptance 4–7 and 16 pass, including the panel-outage recovery scenario.
3. **Response channel.** Waiter creation, bounded waiting, delivery, timeout and disconnect handling. *Done when:* acceptance 8–10 pass under load.
4. **Task isolation and deployment recipes.** Child-process runner with limits, crash containment, compose and Kubernetes recipes, drain runbook. *Done when:* acceptance 11–12 and 14 pass, and a misbehaving task is contained without operator intervention.

### Risks / notes

- Two writers on one database invite coupling: relay and outbox rows are owned by exactly one side, and any future shared table must be designed with the same rule before it lands.
- At-least-once delivery means duplicates are normal: every relay consumer path must be idempotent, and a unique constraint stays the last line of defence rather than application checks alone.
- Clocks lie: heartbeat staleness, waiter timeouts and relay lag all compare database time, never the clock of whichever process is reporting.
- The response channel is a friendly denial-of-service target if unbounded: hard maximum timeout, per-caller concurrency cap and no thread-held waiting.
- Child-process isolation is a safety net, not a security boundary: it contains crashes and resource abuse, but untrusted code still requires the sandboxing model of the plugin runtime, and the docs must say so plainly.
- Version skew between engine and panel is the most likely production incident after a partial upgrade: fail the handshake loudly, show the instance as unhealthy, and keep the previous version's protocol support for one release window.
- Embedded mode must remain the default and the best-tested path; split mode may never become a prerequisite for any feature that the embedded mode cannot serve.
- Observability must label every metric and log line with the instance id from day one, or split-topology debugging degenerates into guessing which replica did what.
