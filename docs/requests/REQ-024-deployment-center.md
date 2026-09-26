# REQ-024 — Deployment Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + infra
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

From the admin panel:

```text
Deployment

Production
● Healthy

Version
2.4.1

Available
2.5.0

[View Changes]
[Deploy]
[Rollback]
```

For Kubernetes-based enterprise deployments, also show:

```text
Replicas: 6
CPU: ...
Memory: ...
```

## Notes

- Ties into the update manager / rollback flow in docs/05-VERSIONING.md (§12–§13).

## Implementation spec

### Scope (in / out)

**In**

- `/deployment` centre in `apps/admin`: environment cards, release browser, deploy wizard with pre-flight, live log, history, rollback, maintenance window, and a conditioned cluster panel.
- **Update awareness:** a scheduled check reads the release manifest feed for the configured channel and caches what it found, so the card shows *current* vs *available* without a network call on page load; `update.available` is emitted once per newly seen version (dedupe on channel + version).
- **View Changes:** release detail with notes, breaking-change flags, the migrations the version ships, minimum core version and the artifact checksum.
- **Deploy:** three steps — pre-flight → confirm → run. Pre-flight reports backup freshness, pending migrations, free disk space, running background jobs, dependency health and whether a maintenance window is required. Production requires typing the target version. The run streams step-by-step output and ends with a health verification.
- **Rollback:** to the previous known-good version, with a mandatory reason and an automatic backup before it starts, using the same step log. Append-only migrations mean a rollback never drops columns, so the older binary can still read the data.
- **Maintenance window:** a configurable banner in every admin session, write routes blocked with `503` and the configured message, reads and health probes untouched.
- **Cluster panel (conditional):** replicas desired/ready, per-workload CPU and memory (request, limit, usage) with a 30-minute sparkline, rollout status, and a workload restart behind a typed confirmation. A single-instance deployment instead shows process uptime, resident memory and a confirmed service restart. The route only renders when the runtime reports a cluster — never a disabled card as a tease.
- **History:** every deploy, rollback and restart with actor, from → to version, duration, result, log and release-notes link.

**Out**

- Provisioning infrastructure, cloud consoles, autoscaling configuration, node maintenance, container builds and release publishing.
- Multi-region orchestration and failover (REQ-035); air-gapped upgrade bundles from removable media (REQ-036) — both linked, not built here.
- Blue-green and canary strategies: rolling/restart with a verified rollback is the bar.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/deployment` | Environment cards, version summary, primary actions |
| `/deployment/releases`, `/releases/{version}` | Channel-filtered release list; notes, migrations, compatibility, checksum |
| `/deployment/deploy?to={version}` | Wizard: pre-flight → confirm → run (live log) |
| `/deployment/history` | Deploy/rollback/restart history with filters and expandable steps |
| `/deployment/kubernetes` | Cluster panel (renders only when a cluster is reported) |
| `/deployment/maintenance` | Window settings, banner message, schedule, current state |

- Environment card: name · health dot + label (`Healthy` / `Degraded` / `Unreachable`) · current version · available version · last deploy (actor + time) · uptime · `View Changes`, `Deploy`, `Rollback`. The dot carries a tooltip with the last probe time and the failing probe when degraded; `Rollback` is disabled with a reason when no previous good version exists.
- Version block matches the brief: `Version 2.4.1` / `Available 2.5.0`; when up to date it reads `Available — (up to date)` instead of an empty field.
- Wizard step 1: check · status (pass/warn/fail) · detail · suggested action. A `fail` disables `Continue`; a warning requires an acknowledgement checkbox.
- Wizard step 2: from → to version, release notes summary, migrations to run, estimated downtime, backup state, and the typed confirmation for production.
- Wizard step 3: step timeline (backup → migrate → deploy → verify), streaming log pane with an auto-scroll toggle, elapsed time, cancel while pre-migration, and a result banner linking to the history entry.
- History table columns: Started · Environment · From → To · Kind · Actor · Duration · Result · Log; filters by environment, kind, result and window.
- Maintenance form: message (≤ 280), start/end (end after start, or open-ended), scope (all / admin-only), enable toggle behind a confirmation. While enabled a persistent shell banner appears and the toggle is highlighted.
- Cluster rows: workload · replicas desired/ready · CPU request/limit/usage · memory request/limit/usage · restarts · age, plus a rollout banner when desired ≠ ready.
- States: card skeletons while loading; "no releases for this channel" empty state; "manifest feed unreachable — showing cached data from {time}" banner; error state with request id and retry; Deploy disabled with a reason while another job runs.
- Keyboard: `d` opens the deploy wizard, `r` opens rollback, `l` toggles log auto-scroll, `Esc` closes dialogs; the log pane keeps its own scroll behaviour.
- Mobile: cards stack, wizard is one column with the timeline on top, the log pane keeps its own scroll region, destructive actions keep the same typed confirmation.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/deployment/version` | Current version + build metadata (also used by the shell footer) | `deployment.read` |
| GET | `/api/v1/deployment/environments` | Environment cards with health and versions | `deployment.read` |
| GET | `/api/v1/deployment/environments/{id}` | One environment: detail, last probe, last deploy | `deployment.read` |
| GET | `/api/v1/deployment/releases` | Releases for a channel, newest first | `deployment.read` |
| GET | `/api/v1/deployment/releases/{version}` | Notes, breaking changes, migrations, compatibility | `deployment.read` |
| POST | `/api/v1/deployment/environments/{id}/preflight` | Pre-flight checks for a target version | `deployment.manage` |
| POST | `/api/v1/deployment/environments/{id}/deploy` | Start a deploy (`to_version`, `confirm_version`, `backup_first`) | `deployment.manage` |
| GET | `/api/v1/deployment/jobs/{id}` | Job status, step list, progress | `deployment.read` |
| GET | `/api/v1/deployment/jobs/{id}/log` | Log stream (SSE) with a cursor fallback for polling | `deployment.read` |
| POST | `/api/v1/deployment/jobs/{id}/cancel` | Cancel before the migrate step begins | `deployment.manage` |
| POST | `/api/v1/deployment/environments/{id}/rollback` | Roll back (`to_version`, `reason`) | `deployment.rollback` |
| GET | `/api/v1/deployment/history` | Deploy/rollback/restart history with filters | `deployment.read` |
| GET | `/api/v1/deployment/checks` | Update-check state (last run, next run, channel) | `deployment.read` |
| POST | `/api/v1/deployment/checks/run` | Run an update check now | `deployment.manage` |
| GET | `/api/v1/deployment/cluster` | Replicas, CPU/memory, rollout status; `404` when not a cluster | `deployment.cluster.read` |
| POST | `/api/v1/deployment/cluster/restart` | Restart a workload (`workload`, `confirm`) | `deployment.manage` |
| GET | `/api/v1/deployment/maintenance` | Current window state + settings | `deployment.read` |
| PUT | `/api/v1/deployment/maintenance` | Enable/disable and configure the window | `deployment.maintenance` |

