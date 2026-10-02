# REQ-014 — System Health

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Much more advanced than the simple health screen in WordPress:

```text
SYSTEM HEALTH

API             ✓ Healthy
PostgreSQL      ✓ Healthy
Redis           ✓ Healthy
S3              ✓ Healthy
Workers         ✓ 4/4
Queue           ✓ Healthy
Search          ✓ Healthy

CPU             34%
Memory          61%
Disk            42%
```

## Implementation spec

New crate `crates/health` (probe registry, sample store, incident tracking, threshold policy) plus the admin section `/health`. It reads the existing clients in `crates/core` (PostgreSQL pool, Redis), `crates/storage`, the search abstraction and the workflow execution tables; `apps/api` already answers the unversioned `/healthz` and `/readyz` probes — those stay untouched, the panel reads the versioned API.

### Scope (in / out)

**In**

- Probe registry with one probe per dependency, each returning `state` (`healthy|degraded|down| unknown`), latency, and a detail object: **API** (self, event-loop and build version), **PostgreSQL** (round-trip on a fixed statement plus pool stats), **Redis** (PING plus used-memory and connected clients), **S3** (HEAD on a dedicated probe object), **Workers** (heartbeat rows, `n/m` counts by worker kind), **Queue** (pending/claimed counts and the oldest pending age), **Search** (index reachable and document count).
- Host metrics on the API host: CPU load and utilisation, memory used/total, disk usage per mounted volume — read from the kernel interfaces, with a cgroup-aware path for containers.
- Sample history: each run writes samples; the panel charts 1 h and 24 h trends and a 7-day daily roll-up; a retention task prunes raw samples older than 30 days.
- Incidents: a state transition opens an incident (with a resolution time when the service recovers); operators can acknowledge with a note. Incidents are the honest history the overview summarises.
- Threshold policy: warn/critical limits per metric (disk, memory, CPU, probe latency, queue depth, worker heartbeat age), check interval, and maintenance windows per service that suppress incident creation without hiding the state.
- Status for other centres: a single `platform health` summary consumed by the security overview and the operator dashboard; readiness is not invented here, it is read from the real probes.

**Out**

- APM/trace UI, log search and log shipping; alert routing (notifications go through the notification centre, this request only decides what is alert-worthy); auto-scaling and self-healing actions (restart, scale, failover) — this screen reports, it does not remediate; per-tenant health views; synthetic external uptime monitoring from outside the deployment.

### Screens (UI)

