# REQ-096 — Queue Mode & Scaling

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Running automation at volume.

- Worker pools with concurrency control per workflow and per queue.
- Queue naming and routing; job processor; worker health endpoint.
- Leader election and distributed locking so scheduled work runs once.
- Pub/sub fan-out of execution events; cross-instance execution stop.
- Multi-instance webhook relay and worker lifecycle events pushed to the control plane.

## Implementation spec

### Scope (in / out)

**In**

- **Worker pools** — named pools (`default`, `heavy`, `interactive`) each with its own concurrency, accepted queue keys, claim batch size and idle backoff. A step worker declares its pool, registers on boot, heartbeats while alive, and can be **drained** (stop claiming, finish in-flight work, exit) for deploys.
- **Queue naming and routing** — every executed step carries a queue key. A workflow sets `queue_mode` (`shared` uses the pool's default key, `dedicated` derives a key from the workflow) and may pin a pool; the key plus priority decides which workers claim it first. Per-queue limits and per-workflow `concurrency_limit` (queue or refuse, per REQ-003) are enforced at claim time.
- **Job processor with graceful lifecycle** — one loop per worker: claim a batch, execute, heartbeat, record, repeat; idle backoff grows to a ceiling and resets immediately on a wake notification. Shutdown on termination signal stops claiming, lets in-flight steps finish inside a grace window (default 60 s), then exits; a hard kill is safe because claims expire.
- **Worker health endpoint** — liveness (process alive) and readiness (database reachable, pool registered, no drain, heartbeat fresh) for orchestration probes, plus a registry view listing instances, pools, queue keys, in-flight counts, build version and last heartbeat.
- **Worker lifecycle events pushed to the control plane** — registered, ready, draining, drained, stopped, crashed (inferred from a stale heartbeat by a janitor tick) and build-version mismatch, so a rolling deploy that leaves old and new workers running is visible.
- **Leader election and distributed locking** — single-winner leases held in the database (with Redis-backed leases when Redis is configured and reachable, database always the fallback) for the periodic jobs: cron scheduler tick, wait sweeper (REQ-091), retention prune (REQ-093) and catalogue seed (REQ-094). A lease is claimed with a conditional insert, renewed on a heartbeat, and checked before each batch, so a lock that expires mid-tick stops work at the next boundary rather than double-running.
- **Pub/sub fan-out of execution events** — execution lifecycle events are published to a channel so every instance can update live views and learn about state changes; the database stays authoritative and a lagging subscriber resynchronises from it rather than trusting the channel.
- **Cross-instance execution stop** — a cancel request may land on an instance that does not own the run: the request is written to the authoritative row, published on the channel, and honoured by the owning instance, which is woken by the notification or detects the watermark at its next batch boundary (REQ-091). Sub-executions stop with their parent.
- **Multi-instance webhook relay** — an inbound trigger or webhook delivery is accepted by any instance, deduplicated by a delivery key with a TTL, converted into a claimable job, and executed by whichever worker claims it. Responses are deterministic (accepted with the run id, or reported as duplicate) and identical on every instance.
- **Backpressure and scale hygiene** — a queue whose depth crosses its threshold switches the overflow policy to wait or refuse with a clear code, the refusal is visible in the trace and to the caller, per-instance connection budget and claim batch size stay configurable, and an emergency **Stop all for this workflow** action stops every in-flight run of one workflow across instances.
- **Operations surface** — one screen for pools, queues, instances, leases and drains, plus a live "running now" attribution showing which instance runs what.

**Out**

- Queue broker replacement (external streaming systems) and autoscaling machinery — a deployment concern; this request keeps claim semantics in PostgreSQL so a single-instance install needs no extra service.
- Multi-region routing and edge workers — REQ-035.
- Engine correctness (durable steps, ledger, guards, cancellation semantics) — REQ-091; this request deploys that contract across processes.
- Cron schedule definitions and the schedule editor — owned by REQ-003's trigger work.

### Screens (UI)

- **`/settings/workflows/workers`** — tabs: **Instances** (key, pool, version, status, in-flight, last heartbeat, Drain and Resume actions), **Pools** (name, concurrency, accepted queue keys, claim batch, idle backoff, edit), **Queues** (key, pool, priority, depth, throughput 5 min, oldest waiting job, concurrency limit, backpressure threshold and overflow policy), **Leases** (name, holder, acquired, renewed, expires, force release), and a health strip summarising readiness across instances with the probe endpoint shown for operators.
- **`/automations/executions` (live strip)** — a collapsible "Running now" panel listing active runs with workflow, current node, elapsed, assigned instance and queue key; updates live via fan-out and falls back to polling when the channel is unavailable.
- **Engine panel (REQ-091)** — queue depth and sweep counters reuse this request's data; the panel links to `/settings/workflows/workers` rather than duplicating it.
- **Run detail** — the header shows the instance key and queue key that executed the run, and the cancel action reports whether the request was delivered live or will land at the next boundary.
- **States** — empty and error states per tab; a queue in backpressure is visually distinct with its policy named; a crashed instance is marked stale with its last heartbeat time; drains in progress show remaining in-flight work.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflow-workers` | Registered instances with pool, version, in-flight, heartbeat | `workflows.ops` |
| GET | `/api/v1/workflow-workers/health` | Liveness and readiness for probes (no session) | none (health) |
| POST | `/api/v1/workflow-workers/{key}/drain` | Request a drain, stop claiming | `workflows.ops` |
| POST | `/api/v1/workflow-workers/{key}/resume` | Cancel a drain | `workflows.ops` |
| GET | `/api/v1/settings/workflows/pools` | Pool definitions and concurrency | `workflows.ops` |
| PATCH | `/api/v1/settings/workflows/pools/{name}` | Change concurrency, queue keys, batch, backoff | `workflows.ops` |
| GET | `/api/v1/settings/workflows/queues` | Queue state: depth, throughput, oldest job, policy | `workflows.read` |
| PATCH | `/api/v1/settings/workflows/queues/{key}` | Limits, priority, backpressure threshold, overflow policy | `workflows.ops` |
| GET | `/api/v1/settings/workflows/leases` | Current leases with holder and expiry | `workflows.ops` |
| POST | `/api/v1/settings/workflows/leases/{name}/release` | Force-release a stuck lease | `workflows.ops` |
| GET | `/api/v1/workflow-executions/{id}/dispatch` | Instance and queue attribution for one run | `workflows.read` |
| POST | `/api/v1/workflows/{id}/stop` | Stop every in-flight run of a workflow across instances | `workflows.run` |

One new permission key: `workflows.ops`, held by the operator role. Queue reads stay on `workflows.read` so run debugging works without operator rights.

### Data model

Migration `database/migrations/0020_queue_scaling.sql` (next free number if taken).

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_pools` | name text pk, concurrency int default 4, queue_keys text[] default array['default'], claim_batch int default 5, idle_backoff_ms int default 1000, max_idle_backoff_ms int default 30000, drain boolean default false, updated_at | checks: concurrency 1–128, claim_batch 1–50, backoff ladder positive and max ≥ base |
| `workflow_queues` | key text pk, pool text → workflow_pools (name) on delete set default, priority int default 5, concurrency_limit int, backpressure_depth int default 1000, overflow_policy text ('queue','refuse'), updated_at | index `(pool, priority)`; checks: priority 0–9, limits positive when set |
| `workflow_workers` | key text pk, pool text → workflow_pools, queue_keys text[], version text, status text ('starting','ready','draining','stopped'), in_flight int default 0, started_at, last_heartbeat_at, stopped_at, drain_reason text | index `(pool, last_heartbeat_at desc)`; a heartbeat older than 30 s is treated as crashed by the janitor |
| `instance_leases` | name text pk ('cron','sweeper','prune','seed'), holder_key text, acquired_at, renewed_at, expires_at, released_at | index `(expires_at)`; a lease is claimable when `released_at is not null or expires_at < now()` |
| `workflow_delivery_keys` | key text pk, workflow_id uuid → workflows cascade, first_seen_at, expires_at | index `(expires_at)` for the prune tick; the key is the trigger's delivery id plus workflow id |
| `workflow_run_stops` | workflow_id uuid → workflows cascade, requested_by uuid → users set null, requested_at, scope text ('workflow','queue'), reason text | index `(workflow_id, requested_at desc)`; read by workers before each claim |

Adds to `workflows`: `queue_key text`, `pool text default 'default'`, `priority int default 5`, `concurrency_limit int`. Adds to `workflow_executions`: `runner_key text`, `queue_key text`, `dispatched_via text ('worker','relay','manual')`. Adds to `workflow_steps`: `claimed_by text` for attribution while a step is in flight.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.worker.registered` / `.ready` | emitted | key, pool, queue keys, version |
| `workflow.worker.draining` / `.drained` / `.stopped` | emitted | in-flight count at each transition |
| `workflow.worker.crashed` | emitted | janitor-detected stale heartbeat, key and last heartbeat |
| `workflow.worker.version_mismatch` | emitted | two build versions claim one pool |
| `workflow.queue.backpressure` | emitted | key, depth, threshold, policy applied |
| `workflow.queue.recovered` | emitted | depth fell back under the threshold |
| `workflow.lease.acquired` / `.lost` / `.force_released` | emitted | lease name, holder, reason |
| `workflow.execution.stop_requested` | emitted | workflow or execution scope, requesting instance |
| `workflow.event.fanout_lagged` | emitted | subscriber fell behind and resynchronised from the database |
| `workflow.delivery.duplicate` | emitted | relay dropped a repeated delivery key |

Fan-out publishes the existing execution lifecycle events unchanged; this request adds no new execution semantics, only propagation.

### Acceptance criteria

- [ ] Two worker processes in one pool claim disjoint batches; a thousand queued steps are executed exactly once each with no lost claim.
- [ ] Worker concurrency is respected: raising a pool's concurrency raises observed in-flight steps up to the limit and never beyond.
- [ ] Per-workflow concurrency limit is enforced with the configured overflow policy, and a refused run explains the limit in its trace and to the caller.
- [ ] A dedicated queue key receives only its workflow's steps and a shared key serves several workflows on one pool.
- [ ] A drain lets in-flight steps finish inside the grace window, stops claiming immediately, and the instance leaves the registry with a `drained` event.
- [ ] Health endpoint answers liveness and readiness correctly: not ready while draining, not ready with a stale heartbeat, ready when healthy, and it works without a session.
- [ ] A stale heartbeat is detected within one janitor tick and reported as a crashed instance with its last heartbeat time.
- [ ] Lease election gives exactly one winner for the cron tick, the sweeper, the prune and the seed across three instances; a lease that expires mid-tick stops further batches from that holder.
- [ ] A force-released lease is immediately claimable by another instance and the release is audited.
- [ ] Scheduled work fires once across three instances over a five-minute window, proving leader election end to end.
- [ ] Live run state reaches every instance through fan-out, and a subscriber with the channel disabled falls back to polling without stale or duplicated rows.
- [ ] Cancelling a run on an instance that does not own it stops the run within one batch boundary and the run detail says how the stop was delivered.
- [ ] Stop-all for a workflow stops every in-flight run of it across instances and records the requester and reason.
- [ ] A webhook delivered twice with the same delivery key starts one run; the second answers duplicate and emits the duplicate event; the key expires on its TTL.
- [ ] A webhook delivered to an instance with no free workers is accepted, queued, and executed by another instance — never dropped.
- [ ] Backpressure switches per policy: with `queue`, runs wait and depth is visible; with `refuse`, the caller receives a clear code, and the recovery event fires when depth falls back.
- [ ] A single-instance deployment with no Redis configured passes the full suite using the database path only.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the exclusive-claim and lease tests each fail when their guard is removed.

### QA plan

The walkthrough must: start three worker instances in one pool (docker or three local processes) plus one in a second pool; register them and read the Instances tab; queue a burst of runs and watch depth, throughput and in-flight; set a per-workflow concurrency limit of 1 and confirm serialised execution; switch one workflow to a dedicated queue key and confirm isolation; drain one instance mid-run and confirm it finishes in-flight work and leaves; inspect and force-release a lease; kill an instance outright and watch the janitor report it crashed; deliver the same webhook twice and read the duplicate answer; deliver a webhook while all workers are draining and confirm a later claim executes it; cancel a run from a second instance's UI and read the delivery note; trigger backpressure with `refuse` and `queue` and read both behaviours; run the same suite with the channel disabled to confirm the polling fallback.

The visual check must see: queue depth and throughput readable without colour alone, a draining instance distinct from a ready one, backpressure warnings attached to their queue row, the live strip not shifting layout as rows update, and no clipped operator copy at 1024px.

### Slices

1. **Worker pools, queues and routing** — pool and queue tables, worker registry with heartbeats, health endpoint, claim-time limits and priority, drain lifecycle, settings screens.
   *Done when:* three workers across two pools execute a burst exactly once with concurrency respected and a drain leaving no orphaned claims.
2. **Leases and scheduled-work single-winner** — lease table and election logic, janitor tick, lease screen and force release, conversion of the periodic jobs to lease-gated runners.
   *Done when:* three instances produce exactly one cron tick and one sweep per interval, and an expired holder does no further work.
3. **Fan-out, cross-instance stop and webhook relay** — channel publishing with database authority, delivery-key dedupe, relay acceptance path, cross-instance cancel and stop-all, lagged-subscriber resync.
   *Done when:* a cancel from any instance stops the run it targets and a duplicated webhook delivery starts one run.
4. **Backpressure, attribution and live views** — thresholds and policies with events and caller codes, run and step attribution columns, the live running strip.
   *Done when:* both overflow policies are demonstrably enforced and every live run shows its instance and queue.

### Risks / notes

- Redis is an accelerator, never a dependency: every mechanism here must work with PostgreSQL alone, or a single-node install becomes a second-class citizen.
- Claim semantics stay in one statement — `for update skip locked` with an expiry — because two claim paths is how a step runs twice under load.
- Leases are the split-brain risk: check before each batch, renew on a heartbeat longer than the batch's worst case, and prefer stopping to guessing.
- Fan-out is best-effort: the database is authoritative, and a subscriber must resynchronise rather than trust a gap-free channel.
- Backpressure must be observable at the caller, or users see mysterious stalls; every refusal carries a code, an explanation and the queue key.
- Grace windows during deploys must be shorter than the claim expiry, or drains leave work claimed by process that no longer exists.
- Connection budgets matter more than concurrency numbers: raise pool concurrency only with a matching database connection budget.