Errors: `400` unknown target version or a confirm value that does not match, `403` permission miss, `409` another job already running for that environment, `422` pre-flight failed so the deploy is refused, `503` from write routes during a maintenance window (with the configured message), `404` on cluster routes when the runtime is not cluster-based.

### Data model

Migration: `database/migrations/0014_deployments.sql` (next free number at build time).

- `deployments` — `id uuid pk default gen_random_uuid()`, `environment text not null`, `kind text not null` (`deploy|rollback|restart`), `from_version text null`, `to_version text null`, `status text not null default 'preflight'` (`preflight|running|verifying|succeeded|failed|cancelled`), `strategy text not null default 'rolling'`, `started_by uuid null references users(id) on delete set null`, `reason text null`, `backup_id uuid null`, `log_key text null`, `error text null`, `started_at timestamptz not null default now()`, `finished_at timestamptz null`, `duration_ms int null`; check `environment in (production,staging,sandbox)`; indexes `(environment, started_at desc)` and a partial index on active statuses so one-job-per-environment is enforceable.
- `deployment_steps` — `id bigint generated always as identity pk`, `deployment_id uuid not null references deployments(id) on delete cascade`, `position int not null`, `name text not null`, `status text not null default 'pending'`, `output text not null default ''`, `started_at timestamptz null`, `finished_at timestamptz null`; unique `(deployment_id, position)`.
- `releases_cache` — `version text pk`, `channel text not null default 'stable'`, `released_at timestamptz null`, `notes_md text not null default ''`, `breaking boolean not null default false`, `migrations text[] not null default '{}'`, `core_min text null`, `artifact_checksum text null`, `checked_at timestamptz not null default now()`; lets `/deployment` render while the feed is unreachable.
- `maintenance_windows` — `environment text pk`, `enabled boolean not null default false`, `message text not null default ''`, `starts_at timestamptz null`, `ends_at timestamptz null`, `scope text not null default 'all'` (`all|admin`), `updated_by uuid null`, `updated_at timestamptz`.
- `environment_health` — `environment text pk`, `version text not null`, `status text not null`, `checked_at timestamptz not null default now()`, `details jsonb not null default '{}'` (probe results: database, cache, object storage, background worker, pending migrations).
- `cluster_metric_samples` — `id bigint generated always as identity pk`, `environment text not null`, `sampled_at timestamptz not null default now()`, `replicas_desired int`, `replicas_ready int`, `cpu_millicores int null`, `memory_bytes bigint null`, `restarts int null`; index `(environment, sampled_at desc)`, pruned to a short window. Live values are still read from the runtime on demand — the table exists for the sparkline.
- `deployment_steps.output` and `cluster_metric_samples` are pruned by the same scheduled job family as the request logs, with the retention window shown in the UI.

### Events

