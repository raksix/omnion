# REQ-044 — Package / App Installer

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (module manager)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

A company says:

> "Install CRM + Helpdesk + Marketing + AI Agent for me."

Omnion:

```text
Resolve Dependencies
       ↓
Install Modules
       ↓
Create Permissions
       ↓
Create Roles
       ↓
Run Migrations
       ↓
Enable Workflows
       ↓
Ready
```

## Notes

- The dependency resolution semantics were specified in docs/05-VERSIONING.md §10; this is
  the one-click UX on top. Bigger sibling: App Marketplace (REQ-048).

## Implementation spec

### Scope (in / out)

**In**

- Module manager: installed-module registry, installable catalogue and curated bundles (`crm-suite`,
  `support-suite`, `marketing-suite`, `tr-accounting` — the last labelled "Türkiye muhasebe paketi"
  as a user-facing example) that name their modules and versions.
- Two-phase install: a **plan** call resolves dependencies against SemVer ranges into an ordered step
  list and diffs it against what is installed; an **install** call executes exactly that plan, step by
  step, with progress streamed over SSE (REQ-041) and a persisted step log.
- Fixed pipeline order: resolve → download/verify → enable module code → migrations (each reversible)
  → permissions → roles → menus/settings → workflows and automations → health check.
- Idempotent execution via a client idempotency key, cancel between steps, and rollback of a failed run
  to its last checkpoint (migrations reversed where the module declared them reversible).
- Lifecycle actions: enable, disable, update (equal or newer only), soft uninstall (data kept) and a
  separately confirmed hard purge; run history with the frozen plan and rollback record.

**Out**

- Purchasing, licensing or payment for third-party packages (REQ-048 marketplace).
- Downloading from a remote registry; version downgrades; per-organization enablement; manifest editing.

### Screens (UI)

Admin routes:

- `/modules` — installed-module table. Columns: Module (name + slug), Version, Status (enabled /
  disabled / update available), Kind (core / platform / business), Depends on (chips, `+N`), Dependents,
  Installed at, Updated at. Filters: status, kind, "update available", text search. Bulk: Enable,
  Disable, Update, Export manifest, Uninstall (soft). Header: **Install modules**, **Check for updates**.
- `/modules/install` — three-step wizard:
  1. **Choose** — searchable catalogue grid (name, description, kind badge, version, installed marker),
     category chips and bundle cards ("Support suite — Helpdesk + Notifications + AI copilot preset");
     sticky footer shows "n selected" and **Review plan**.
  2. **Review plan** — counts (to install / already installed / update / conflict), ordered step list,
     permission diff grouped by module, roles created or modified, migration and workflow counts,
     warnings (deprecated, pinned). A conflict blocks the step and names both incompatible ranges with
     the closest compatible version offered.
  3. **Run** — live step list (queued / running / done / failed / skipped with durations), streaming log
     pane with copy and export, per-step progress, **Cancel** (blocked mid-migration with the reason
     shown) and **Roll back** on failure; success shows a "Ready" summary with links to the new screens.
- `/modules/{slug}` — manifest (description, version, kind, homepage), contributed permissions, roles,
  menus, workflows, migrations with applied timestamps, dependents, install log, lifecycle actions.
- `/modules/installations` — history table: Run (short id), Action (install / update / uninstall /
  rollback), Packages (chips), Requested by, Dry run badge, Status, Duration, Started at. A row opens
  `/modules/installations/{id}` with the frozen plan, step log, actor and error detail.
- States: `/modules` empty → "No modules installed yet" with the wizard CTA; loading → skeleton rows;
  plan error → banner with retry; run failure → red step, error text, rollback button; missing
  `modules.manage` → read-only table with actions hidden.
