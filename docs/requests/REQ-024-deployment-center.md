# REQ-024 — Deployment Center

> **Status:** in-progress (`d2ced888`, `4ca9609a`, `34661bce`, `a24c452a`, `9a77f1c4`; tick 97 — **slice 3 is now shipped as code in full: rollback with its mandatory reason and pre-backup, the maintenance window's decisions, screen, `503` enforcement, and the persistent shell banner — and the card's Rollback button is no longer a dead span.** Slices 1, 2 and 3 are all open on the *same* thing now: one browser pass over this build. The tick-96 pass still running was compiled before every one of these commits and cannot see the wizard, the rollback dialog, the maintenance screen or the banner — so it measures slice 1's five screens and nothing else. Gates that are real: 68 crate tests, 325 api-lib tests, `tsc` clean, `node --check` clean, and six window/rollback properties proved against the live `omnion_qa_w5` database by their own constraint names.)
> · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + infra
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
- [x] The update check runs on schedule, caches the manifest and emits `update.available` once per new version.
- [x] The up-to-date state reads clearly instead of showing an empty Available field.
- [ ] `View Changes` opens a release detail with notes, migrations, breaking flags and checksum.
- [x] Pre-flight reports each check with pass/warn/fail and blocks `Continue` on a failure. *(server + unit: a `fail` **or** an `unknown` blocks, and the route repeats the report on the deploy itself — the panel is not the enforcement point.)*
- [x] Production deploy requires the exact target version; a mismatch is rejected client and server side.
- [ ] A deploy runs its steps in order, streams the log, and ends with a health verification.
- [x] A second deploy for the same environment while a job runs is refused with `409`. *(the `0211` partial unique index is the guard; `create_job` matches the refusal on the constraint name and answers with the blocking job's id.)*
- [x] Cancel is available before the migrate step and refused after it starts, with a message.
- [ ] A failed step marks the deployment `failed` and offers rollback from the result banner.
- [x] Rollback requires a reason, takes a backup first, and produces a history entry. *(server + unit + live: a `rollback` row without a reason is refused by `deployments_rollback_needs_a_reason`; the route refuses an empty one with a `400` before it writes anything, and the dialog refuses it client-side in the same words. The plan is `backup → deploy → verify` — **not** the deploy's four — because the older binary reads the append-only schema and `may_cancel` treats a plan with no migrate step as cancellable throughout. Proved on the live database: three step rows in that order, zero `migrate` rows.)*
- [x] History filters by environment, kind, result and window; rows expand to the step list.
- [ ] Maintenance mode blocks write routes with `503` plus the message, shows the banner, and leaves reads and probes working. *(the API half is proved — the deploy route asks the window before the pre-flight and answers `503` with the operator's own message, and the three form rules are `422`s with their reasons. **Not yet ticked:** the browser half. The banner is not in the shell and the driven pass that would catch a window stuck open has never run over this build.)*
- [ ] The cluster panel renders only when a cluster is reported, with real replicas, CPU and memory.
- [ ] Workload restart requires confirmation and `deployment.manage`.
- [ ] A single-instance deployment shows the alternative card with a working restart action.
- [x] The feed-unreachable banner appears with the cached timestamp when the manifest call fails.
- [ ] Every screen has empty, loading and error states, and no placeholder numbers anywhere.
- [ ] Mobile keeps the log pane in its own scroll region; keyboard shortcuts work.
- [ ] `cargo test`, `pnpm typecheck`, `pnpm build` and the browser walkthrough are green.

### QA plan

The walkthrough must: open `/deployment`, assert the card fields and the health tooltip; open `View Changes` and read a release; start the deploy wizard, walk all three steps and cancel before the migrate step (the harness must not deploy a live environment); toggle maintenance on, assert the banner and a `503` on a write route, then toggle it off; open history and expand a row to its steps; filter releases by channel; on a non-cluster deployment assert the Kubernetes route shows an honest "not a cluster-based deployment" state instead of an empty panel.

Visual check should see: a health indicator that is unmistakable with text (not colour alone), Version/Available aligned on every card, a step timeline with no clipped labels, a log pane with its own scrollbar and monospace text that does not overflow, and destructive actions visually distinct from `Deploy`.

### Slices

1. **Read-only centre.** Migration, version endpoint, environment cards, release list + detail from the cached manifest, update-check job with `update.available`, history list. *Done when:* the card shows current vs available and `View Changes` renders a real release.
   *Status:* **the decision layer is shipped and green; no route and no screen exists yet**
   (`b7d9e1da`, `f8bd11ac`). The card in the brief is four lines and one of them is the dangerous
   one, so the parts that can be wrong without looking wrong are a crate with its own tests
   rather than expressions at a handler: `Version` is parsed and ordered (so `1.10.0` never
   sorts below `1.9.0` and build metadata never invents an upgrade), `Channel::admits` is a
   complete gate, and `Availability` has a third `Blocked { candidate, reason }` variant so a
   release that exists but may not be offered is reported *with its reason* — the two states where
   the alternative is an empty field, which the spec names as a bug in its own words.
   `preflight.rs` keeps `Unknown` as a fourth state distinct from `Pass` and `Warn`: the spec
   says a check that cannot be answered is visible, never a silent pass, and folding it into
   `Warn` would make it acknowledgeable. A check the caller forgot is filled in as `Unknown`, so
   a partial report **blocks** rather than passing with six of seven rows.
   `job.rs` holds the cancel boundary as a rule: cancellable up to the step before `migrate`, not
   from it onward, because past that point stopping is a rollback rather than a cancel.
   `manifest.rs` holds the `update.available` dedupe as a `SeenSet` keyed on **channel +
   version**, and a feed that cannot be read is a state carrying a reason rather than an error —
   the spec requires the instance to keep rendering while offline.
   The `0211` migration carries one-job-per-environment as a **partial unique index** over the
   three active states (`verifying` included), so the `409` is enforcement rather than a racy
   check, and it was validated against a live database: every check constraint bites, a second
   active job on a *different* environment is allowed, and the file applies twice without error.
   **Shipped since that paragraph** (`c8cd8404`, `87b0cdb4`): the seven read routes, the
   scheduled check worker, the `0212` migration and the five screens. Three of them are decided
   in this tick and are worth writing down, because each is a place the obvious version is wrong:
   the card's `Available` line is **computed by the server** and travels as a three-state
   `availability` enum, because a panel that compared two version strings itself would offer
   `1.9.0` as an upgrade from `1.10.0` and no test on the panel would catch it; a **dead feed is
   a state and not an error**, so the cache is left exactly as it was and the run is recorded
   failed with its reason, because clearing the cache takes the centre down every time the
   publisher's CDN has a bad afternoon; and the `update.available` **dedupe is the database's**,
   one `insert … on conflict do nothing` whose affected-row count is the answer, so a scheduled
   pass and an operator pressing "check now" in the same second emit one event per version by
   construction.
   **One bug worth recording because I wrote it first:** the dedupe claim is written *before* the
   event is emitted, so an emit failure left a version marked "seen" that was never announced,
   and no later check would ever announce it — a silent, permanent loss behind a screen that
   cheerfully reports "nothing new since the last check". A failed emit now releases the claim it
   just took; the recovery is a duplicate at worst, which a consumer can handle.
   **Still open on this slice:** the browser pass. The screens are rendered by `tsc` and by
   nothing else, so the screen-level boxes stay unticked until the walkthrough reports.
2. **Deploy wizard.** Pre-flight, deploy job with step records and log streaming, confirm-by-typing, cancel rule, result banner, history detail. *Done when:* a deploy of a locally built version completes end to end with a health verification and a history row.
   *Status:* **shipped as code (`6f20f516`, `0e22b88b`); the browser pass that closes it is still owed.**
   The store half is `crates/deployment/src/jobs.rs` and the decision it enforces is the one the
   `0211` migration already had a constraint for: one active job per environment is a *partial
   unique index*, and `create_job` turns its refusal into a `409` by matching on the constraint
   **name** — not on `23505`, which a different unique index could also raise, and not with a
   check-then-insert, which is a race whose loser overwrites a live deploy's row.
   The runner is deliberately not a deployer. It takes the backup, applies each migration while
   naming it, records the rollout, and then its `verify` step reads the version the instance
   **reports** and compares it to the version it was asked to move to. The binary swap belongs to
   `infra/deploy/deploy-omnion-live.sh`, and the log says so. That is not a limitation worked
   around: a runner that *claimed* to swap the process and did not would write the single most
   dangerous row in this table — a succeeded deploy that changed nothing — and a `verify` step that
   read back its own target instead of the instance's would make every such deploy green.
   Two refusals carry a **reason** rather than a status: `cancel` past the migrate step answers
   `409` with `cancel_refusal`'s sentence, so the panel can say "this is a rollback now" and offer
   the button, and a `409` on a busy environment carries the blocking job's id in `details` so the
   operator is not hunting through history for which deploy is in the way.
3. **Rollback + maintenance.** Rollback with reason and pre-backup, failure-path rollback entry point, maintenance enforcement and banner. *Done when:* a rollback returns the instance to the previous version and a window visibly blocks writes.
   *Status:* **the code is whole and green (`d2ced888`, `4ca9609a`, `34661bce`, `a24c452a`); the
   browser pass that closes it is still owed, and one part of it is not built yet.**
   The window's three decisions are in the crate, which is the point: `is_active` needs three
   conditions and the third is the one a naive check drops — *ending* a window is what leaves
   `enabled = true` in the table, so a check that only reads the toggle blocks writes for ever
   after the window closes. That is proved on the live database rather than asserted: an ended
   window reports `enabled=t, started=t, not_ended=f` and the enforcement query returns `0`.
   The scope is a checkbox that decides who is refused, and the difference is the difference
   between "the site is down for customers" and "I am changing settings, do not fight me" — so
   an `admin` window refuses the panel write and **not** the public one.
   The `503` carries the operator's own sentence and carries **no `Retry-After`**: a window is
   open-ended by default, and a header that counts down to nothing teaches a client to retry on
   a timer for ever. A client that got a generic "service unavailable" could not tell a planned
   window from an outage, which is the distinction that decides whether it waits or pages.
   A deploy now asks the window **before the pre-flight**, not after it: a deploy refused for a
   window must not have taken a backup or written an audit row claiming it started.
   **The two things this slice still owes, named rather than implied.** The shell banner is not
   in the app shell yet — the API computes `active` and the screen uses it, but every admin
   session has no banner, and the spec asks for one in every session. And the driven pass that
   would catch a window stuck open has never run: `a24c452a` is what that pass is, and it needs
   a pass over this build to have produced a verdict. The screen's own catch-block behaviour is
   untested in the way that matters — a form that swallows its own refusal and shows nothing is
   indistinguishable from a save that worked until you look at the database.
4. **Cluster panel.** Cluster detection, live replica/CPU/memory read, metric sampling for the sparkline, conditional route, workload restart with confirmation. *Done when:* a cluster-backed environment shows real numbers and restart works, while a single-instance environment shows the alternative card with no empty cluster shell.

### Risks / notes

- This screen can take the platform down: every destructive action is permission-guarded, confirmed by typing, logged with an actor and reversible. A deploy that cannot roll back is not shipped.
- Pre-flight exists to stop a deploy that would fail halfway, so it must be honest — if backup freshness cannot be determined that is a visible warning, never a silent pass.
- Rollback safety rests on the append-only migration rule from the versioning docs. A release that drops or renames a column is flagged breaking and needs an explicit acknowledgement that data would be restored from backup instead.
- The cluster panel reads runtime data through the API: no cluster credential reaches the browser, and the restart action is treated as an audited write.
- Keep the manifest feed optional: an instance that cannot reach it (offline or air-gapped, REQ-036) must still show its own version and history, with the cached banner explaining what is stale.