- **Emitted:** `deployment.check.completed`, `update.available`, `deployment.started`, `deployment.step.failed`, `deployment.succeeded`, `deployment.failed`, `deployment.rolled_back`, `deployment.cancelled`, `deployment.maintenance.enabled`, `deployment.maintenance.disabled`, `deployment.cluster.restart_requested`.
- **Consumed:** package-manager mirror events (REQ-044) update the installed component versions the card summarises; `backup.completed` (REQ-013) closes the pre-flight backup-freshness check.
- Webhook relevance: `update.available` and every `deployment.*` event are subscribable so an operator can wire a chat message per deploy result. Payloads carry environment, versions, result, duration and job id — never a log dump, never credentials.
- Notification relevance: a failed deploy notifies every holder of `deployment.manage` through the REQ-021 router.

### Acceptance criteria

- [ ] `/deployment` renders environment cards with the brief's fields (name, health, Version, Available, View Changes, Deploy, Rollback).
- [ ] The update check runs on schedule, caches the manifest and emits `update.available` once per new version.
- [ ] The up-to-date state reads clearly instead of showing an empty Available field.
- [ ] `View Changes` opens a release detail with notes, migrations, breaking flags and checksum.
- [ ] Pre-flight reports each check with pass/warn/fail and blocks `Continue` on a failure.
- [ ] Production deploy requires the exact target version; a mismatch is rejected client and server side.
- [ ] A deploy runs its steps in order, streams the log, and ends with a health verification.
- [ ] A second deploy for the same environment while a job runs is refused with `409`.
- [ ] Cancel is available before the migrate step and refused after it starts, with a message.
- [ ] A failed step marks the deployment `failed` and offers rollback from the result banner.
- [ ] Rollback requires a reason, takes a backup first, and produces a history entry.
- [ ] History filters by environment, kind, result and window; rows expand to the step list.
- [ ] Maintenance mode blocks write routes with `503` plus the message, shows the banner, and leaves reads and probes working.
- [ ] The cluster panel renders only when a cluster is reported, with real replicas, CPU and memory.
- [ ] Workload restart requires confirmation and `deployment.manage`.
- [ ] A single-instance deployment shows the alternative card with a working restart action.
- [ ] The feed-unreachable banner appears with the cached timestamp when the manifest call fails.
- [ ] Every screen has empty, loading and error states, and no placeholder numbers anywhere.
- [ ] Mobile keeps the log pane in its own scroll region; keyboard shortcuts work.
- [ ] `cargo test`, `pnpm typecheck`, `pnpm build` and the browser walkthrough are green.

### QA plan

The walkthrough must: open `/deployment`, assert the card fields and the health tooltip; open `View Changes` and read a release; start the deploy wizard, walk all three steps and cancel before the migrate step (the harness must not deploy a live environment); toggle maintenance on, assert the banner and a `503` on a write route, then toggle it off; open history and expand a row to its steps; filter releases by channel; on a non-cluster deployment assert the Kubernetes route shows an honest "not a cluster-based deployment" state instead of an empty panel.

Visual check should see: a health indicator that is unmistakable with text (not colour alone), Version/Available aligned on every card, a step timeline with no clipped labels, a log pane with its own scrollbar and monospace text that does not overflow, and destructive actions visually distinct from `Deploy`.

### Slices

1. **Read-only centre.** Migration, version endpoint, environment cards, release list + detail from the cached manifest, update-check job with `update.available`, history list. *Done when:* the card shows current vs available and `View Changes` renders a real release.
2. **Deploy wizard.** Pre-flight, deploy job with step records and log streaming, confirm-by-typing, cancel rule, result banner, history detail. *Done when:* a deploy of a locally built version completes end to end with a health verification and a history row.
3. **Rollback + maintenance.** Rollback with reason and pre-backup, failure-path rollback entry point, maintenance enforcement and banner. *Done when:* a rollback returns the instance to the previous version and a window visibly blocks writes.
4. **Cluster panel.** Cluster detection, live replica/CPU/memory read, metric sampling for the sparkline, conditional route, workload restart with confirmation. *Done when:* a cluster-backed environment shows real numbers and restart works, while a single-instance environment shows the alternative card with no empty cluster shell.

### Risks / notes

- This screen can take the platform down: every destructive action is permission-guarded, confirmed by typing, logged with an actor and reversible. A deploy that cannot roll back is not shipped.
- Pre-flight exists to stop a deploy that would fail halfway, so it must be honest — if backup freshness cannot be determined that is a visible warning, never a silent pass.
- Rollback safety rests on the append-only migration rule from the versioning docs. A release that drops or renames a column is flagged breaking and needs an explicit acknowledgement that data would be restored from backup instead.
- The cluster panel reads runtime data through the API: no cluster credential reaches the browser, and the restart action is treated as an audited write.
- Keep the manifest feed optional: an instance that cannot reach it (offline or air-gapped, REQ-036) must still show its own version and history, with the cached banner explaining what is stale.
