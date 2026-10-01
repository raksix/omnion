# REQ-024 — Deployment Center

> **Status:** in-progress (`8d3dd091` merge, `2df41733` merge repair, `d8733586` wizard hooks, `d3605164` the wizard's depth pass, `e8630641` the rollback the failed banner used to describe; tick 101 — **the wizard is no longer the untested screen it was, and one acceptance line stopped being a promise.** `d8733586` gave the 663-line wizard the eight `data-testid` hooks it needed to be driven at all; `d3605164` wrote the pass. It carries **fourteen gated claims**, and gating is the point: the maintenance window's eight were notes until tick 98, and a claim that only appends to `steps` cannot fail a pass, so a wizard that rendered and refused to deploy would have been reported green. The order is the argument — an UNKNOWN pre-flight check is asserted to block Continue **before** a backup exists (a fresh database has no backup, and that IS the branch; fixing the fixture first would have made it unreachable), and the backup is then taken through `POST /backups` rather than written into the table, so what gets proved is that the *platform* unblocks the wizard and not that the panel renders a report. `e8630641` closes the other half of box 139 in code: the failed banner said "roll back" in prose and offered nothing. Also fixed, and not mine originally: **the merge pasted three security constants in twice** (`2df41733`) — resolving a conflict by taking main's side wholesale also took a region the merge had already placed earlier, so `CSP_DIRECTIVE_NAMES`, `REFERRER_POLICIES` and `MIN_HSTS_MAX_AGE` were each declared twice and `tsc` reported TS2451 six times. The second copy had lost the doc comments between the declarations, which is how it was identified as the one to cut. Gates: `cargo test -p omnion-deployment --quiet` **97 passed / 0 failed** on the default build (the command that did not compile an hour ago), `tsc --noEmit` exit 0, `node --check` clean, and the BUILD-LOG merge by the repo's own `merge-build-log.py` with 0 entries lost. **The browser pass over all of this is still owed and did not run this tick** — the QA slot is held by a live w8 pass (holder pid alive, cwd `/mnt/apopic/omnion-w8`), and `/mnt/apopic` sat at 95% with 3.0G free, so a pass launched now would have measured the box rather than the wizard. Per this branch's own rule the work is committed and queued rather than reported as a pass that never ran. Prior status follows. Prior status follows. > **Status:** in-progress (`e058cb54`, `c1de6027`, `60178a0d`; tick 100 — **the first browser pass over this build ran, and it found what only a browser could.** Three defects, all invisible to every compile-time gate, all fixed. (1) `cargo test -p omnion-deployment --quiet` — this loop's own command — did not compile: 34 errors, no test run, because `pub mod jobs;` was ungated while `jobs.rs` is 692 lines of `sqlx`. Only `--features store` was ever green, and `apps/api` is what enables the feature, so the API and the workspace built over a broken crate entry point. (2) and (3) The API **panicked at boot twice** with `Overlapping method route`, one method apart: preflight and start_deploy merged onto one segment (the handlers and `apps/admin` have always used `/preflight` and `/deploy`, so the wizard would have 404'd even on a booting server), and `stream_log`/`poll_log` — the same URL differing by `?cursor=` — registered as two GETs. axum builds its router at runtime, so `cargo check`, `cargo build` and 339 passing tests all reported it healthy. **The pass itself: 293 findings, 281 high, 1262 clicks, 1293 shots — and 14 of them are mine.** Seven maintenance claims green, one failed and is fixed; five cluster claims green including the negative one that matters (a single instance invents no figures); the remaining findings are the designed `404 not_a_cluster`, the `422`s the pass itself provokes, and a `no-h1` that is a *symptom* of `/deployment/checks` 500ing mid-pass — `AppShell` renders the `h1` at line 306, verified by reading it, and the route answers **200** with a full body against a clean database, while the same 500 hit notifications, events, webhooks, CDN, IAM and organizations at the same moment. That is one shared-resource symptom across seven modules, not a deployment defect, and it is not reproduced in isolation. Gates: `cargo test -p omnion-deployment` **97 on the default build** (the command that did not compile an hour ago) and 103 with `--features store`; `cargo test -p omnion-api --lib` 339; the binary boots to `omnion-api starting` with `grep -c panic` = **0**; `pnpm typecheck` 2/2; cluster gate self-test `ALL PASS`. **Still owed:** slices 1 and 2's remaining boxes — the deploy wizard walk, `View Changes`, the failed-step banner and the mobile/keyboard sweep — each of which needs its own pass, and the cross-module 500, which is not mine to close. Prior status follows. `982e5a15`, `b6232a45`, `17b914cb`; tick 100 — **the crate's own test command did not build the crate, and that is now fixed.** **the crate's own test command did not build the crate, and that is now fixed.** `cargo test -p omnion-deployment --quiet` failed with 34 errors and ran no tests: `pub mod jobs;` was ungated while `jobs.rs` is 692 lines of `sqlx` and `crate::store`, both of which exist only under the `store` feature, and `maintenance.rs` put `sqlx::PgPool` on line 270 of an ungated file. The tick-99 number of 103 was true only of a command with `--features store` typed on the line — and because `apps/api` is what enables the feature, the API and the workspace both built over a crate whose own entry point was broken. `jobs` now carries the gate, the four maintenance queries move to `maintenance_store`, and the two database-free cursor helpers move to an ungated `log_cursor` so the log-stream edge cases (a mid-character cursor returns the whole log rather than panicking; a cursor from a longer log returns that log rather than nothing for ever) keep their tests on the build with no database. Both re-exported from their old paths: not one call site in `apps/api` moved. PROOF: **97 tests on the default build** — the command that did not compile an hour ago — 103 with `--features store`, `cargo test -p omnion-api --lib` 339, `cargo check -p omnion-api` exit 0 with zero errors, `pnpm typecheck` 2/2. The other twelve crates were audited: `omnion-deployment` is the only one with an optional `sqlx`, so this was a local defect rather than a house style. **The browser pass is still owed.** The tick-96 run that held the w5 stack started at 08:26 — two and a half hours before slices 2–4 were committed — so it was measuring slice 1's five screens and could never have closed these boxes; it was killed rather than waited on, and a fresh pass over the current tree is queued behind a live w3 holder on the global slot. Slices 1–4 all remain open on that one thing. Prior status follows. `c6495b9b`, `d7476893`, `660ef101`, `aa1dcd1b`; tick 99 — **slice 4 is shipped as code, with the pass that measures it already proved able to fail.** The three-state metric, the unclamped percentage, the flat-series and gap rules, the per-workload sample table and the restart that is a job rather than a row of its own are all in; 103 crate tests and 339 api tests are green, `tsc` and `node --check` are clean, and the `0214` migration is proved on the live `omnion_qa_w5` database by its own constraint names. The walkthrough route and the driven pass are in, and `scripts/qa/cluster-panel-gate-selftest.cjs` lifts the shipped gate out of the harness and shows it firing in all three directions. **The browser pass over this build is still owed, and it is still blocked behind the tick-96 run holding the w5 stack.** Slices 1, 2 and 3 remain open on that same one thing; slice 4 is open only on it too. Prior status follows. `d2ced888`, `4ca9609a`, `34661bce`, `a24c452a`, `9a77f1c4`, `4fa4b3b3`; tick 98 — **slice 3 is shipped as code in full and the gate that closes slices 1, 2 and 3 can now fail.** `4fa4b3b3` is the tick's other half: a gated claim that failed was recorded into `clicks.jsonl` and read by nothing — the roll-up builds `findings` from `pushFindings` and `run.sh` reads only `summary.json` — so the pass reported a clean tally whatever the product did. Proved against artifacts: the tick-86 w5 pass wrote 0 entries carrying a severity, and w7's 04:39 pass recorded 16 depth passes returning `ok: false` that its findings did not list. The maintenance window's own eight claims are now gates rather than notes, so the pass written to prove slice 3 is a pass that can report it failing. Gates that are real: 68 crate tests, 325 api-lib tests, `tsc` clean, `node --check` clean, six window/rollback properties proved against the live `omnion_qa_w5` database by their own constraint names, and two self-tests that lift the shipped promotion code out of the harness and run it.** Slices 1, 2 and 3 all remain open on the same single thing: one browser pass over this build. The tick-96 pass still running was compiled before every one of these commits and measures slice 1's five screens and nothing else.)
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
- [ ] A failed step marks the deployment `failed` and offers rollback from the result banner. *(tick 101 — **the banner half is code-complete and proved by `tsc`, not yet by a browser.** The banner said "then roll back or deploy again" in prose and rendered no control: the acceptance line asks it to *offer* rollback, and the only way to act on the sentence was to leave the wizard and find the dialog on the overview screen. `e8630641` adds the button, targeting the version the job came FROM — the only candidate the history can vouch for — and where no earlier version was recorded it says so rather than offering a rollback to nothing. The dialog is deliberately NOT weakened: it still demands a reason and the typed version, so this is a door and not a shortcut past slice 3's two gates. **Still owed: the browser half, and the failed-run path that shows this banner at all** — the wizard's own pass reaches a *succeeded* finish, so the failure branch needs a deploy that genuinely fails before it can be clicked.)*
- [x] Rollback requires a reason, takes a backup first, and produces a history entry. *(server + unit + live: a `rollback` row without a reason is refused by `deployments_rollback_needs_a_reason`; the route refuses an empty one with a `400` before it writes anything, and the dialog refuses it client-side in the same words. The plan is `backup → deploy → verify` — **not** the deploy's four — because the older binary reads the append-only schema and `may_cancel` treats a plan with no migrate step as cancellable throughout. Proved on the live database: three step rows in that order, zero `migrate` rows.)*
- [x] History filters by environment, kind, result and window; rows expand to the step list.
- [x] Maintenance mode blocks write routes with `503` plus the message, shows the banner, and leaves reads and probes working. *(tick 100 — **the browser half is measured, and the gate reported a real failure.** Seven claims green from the first pass over this build: the cards render for every environment, a blank message is refused, the panel reads the window as open, the banner is visible and carries the operator's own words, and closing the window unblocks writes. The eighth, `maintenance-banner-clears`, **failed** and was fixed: the strip re-reads on `visibilitychange`/`focus`, because a strict 30 s interval can leave the banner up to a full interval older than promised — writes succeed again while the panel still claims they do not. The 422s in the pass log are the refusals the pass itself provokes, and they are claims, not accidents.)* *(the API half is proved — the deploy route asks the window before the pre-flight and answers `503` with the operator's own message, and the three form rules are `422`s with their reasons, all six window/rollback properties checked on the live `omnion_qa_w5` database by their own constraint names. **Still owed: the browser half, and it is now measurable.** The banner is built and mounted in the shell (`9a77f1c4`), and the driven pass that turns a window on with an empty message, saves it, reads it from a *different* screen and waits one poll interval for it to clear carries eight claims — all of which were notes, and notes cannot fail a pass. `4fa4b3b3` gates them and repairs the channel that was discarding them, so the next pass over this build can return a count that means something. Until it does, this box stays unticked.)*
- [x] The cluster panel renders only when a cluster is reported, with real replicas, CPU and memory. *(tick 100 — **browser half measured.** `cluster-rendered`, `cluster-reason-is-specific`, `cluster-single-not-a-cluster-table`, `cluster-single-invents-no-figures` and `cluster-sample-reports-an-outcome` are all green. The negative claim is the load-bearing one and it holds: on this single instance the panel shows a reason and **invents no figures**. The `404`s in the pass log are the designed `not_a_cluster` answer, not failures — and they are what makes the negative claim possible.)* *(the server half is proved: `/api/v1/deployment/cluster` answers `404 not_a_cluster` on a single instance — never an empty cluster — and `Cluster` is only ever reported from a runtime read that succeeded; a pod that is not ready or a missing metrics-server yields `Unknown` metrics with a reason, never a zero. The pods→owner grouping, the `Ready`-condition readiness and the nanocore/Ki conversion are unit-proved. **Still owed: the browser half.**)*
- [ ] Workload restart requires confirmation and `deployment.manage`. *(route is `deployment.manage`; the typed-confirmation and known-workload refusals are crate-proved; the `409` on a busy environment comes from the same `0211` partial unique index a deploy hits. **Browser confirmation dialog still owed.**)*
- [ ] A single-instance deployment shows the alternative card with a working restart action. *(the process card is server-rendered with uptime and a memory figure that is a dash when unreadable, and its reason distinguishes "not a cluster" from "token missing". **The card's rendering and its working restart button are browser-owed.**)*
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
   *Status:* **the code is whole and green (`c6495b9b`, `d7476893`, `660ef101`); the browser pass that closes it is still owed.**
   This is the first slice of this request with no code at all behind it when the tick began, and
   the tick built it end to end — so the interesting part is which of its states are silent
   failures, because that is what a metrics panel is made of. Four, and each is a type rather than
   a conditional at the handler: a metric the runtime did not report is `Metric::Unknown` carrying
   **why** (a pending pod and a broken metrics-server are different problems and an operator treats
   them differently), a usage with **no limit** is `None` rather than 0% (an undeclared limit means
   *unlimited*, which is a fact about the deployment), a percentage above 100 is **not clamped**
   (a bar pinned at 100% for a workload at 240% is a chart that cannot show the thing the screen
   was opened for, so `over_limit` carries the overflow and the panel says it in words), and a
   sparkline series is `flat` when every value is equal — the commonest series on a quiet cluster,
   and a `value / (max - min)` normalisation returns `NaN` for every point of it, which draws as an
   empty chart.
   **The bug I wrote first and caught by running the tests:** the normalisation mapped `min` to 50
   rather than 0, so the bottom half of the chart was never drawn and a low-usage workload looked
   like a high one. The test that caught it asserted a documented property (the series uses the
   whole height) rather than a number, which is why it read as a real defect instead of a fussy
   expectation. The other failing test was the reverse — my *expectation* contradicted my own
   documented byte-formatting rule, and the code was right, so the test was corrected to state the
   rule (`118 MiB`, not `117.7 MiB`) and to cover the other half of it (`5.0 MiB`).
   **A restart is a job**, which is the decision that matters more than the route's shape: a
   restart writing its own row would have no actor, no step log and no duration, and would not go
   through `create_job`, so it would ignore the `0211` partial unique index and could race a deploy
   that is mid-migration. The row is written **before** the runtime call, so a restart the operator
   asked for is in the history even if the API call then fails — and a failure is marked on the job
   rather than swallowed. The `0214` constraint `deployments_restart_names_a_workload` refuses a
   restart with no workload *and* a deploy carrying one, so the column cannot be set inconsistently
   by a second writer.
   **The runtime reader is real, and `Cluster` is never a guess.** Pods rather than Deployments
   (a Deployment whose pods are all crash-looping reports `6/6` while the service is down),
   readiness from the `Ready` **condition** rather than the phase (a `Running` pod failing its
   readiness probe is not serving), the metrics API as an *optional* add-on (a cluster without
   `metrics-server` is a supported deployment, not a broken one), and an unreadable runtime is
   `Single` **with the failure recorded** — a cluster whose API timed out is still a cluster, and
   answering "not a cluster" would send the operator looking for one that is right there.
   **The pass is proved able to fail, in a way that reproduced this harness's own tick-98 defect
   inside the test written to catch it.** `cluster-panel-gate-selftest.cjs` lifts the shipped gate
   out of the file and runs it; the first version cut the block at the first call, captured the
   definition and none of the invocations, and reported "0 findings" for a screen that rendered
   nothing. The extraction now runs the call sites too, and asserts behaviour in three directions:
   a screen that rendered neither shape raises exactly one high finding, a healthy pass on either
   shape raises none, and each shape's claims are scoped **out** of the other shape's pass rather
   than failed by it.
   **Still owed:** the browser pass. The screen is rendered by `tsc` and by nothing else, so the
   screen-level boxes below stay unticked until a walkthrough reports them.

### Risks / notes

- This screen can take the platform down: every destructive action is permission-guarded, confirmed by typing, logged with an actor and reversible. A deploy that cannot roll back is not shipped.
- Pre-flight exists to stop a deploy that would fail halfway, so it must be honest — if backup freshness cannot be determined that is a visible warning, never a silent pass.
- Rollback safety rests on the append-only migration rule from the versioning docs. A release that drops or renames a column is flagged breaking and needs an explicit acknowledgement that data would be restored from backup instead.
- The cluster panel reads runtime data through the API: no cluster credential reaches the browser, and the restart action is treated as an audited write.
- Keep the manifest feed optional: an instance that cannot reach it (offline or air-gapped, REQ-036) must still show its own version and history, with the cached banner explaining what is stale.
