# REQ-078 — System Updates Center

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Updating the platform without fear.

- Update manager screen: available version, changelog, size, requirements.
- Migration-required warning and a pre-update backup indicator (with "take backup" button).
- Plugin/theme compatibility check before updating (which ones would break).
- One-click update with progress, plus rollback to the previous version.
- Update history with outcomes and diffs of what changed.

## Implementation spec

### Scope (in / out)

**In**
- The Updates centre in the panel (`/updates`) as the single answer to "what can be updated, is it safe, and what happened last time": a platform card, an extension list (plugins, theme packages), a policy tab and a history tab.
- Update checks: a scheduled check (channel stable / beta, plus a manual `Check now`) that resolves the newest available version for the platform and for every installed plugin and theme. One job per run with per-item results; a failing source never blanks the screen, it marks that item `unknown`.
- Platform update detail: installed → available version, release date and channel, changelog rendered from the release notes, download size, and the requirements block — minimum platform version, runtime and database floors, storage headroom, and the migration list with a count and a per-migration summary. The screen states plainly whether migrations are required, whether downtime is expected and how long the last comparable run took.
- Gates before a platform update, each a checklist row with a pass/warn/fail state and an action: `Backup fresh enough` (REQ-013, with `Take backup now`), `Extensions compatible`, `Maintenance mode planned` (REQ-024's window), `Typed confirmation` (the target version typed exactly), `Storage and database headroom`, `Health checks reachable`.
- Compatibility check (the "which ones would break" question): for a platform target version every installed plugin and theme is evaluated against its declared platform requirement range and the target's breaking-change list, naming the item, its installed version, the required range, the verdict (`compatible` / `compatible_with_warnings` / `incompatible` / `unknown`) and, when a fix exists, the version that would resolve it with a path into the extension update. Generated on demand (`Regenerate`) and stored as evidence on the run.
- Extension updates: pending plugin and theme updates with per-item and bulk `Update`, a per-item detail (installed → available, changelog, size, verdict), `Hold this version` (skip until lifted), and `Update all compatible` which applies only to items not marked `incompatible`.
- One-click update with progress: the platform path creates a REQ-024 deployment run (backup → download → verify checksum → maintenance → migrate → deploy → verify → resume) and this screen renders its step timeline, live log, elapsed time, cancel before the migrate step, and a result banner; the extension path runs through the package installer (REQ-044) and the theme install path (REQ-062) with per-item results.
- Rollback and history: `Roll back to <version>` for the platform (delegating to REQ-024's rollback, requiring a reason) and per extension (allowed only while the previous version is within the platform's supported range, otherwise refused with the reason); history lists every run with actor, scope, from → to, duration, outcome, the compatibility report in force, the backup used, steps, log link and a change diff — migrations applied, extension versions moved, configuration keys touched.
- Manual/offline path and policy: update from a package file (`/updates/upload`) with the same validation-report style as theme upload (REQ-036 relies on it), plus channel, check schedule, auto-backup before updates, whether an update may run unattended (off by default) and the dashboard banner behaviour; pending updates also ride the notification centre (REQ-021). A caller with `updates.read` but no action keys sees versions, gates, report and history with actions disabled and the required permission named on each.

**Out**
- The deployment engine itself — jobs, step definitions, live log transport, pre-flight, maintenance windows, environment cards, the Kubernetes panel — all belong to REQ-024. This screen orchestrates and explains; it never re-implements a deploy.
- Installing a *new* plugin or theme and removing one — REQ-044 and REQ-062. Backup creation, verification and restore mechanics — REQ-013; here they are a gate and a link.
- Update *sources* and marketplace metadata — REQ-048/REQ-023 own the catalogues; rolling back content or configuration (REQ-112, REQ-077) and reverting a schema migration by hand are out too, since a rollback restores a released version and never rewrites migrations in place. Auto-updating extensions on a schedule in the background also waits for a later wave: the policy exposes the flag, the runner lands once the gates and the run record have proven themselves.

### Screens (UI)

| Route | Screen |
|---|---|
| `/updates` | Overview: platform card, pending extension updates, last check, last backup, policy summary |
| `/updates/platform` · `/updates/platform/run/<id>` | Platform detail (changelog, requirements, gates, `Update now`) · live run (timeline, log, cancel, result) |
| `/updates/extensions` · `/updates/extensions/<kind>/<key>` | Pending updates with verdicts and bulk actions · one extension (changelog, compatibility, update / roll back / unhold) |
| `/updates/compat` | Compatibility report for a target version, with the fix path per item |
| `/updates/history` · `/updates/history/<id>` | Run history · one run with steps, evidence and the change diff |
| `/updates/policy` | Channel, schedule, auto-backup, banner behaviour |
| `/updates/upload` | Update from a package file (offline path) with a validation report |

- **Overview.** Platform card: installed version with build metadata, "+2 patch releases available" or `Up to date`, release date, size, a one-line summary of the headline change, and the primary action (`Review update`, or `Check now` when the last check is older than the schedule). Extension card: `5 updates available · 1 incompatible with 3.0.0` with `Review`. Status strip: last check (time, source, result), last backup (age, size, destination health), maintenance state, last run outcome with a link.
- **Gates checklist.** Each row: state icon, title, one-line explanation and the action (`Take backup now`, `Regenerate report`, `Schedule window`, `Fix 1 incompatible extension`, `View pre-flight`). `Update now` stays disabled while any row is `fail`, with one sentence naming the first blocker; `warn` rows are acknowledgable with a checkbox recorded on the run. The typed confirmation accepts the target version exactly and shows it as a copyable chip.
- **Run screen.** Step timeline with per-step duration and status, log pane (auto-scroll, level filter, search, copyable lines), cancel until the migrate step starts, a "what happens if I close this tab" note, and a result banner: success with the new version plus a `Verify` checklist (health endpoint, version endpoint, homepage render, extension smoke list), or failure with the failing step, a log excerpt and the two next actions (`Roll back to <version>`, `Open the log`).
- **Compatibility report and extension list.** Report: item (name, kind, installed version), required range, verdict, fix (`Update to 2.4.1` or `Latest release is still incompatible — contact the author`), a `Why` tooltip, summary chips (N compatible / with warnings / incompatible / unknown) and `Export report` for a change-approval record (REQ-059). Extension list: name, kind, installed → available, size, verdict, `Held` badge, last updated, actions (`Update`, `Hold`, `Details`); bulk `Update selected` runs sequentially with per-item results and keeps the previous version on failure.
- **States, keyboard and mobile.** No updates: last check time, channel and `Check now`. Check running: per-item progress with cancel. Check failed: the error and source with a retry while the last known state stays visible. Offline policy: `Updates disabled by policy` with the upload path emphasised. Permission-limited: read-only. A run already in progress: a link to it instead of a second run (`409`). Keyboard: `g u` updates, `g h` history, `c` check now, `⌘/` shortcut sheet, `Esc` closes dialogs. Below 900 px the overview stacks, the gates become an accordion with the blocker first, the run keeps a scrollable log with a sticky cancel, and the compatibility table becomes a card list.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/updates` | Overview: platform state, extension count, last check, last backup, policy summary | `updates.read` |
| POST · GET | `/api/v1/updates/check` · `/api/v1/updates/checks/{id}` | Run an update check now (async) · check status with per-item results | `updates.manage` · `updates.read` |
| GET | `/api/v1/updates/candidates` | Pending updates (`kind=platform\|plugin\|theme`, `include_held`) | `updates.read` |
| GET | `/api/v1/updates/platform` | Platform candidate: changelog, size, requirements, migrations | `updates.read` |
| POST | `/api/v1/updates/platform/preflight` | Gates evaluation for a target version (`target`) with the checklist | `updates.read` |
| POST | `/api/v1/updates/platform/apply` | Start the platform update (`target`, `confirm_version`, `backup_id` or `take_backup`, `ack_warnings[]`) — creates the REQ-024 run | `deployment.deploy` (+ `backup.create` when taking one) |
| POST | `/api/v1/updates/platform/rollback` | Roll back to a previous version (`to_version`, `reason`) | `deployment.rollback` |
| GET | `/api/v1/updates/compat` | Compatibility report for a target (`target`) | `updates.read` |
| POST | `/api/v1/updates/compat/refresh` | Regenerate the report | `updates.manage` |
| GET | `/api/v1/updates/extensions` · `/{kind}/{key}` | Pending extension updates with verdicts · one extension (versions, changelog, compatibility) | `updates.read` |
| POST | `/api/v1/updates/extensions/apply` | Update one or many (`items[]`, `backup_first`) — per item behind its own guard | `plugins.update` · `themes.update` |
| POST | `/api/v1/updates/extensions/{kind}/{key}/rollback` | Restore the previous version | `plugins.update` · `themes.update` |
| PUT · DELETE | `/api/v1/updates/extensions/{kind}/{key}/hold` | Hold a version · lift the hold | `plugins.update` |
| POST | `/api/v1/updates/upload` | Update from an uploaded package (offline path, multipart) with a validation report | `plugins.install` · `themes.install` |
| GET | `/api/v1/updates/history` · `/{id}` | Run list · one run with steps, evidence and the change diff | `updates.read` |
| GET | `/api/v1/updates/history/{id}/log` | Log stream (SSE, cursor fallback for polling) — REQ-024 job log | `updates.read` |
| GET · PUT | `/api/v1/updates/policy` | Read · save channel, schedule, auto-backup, banner behaviour | `updates.read` · `updates.manage` |
| GET | `/api/v1/updates/backup-status` | Freshness of the newest verified backup against the gate threshold | `backup.read` |
| POST | `/api/v1/backups` | `Take backup now` — REQ-013 route | `backup.create` |
| GET · PUT | `/api/v1/deployment/maintenance` | Maintenance window state and settings — REQ-024 route | `deployment.read` · `deployment.maintenance` |

The centre never widens an action's permission: it re-uses the owning guard (`deployment.deploy`, `deployment.rollback`, `backup.create`, `plugins.update`, `themes.update`, `plugins.install`, `themes.install`) and adds only its own read/write keys. Error codes: `update_blocked_backup_stale`, `update_blocked_incompatible`, `update_blocked_maintenance_conflict`, `update_confirm_version_mismatch`, `update_run_in_progress`, `update_target_unknown`, `extension_hold_active`, `rollback_target_unsupported`.

### Data model

Migrations: `0119_system_updates.sql`, `0120_update_history.sql` (reserved band 0116–0127; append-only ledger — take the next free number if taken).

```sql
-- 0119_system_updates.sql
update_candidates (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid null references organizations (id) on delete cascade,   -- null = installation-wide
  kind text not null, key text not null, channel text not null default 'stable',
  installed_version text null, available_version text not null,
  release_notes text null, release_url text null, size_bytes bigint null, checksum text null,
  requirements jsonb not null default '{}',         -- min platform/runtime/db, storage headroom
  migrations jsonb not null default '[]',           -- [{name, summary, destructive}]
  compatibility text not null default 'unknown',
  held boolean not null default false, held_by uuid null references users (id) on delete set null,
  released_at timestamptz null, first_seen_at timestamptz not null default now(),
  last_seen_at timestamptz not null default now(), dismissed_at timestamptz null,
  constraint update_candidates_kind_check check (kind in ('platform','plugin','theme')),
  constraint update_candidates_compat_check check (compatibility in ('compatible','compatible_with_warnings','incompatible','unknown')),
  constraint update_candidates_key unique (organization_id, kind, key, channel, available_version)
);
create index update_candidates_pending_idx on update_candidates (organization_id, kind, available_version) where dismissed_at is null and held = false;

update_compat_reports (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid null references organizations (id) on delete cascade,
  target_version text not null, platform_from_version text not null,
  generated_at timestamptz not null default now(), generated_by uuid null references users (id) on delete set null,
  summary jsonb not null default '{}', items jsonb not null default '[]', blocking boolean not null default false
);
create index update_compat_reports_target_idx on update_compat_reports (organization_id, target_version, generated_at desc);

-- 0120_update_history.sql
update_runs (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid null references organizations (id) on delete cascade,
  scope text not null, target_version text not null, from_version text null,
  status text not null default 'queued', steps jsonb not null default '[]',
  deploy_job_id uuid null,                          -- REQ-024 job (soft reference)
  backup_id uuid null references backups (id) on delete set null,
  compat_report_id uuid null references update_compat_reports (id) on delete set null,
  items jsonb not null default '[]',                -- extension results for scope = 'extensions'
  change_diff jsonb not null default '{}',          -- migrations, versions, config keys, files
  acknowledged jsonb not null default '[]', error text null,
  started_by uuid null references users (id) on delete set null,
  started_at timestamptz not null default now(), finished_at timestamptz null,
  constraint update_runs_scope_check check (scope in ('platform','extensions')),
  constraint update_runs_status_check check (status in ('queued','running','succeeded','failed','rolled_back','cancelled'))
);
create index update_runs_org_started_idx on update_runs (organization_id, started_at desc);
create index update_runs_active_idx on update_runs (status) where status in ('queued','running');

update_policy (
  organization_id uuid primary key references organizations (id) on delete cascade,
  channel text not null default 'stable', check_schedule text not null default 'daily',
  auto_backup boolean not null default true, backup_retention_days integer not null default 14,
  require_compat_ack boolean not null default true, auto_apply_extensions boolean not null default false,
  maintenance_message text null, updated_by uuid null references users (id) on delete set null,
  updated_at timestamptz not null default now(),
  constraint update_policy_channel_check check (channel in ('stable','beta')),
  constraint update_policy_message_length check (maintenance_message is null or length(maintenance_message) <= 200)
);
```

`deploy_job_id` is a soft reference to the deployment centre's job table on purpose: the run row is this screen's record (evidence, gates, diff), the job is the engine's. Candidate rows are disposable project data — deleting them and re-checking rebuilds the same view. New permission keys in `crates/permissions/src/catalogue.rs`, category `updates`: `updates.read`, `updates.manage`; every action key it re-uses already exists or arrives with its own request.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `updates.check_completed` | A check run finished | `checked_at`, `platform_candidate`, `plugin_count`, `theme_count`, `errors[]` |
| `updates.available` | A new candidate appeared (or a held one was unheld) | `kind`, `key`, `installed_version`, `available_version`, `channel` |
| `updates.compat_report_ready` | A report generated | `target_version`, `summary`, `blocking` |
| `updates.run_started` | A platform or extension update began | `run_id`, `scope`, `from_version`, `to_version` |
| `updates.run_succeeded` · `.failed` | Run outcome | `run_id`, `scope`, `to_version`, `duration_ms`, `error` |
| `updates.rolled_back` · `updates.policy_changed` | Rollback completed · policy saved | `run_id`, `to_version`, `reason` · `channel`, `check_schedule`, `auto_backup` |

Consumed: `deployment.job.started` · `.succeeded` · `.failed` · `.cancelled` (REQ-024 — mirror runtime states into `update_runs` and the live timeline), `backups.created` (clear the `Backup fresh enough` gate once the new backup verifies), `plugins.updated` and `themes.package.installed` (retire candidates, refresh the list), `system.health.degraded` (surface a warning row before suggesting an update), `content.page.published` (post-update verification checklist). Webhook relevance: `updates.available` and `updates.run_failed` are useful outbound signals for an installation's own monitoring; `updates.check_completed` is deliberately not webhooked (a daily scheduled check is noise).

### Acceptance criteria

- [ ] `/updates` shows the installed version with build metadata, the latest available version from the last check, the last check time and the last backup age for the seeded installation.
- [ ] `Check now` runs asynchronously, shows per-item progress and records every installed plugin and theme; a source failure leaves other results intact and marks only that item `unknown`.
- [ ] The platform detail renders the changelog, size and requirements, and states whether migrations are required with their count and any destructive flag.
- [ ] Every gate row reflects real state: a stale backup shows `fail` with `Take backup now`, and taking a backup flips the row to `pass` without a page reload.
- [ ] `Update now` is disabled while a `fail` gate exists and the message names the first blocker; `warn` gates require an acknowledgement stored on the run.
- [ ] A wrong typed confirmation returns `update_confirm_version_mismatch` and starts nothing; the correct one starts exactly one run.
- [ ] Starting a second run while one is active returns `409 update_run_in_progress`, and the UI links to the running one instead of duplicating.
- [ ] The compatibility report lists every installed plugin and theme with installed version, required range, verdict and fix, and one incompatible item marks the report blocking.
- [ ] `Update all compatible` skips every `incompatible` item; each applied item reports its own result and keeps the previous version on failure.
- [ ] `Hold` removes an extension from the pending list and from `Update all` until unheld, while the held version stays visible in its detail.
- [ ] The run screen streams steps and log lines; cancel works before the migrate step and is refused with the reason after it starts.
- [ ] Rollback restores the previous version, records the reason, writes a run of its own, and refuses a target outside the supported range with `rollback_target_unsupported`.
- [ ] History lists every run with actor, scope, from → to, duration, outcome, the backup used and the compatibility snapshot in force, and the detail exposes the change diff (migrations, extension versions, configuration keys).
- [ ] The offline path accepts a package with a validation report, and an installation with the policy set to offline shows checks disabled while the upload path stays available.
- [ ] A read-only caller sees versions, gates, report and history with actions disabled and the required permission named on each.
- [ ] The centre never performs an update the owning engine would refuse: calling the action endpoints directly with a read-only key is rejected by the same guards.
- [ ] `updates.available`, `updates.run_started`, `updates.run_succeeded` and `updates.policy_changed` reach a subscribed endpoint with the documented payloads.
- [ ] The overview, gates checklist and compatibility table are usable at 390 px, and the walkthrough reports zero high findings on the new routes.

### QA plan

Add `/updates`, `/updates/platform`, `/updates/extensions`, `/updates/compat`, `/updates/history` and `/updates/policy` to the `routes` array in `scripts/qa/walkthrough.cjs`, and enter the centre from the sidebar. The harness never performs a real platform upgrade: it points the update check at a fixture feed served by the QA script (a newer platform version plus two extension versions, one deliberately incompatible with it) and exercises only the guarded paths. The walkthrough must: open `/updates` and read both cards; run `Check now` against the fixture feed and see the candidates appear; open `/updates/platform` and read the changelog, requirements and migration warning; read the gates checklist, confirm the stale-backup `fail`, take a backup through `Take backup now` and see the row flip; open `/updates/compat`, regenerate the report, see the incompatible item and follow its fix link; attempt `Update now` with a wrong typed confirmation (expect the refusal) and confirm nothing started; update the compatible extension from a local fixture package and read the per-item result; hold and unhold the second extension; start a platform run and cancel it before the migrate step, then open the history entry and read its evidence and diff; and open `/updates/policy`, change the channel and schedule and save. Visual check: the overview states versions and backup age without ambiguity, the gates checklist makes its blocker obvious, compatibility verdicts are distinguishable without relying on colour alone, the run timeline shows real steps with durations, the history table shows one row per run with an honest outcome, and no screen presents a raw log or JSON blob as its primary content. The mobile pass must show the stacked overview, the accordion gates and a scrollable log.

### Slices

1. **Candidates, checks and the overview.** Migrations `0119_system_updates.sql` and `0120_update_history.sql`, the check job with fixture-friendly sources, candidate storage with hold/dismiss, the overview and extension list, and the platform detail with changelog, requirements and migration warning. *Done when:* acceptance 1–3, 10 pass and `/updates` is in the walkthrough inventory.
2. **Gates, compatibility and the platform run.** Pre-flight evaluation, the gates checklist with `Take backup now`, the compatibility report with its fix path, typed confirmation, the run over REQ-024 with the live step timeline and cancel, and the rollback action. *Done when:* acceptance 4–9, 11–12 pass.
3. **History, evidence, policy and the offline path.** Run history list and detail with the backup, compatibility snapshot, steps, log link and change diff; extension per-item results and rollbacks; the policy screen; the offline upload path; read-only rendering, mobile behaviour, and the events verified against a subscribed endpoint. *Done when:* acceptance 13–18 pass with a green walkthrough and no high findings.

### Risks / notes

- The most dangerous button in the panel lives here, so every dangerous action is gated by evidence and not by a dialog alone: backup freshness, compatibility verdict, typed version confirmation, a recorded acknowledgement. Removing a gate to reduce friction is the change that turns one bad release into an outage with no way back.
- This screen must stay a coordinator: if it starts downloading, extracting and migrating on its own, the codebase has two deploy engines that will disagree about maintenance mode and rollback. The pipeline belongs to REQ-024, package handling to REQ-044/REQ-062.
- Rollback is only real if the previous version and its schema are still supported: a rollback after a destructive migration is refused up front with the reason, destructive migrations carry a flag in the release metadata, and the run records that the rollback path was knowingly given up.
- Extension compatibility data is frequently missing or wrong; the report must be honest about `unknown` and never guess `compatible`, which is why the bulk action skips anything not positively compatible.
- A stale or unverified backup is worse than none because it looks like safety: `backup-status` reads REQ-013's verification state, not just the row's existence, and the gate fails when the newest verified backup is older than the policy threshold.
- Update checks make outbound calls that some installations forbid: the source is a plugin point, the offline path is first-class, and a failed check never leaves the screen implying "up to date".
- Runs must be observable after the fact — log lines, the compatibility snapshot and the diff are stored with the run rather than recomputed against a changed world later, and timestamps render in the installation timezone with the viewer's locale.