- Keyboard: `/` search, `j`/`k` rows, `Space` select, `Enter` open, `Cmd/Ctrl+Enter` advance the wizard,
  `Esc` cancel with a confirm. Mobile: tables become status-chip cards, wizard steps collapse to an
  accordion, the review summary sticks to the bottom, the log pane becomes a full-width sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/modules` | Installed modules with versions and status | `modules.read` |
| GET | `/api/v1/modules/catalogue` | Installable modules and bundles | `modules.read` |
| GET | `/api/v1/modules/{slug}` | Module detail (manifest, contributed artifacts) | `modules.read` |
| POST | `/api/v1/modules/plan` | Resolve the selection into an ordered plan (no writes) | `modules.manage` |
| POST | `/api/v1/modules/install` | Execute a plan (`dry_run=true` for a preview run) | `modules.manage` |
| GET | `/api/v1/modules/installations` | Run history | `modules.read` |
| GET | `/api/v1/modules/installations/{id}` | Frozen plan and step log | `modules.read` |
| GET | `/api/v1/modules/installations/{id}/stream` | SSE progress for a running install | `modules.read` |
| POST | `/api/v1/modules/installations/{id}/cancel` | Cancel between steps | `modules.manage` |
| POST | `/api/v1/modules/installations/{id}/rollback` | Roll back a failed or cancelled run | `modules.manage` |
| POST | `/api/v1/modules/{slug}/enable` | Enable a module (`/disable` for the reverse) | `modules.manage` |
| POST | `/api/v1/modules/{slug}/update` | Update to the target version | `modules.manage` |
| DELETE | `/api/v1/modules/{slug}` | Soft uninstall (`?purge=true` for the destructive variant) | `modules.manage` |

One run at a time per installation: a second request returns `409` with the running run id; `plan` and
`install` answer `422` with the conflict list when resolution fails.

### Data model

Migration `database/migrations/0014_package_installer.sql` (next free number at build time).

- `modules` — `id uuid pk`, `slug text not null`, `name`, `version text not null`, `kind text not null
  default 'business' check (kind in ('core','platform','business'))`, `enabled boolean not null default
  false`, `manifest jsonb not null default '{}'::jsonb`, `installed_at timestamptz not null default
  now()`, `updated_at timestamptz`; unique `(slug)`.
- `module_dependencies` — `module_id uuid references modules(id) on delete cascade`, `depends_on_slug
  text not null`, `version_range text not null`, primary key `(module_id, depends_on_slug)`.
- `module_installations` — `id uuid pk`, `organization_id uuid null references organizations(id)`,
  `requested_by uuid references users(id) on delete set null`, `action text not null check (action in
  ('install','update','uninstall','rollback'))`, `status text not null default 'planning' check (status
  in ('planning','running','completed','failed','cancelled','rolled_back'))`, `dry_run boolean not null
  default false`, `plan jsonb not null default '{}'::jsonb`, `idempotency_key text`, `error text`,
  `started_at timestamptz not null default now()`, `finished_at timestamptz`.
- `module_installation_steps` — `id bigint identity pk`, `installation_id uuid not null references
  module_installations(id) on delete cascade`, `ordinal int not null`, `kind text not null check (kind
  in ('resolve','download','enable','migrate','permissions','roles','menus','workflows','verify'))`,
  `label text not null`, `status text not null default 'queued' check (status in
  ('queued','running','done','failed','skipped'))`, `detail jsonb not null default '{}'::jsonb`,
  `started_at`, `finished_at`.
- Indexes: unique `module_installations_idem_idx (requested_by, idempotency_key)`,
  `module_installations_status_idx (status, started_at desc)`, unique
  `module_installation_steps_order_idx (installation_id, ordinal)`.
- The migration seeds the core and platform modules shipped by the distribution as installed+enabled.

### Events

- Emitted: `module.install.started`, `module.install.completed`, `module.install.failed`,
  `module.enabled`, `module.disabled`, `module.rollback.completed`.
- Consumed: none — the installer only publishes; notifications (REQ-021) turn `module.install.failed`
  into an admin alert.
- Webhook relevance: high for operations pipelines; payloads carry slugs, versions and step counts,
  never migration SQL or file contents.

### Acceptance criteria

- [ ] `/modules` lists installed modules with version, status and dependency chips.
- [ ] The wizard resolves "CRM + Helpdesk + Marketing + AI Agent" into a dependency-ordered plan.
- [ ] The plan preview shows to-install / already-installed / update / conflict counts.
- [ ] An unsatisfiable range blocks step 2 with both conflicting requirements named.
- [ ] A dry run writes nothing yet returns the full plan.
- [ ] Running the plan executes steps in order and the SSE log streams each transition.
- [ ] A completed install adds the module's permissions to the IAM catalogue and seeds its default roles.
- [ ] Menus for newly enabled modules appear in the navigation without a redeploy.
- [ ] Workflows shipped by the module are enabled and visible in `/workflows`.
- [ ] Cancel stops between steps and marks the remaining steps `skipped`.
- [ ] A failing step marks the run `failed`, names the step and offers rollback.
- [ ] Rollback reverses reversible migrations, leaves the module disabled, and records the run.
- [ ] Re-sending an idempotency key returns the original run; a concurrent install returns `409`.
- [ ] Disable hides menus and blocks module APIs without dropping data.
- [ ] Soft uninstall keeps rows; hard purge requires typing the slug and is audited.
- [ ] History lists every run with actor, duration and outcome; detail shows the frozen plan.
- [ ] Permission keys `modules.read` / `modules.manage` exist in the catalogue.
- [ ] `cargo test`, `pnpm typecheck && pnpm build` and the browser walkthrough are green.

### QA plan

- Walkthrough: `/modules` (seeded core modules) → **Install modules** → select CRM + Helpdesk +
  Marketing + AI Agent → review the plan (counts and step order) → dry run and confirm nothing changed
  → real install with the step list and log streaming → "Ready" summary → verify the new nav entries,
  `/workflows` entries and IAM permissions → inspect the run in `/modules/installations` → install a
  conflicting pair and confirm the blocking panel → cancel a run and roll it back.
- Visual check: the stepper marks the active step, running steps animate, failed steps are red with
  readable errors, the log pane does not overflow, the review summary is scannable at 1440 px, and
  mobile cards expose the primary action in one tap.
- Regression: `/pages`, `/media`, `/sites`, `/ai`, `/workflows` still load after install cycles.

### Slices

1. **Registry + resolver** — migration `0014_package_installer.sql`, module and dependency tables, seed,
   list/catalogue/detail endpoints, permission keys, resolver unit tests. **Done when:** the resolver
   returns a deterministic ordered plan and rejects an unsatisfiable set in tests.
2. **Wizard UI** — `/modules` table, three-step wizard, plan review, conflict panel, dry-run and
   keyboard/mobile behaviour. **Done when:** a plan is produced and reviewed from the UI with no writes.
3. **Execution + progress** — step runner (migrations, permissions, roles, menus, workflows), SSE
   progress, cancel, rollback, history screens. **Done when:** a real install completes end to end on the
   QA database and a failed run rolls back cleanly.
4. **Lifecycle** — enable/disable, update, soft uninstall, hard purge, per-module detail, audit entries.
   **Done when:** every lifecycle action changes panel behaviour and appears in history.

### Risks / notes

- Migrations are the irreversible part: refuse a plan containing a non-reversible migration unless the
  operator explicitly acknowledges it.
- Rollback never deletes customer data — only schema and registrations created inside this run, guarded
  by the frozen plan snapshot; freeze that snapshot at execution start.
- Concurrent installs racing on permissions and roles need real locking; the `409` guard alone is not
  enough, and uninstall must refuse with the dependent list.