- `/health` — overview. Banner summarising the worst state ("All systems operational" / "Degraded: Redis"). Service rows: **Service · Status · Latency (p95, 5 min) · Last checked · Detail (chevron)**; every row links into its detail screen. Metric cards: **CPU · Memory · Disk · Load average · DB connections · Queue depth**, each with the current value, the threshold marker and a 24 h sparkline. Controls: "Run all checks", auto-refresh select (5 s / 15 s / 60 s / off, default 15 s, persisted per user), "Last checked" timestamp. Worker card shows `4/4` style counts with the list of worker kinds on hover; when a worker is stale it is named explicitly.
- `/health/services/{key}` — service detail: current state, checks table **Check · State · Latency · Message · Checked at**, a 24 h trend chart, recent failures with timestamps, and the configuration the probe uses with credential references masked. Contains a "Run this check" action.
- `/health/metrics` — metric table/graphs: **Metric · Current · Min · Avg · Max (1 h / 24 h) · Threshold · State** with a sparkline per row, a range selector (1 h / 24 h / 7 d) and CSV export of the current range.
- `/health/incidents` — table: **Opened · Service · From → To · Duration · State (open/resolved) · Acknowledged by · Note**. Filters: service, state, date range. Row opens the incident with its sample context. Actions: acknowledge (with note), resolve manually when a service recovered during a maintenance window. Bulk: acknowledge.
- `/health/settings` — threshold form grouped per metric: interval seconds (5–600), worker heartbeat stale after seconds (30–3600), and warn/critical pairs — disk %, memory %, CPU %, probe latency ms, queue depth count — each validated numerically and shown ordered; maintenance windows table (**Starts · Ends · Services · Note · Created by**) with a create form that rejects an end before a start; notification toggles for degraded, recovered and threshold breach. Saving refreshes the next run immediately.
- States: live values never render as `—` unless the probe genuinely returned `unknown` with a message; loading shows skeleton rows and badges; an error banner offers retry. A failed manual run surfaces which probe errored instead of blanking the page. Keyboard: `r` refresh, `f` toggle auto-refresh, `/` filter, `⌘K` palette. Mobile: service rows become cards, the metric grid turns into a single column, charts keep a fixed height so layout does not jump.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/health/overview` | Service states, metric snapshot, summary banner | `health.read` |
| POST | `/api/v1/health/checks/run` | Run every probe now and record samples | `health.manage` |
| GET | `/api/v1/health/services/{key}` | One service with checks and recent failures | `health.read` |
| GET | `/api/v1/health/samples` | Samples for a metric and range (1h/24h/7d) | `health.read` |
| GET | `/api/v1/health/metrics` | Aggregated metric table for the current range | `health.read` |
| GET | `/api/v1/health/incidents` | Incident list (paged, filtered) | `health.read` |
| PATCH | `/api/v1/health/incidents/{id}` | Acknowledge or resolve an incident | `health.manage` |
| GET | `/api/v1/health/settings` | Thresholds, intervals, maintenance windows | `health.read` |
| PUT | `/api/v1/health/settings` | Save thresholds and intervals | `health.manage` |
| GET | `/api/v1/health/maintenance-windows` | Window list | `health.read` |
| POST | `/api/v1/health/maintenance-windows` | Create a maintenance window | `health.manage` |
| DELETE | `/api/v1/health/maintenance-windows/{id}` | Remove a window | `health.manage` |
| GET | `/api/v1/health/summary` | One-line platform health for other centres | `health.read` |

New catalogue keys (category `health`): `health.read`, `health.manage`. Reading health may be broader than changing thresholds; `POST /checks/run` is a mutation (it writes samples) and therefore rides `health.manage`. `/healthz` and `/readyz` stay unversioned and unguarded.

### Data model

**`health_samples`** — `id bigint generated always as identity pk`, `service text not null` (`api|postgres|redis|s3|workers|queue|search|host`), `metric text not null`, `value double precision not null`, `unit text not null default ''`, `state text not null`, `detail jsonb not null default '{}'`, `sampled_at timestamptz not null default now()`. Indexes `(service, metric, sampled_at desc)` and `(sampled_at)` for retention pruning.

**`health_incidents`** — `id uuid pk default gen_random_uuid()`, `service text not null`, `from_state text not null`, `to_state text not null`, `summary text not null default ''`, `detail jsonb not null default '{}'`, `started_at timestamptz not null default now()`, `resolved_at timestamptz null`, `suppressed boolean not null default false`, `acknowledged_by uuid null → users(id) on delete set null`, `acknowledged_at timestamptz null`, `note text null`. Indexes `(started_at desc)` and `(service) where resolved_at is null`; a constraint keeps `suppressed` incidents out of the open list.

**`health_settings`** — single row: `id smallint pk default 1 check (id = 1)`, `check_interval_seconds int not null default 60` (5–600), `worker_stale_seconds int not null default 120` (30–3600), `thresholds jsonb not null default '{}'` (per metric warn/crit), `notifications jsonb not null default '{}'`, `updated_by uuid null → users(id)`, `updated_at timestamptz not null default now()`.

**`health_maintenance_windows`** — `id uuid pk default gen_random_uuid()`, `starts_at timestamptz not null`, `ends_at timestamptz not null`, `services text[] not null default '{}'` (empty means all), `note text not null default ''`, `created_by uuid null → users(id)`, `created_at timestamptz not null default now()`. Constraint `ends_at > starts_at`; index `(starts_at, ends_at)`.

**`worker_heartbeats`** — `id text pk` (worker name), `kind text not null`, `host text not null`, `version text not null`, `state text not null default 'running'`, `started_at timestamptz not null default now()`, `last_seen_at timestamptz not null default now()`, `meta jsonb not null default '{}'`. Index `(kind, last_seen_at desc)`. This is the table that makes "Workers 4/4" a fact.

Migration: `database/migrations/0014_system_health.sql`, append-only, commented in the `0009` style.

### Events

**Emitted:** `health.service.degraded`, `health.service.recovered`, `health.threshold.breached`, `health.incident.acknowledged`, `health.checks.completed`. Each carries the service key, the state transition, the metric and the sample window — no raw host detail beyond what the panel shows.
**Consumed:** `backup.completed` (refreshes destination-related storage counters) and `worker.heartbeat.missed` if a worker reports its own loss; nothing else is required.

Webhook relevance: `health.service.degraded` and `health.service.recovered` are the two an operations endpoint subscribes to; `health.threshold.breached` fires at most once per metric per window so a flapping disk does not flood an endpoint. Audit entries use the `health.*` namespace, written only for manual actions (run checks, settings change, acknowledge, maintenance window).

### Acceptance criteria

- [ ] `crates/health` exists with a probe registry and one probe per dependency, unit-tested.
- [ ] `database/migrations/0014_system_health.sql` applies on fresh and populated databases.
- [ ] `/health` shows all seven services from the request sketch with real states, not constants.
- [ ] Stopping Redis flips its row to `down` within one interval and restores on recovery.
- [ ] Worker counts come from heartbeat rows; stopping a worker changes `4/4` to `3/4` and names it.
- [ ] CPU, memory and disk values match the host within a small tolerance and update on refresh.
- [ ] Auto-refresh (15 s) visibly updates timestamps and values without a manual reload.
- [ ] "Run all checks" records a new sample set and reports per-probe failures instead of failing whole.
- [ ] Service detail lists each probe with latency and message and charts the last 24 h.
- [ ] A state transition opens an incident; recovery resolves it with a duration.
- [ ] Acknowledging an incident stores the actor, the note and the timestamp.
- [ ] A maintenance window suppresses incident creation while the state still shows degraded.
- [ ] Thresholds save and a breach beyond the critical limit emits `health.threshold.breached` once.
- [ ] Out-of-range settings (interval 0, heartbeat 0, warn above critical) are refused with messages.
- [ ] Metric ranges (1 h, 24 h, 7 d) return real aggregates and CSV export matches the range shown.
- [ ] Sample retention prunes raw samples older than 30 days without touching incidents.
- [ ] Reads require `health.read`; run-checks and settings require `health.manage` (`403` otherwise).
- [ ] Walkthrough passes with zero high findings.

### QA plan

The walkthrough must visit `/health`, `/health/metrics`, `/health/incidents`, `/health/settings`, and a service detail page, and click: auto-refresh, "Run all checks", one service row, the metric range selector, CSV export, acknowledge on an incident fixture, and save an invalid threshold to capture the validation message. It should also exercise the degraded path in the QA stack (stop the Redis container during the pass, confirm the panel turns red, then restart it and confirm recovery — the disposable stack exists for exactly this). Visual check should see: status badges distinguishable without colour, sparklines drawn from real samples rather than flat lines, the metric grid aligned, no `NaN`/`Infinity` text anywhere, and mobile cards that keep the status and the value visible without scrolling.

### Slices

1. **Probes + overview** — schema for samples and settings, probe registry, run loop, `/health` with the seven service rows and the metric cards. Done: stopping a dependency changes the panel and restarting it recovers, all from real probes.
2. **History + service detail** — sample aggregation, sparklines, ranges, per-service detail and CSV export, retention pruning. Done: 24 h trends render from stored samples and pruning keeps the configured retention.
3. **Incidents + thresholds** — transition detection, incident store, acknowledge/resolve, threshold policy and breach events, `/health/incidents`. Done: a scripted outage produces one incident with a duration and an acknowledgement that persists.
4. **Workers + maintenance + summary** — worker heartbeats, stale detection, maintenance windows, platform summary endpoint feeding the security overview, webhook for degraded/recovered. Done: a killed worker is reported as stale and the summary endpoint reflects the worst current state.

### Risks / notes

- Probes must be cheap and must never take the platform down: short timeouts (1–3 s), no shared connection starvation, and a probe failure must degrade its own row only.
- Host metrics differ across cgroup v1/v2 and bare metal; fall back to `unknown` with a message rather than showing a wrong number, and never divide by a zero total.
- Sample volume: keep one row per metric per interval, aggregate on read where possible, and prune; a per-request sample would bloat the table within days.
- The panel must not become an alarm firehose: one incident per transition, breach events deduplicated per window, and maintenance windows honoured by all emitters.
- Baseline values (what "normal" CPU or queue depth is) differ per deployment, so thresholds start empty with sensible defaults and a first-run hint instead of hard-coded assumptions.
