# REQ-034 — Sandbox Environments

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (org-scoped environments)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Every organization can create a sandbox:

```text
Production
    │
    └── Create Sandbox
              ↓
          Sandbox Copy
```

There they can test:

- plugins
- themes
- workflows
- AI agents
- database changes

Then:

**Promote to Production**

## Notes

- Extends the staging flow of REQ-017; promotion uses the same-artifact principle from
  docs/05-VERSIONING.md §17.

## Implementation spec

### Scope (in / out)

In:

- Organization-scoped environments: exactly one `production` plus up to N sandboxes (default 3), each with an isolated PostgreSQL schema, a separate object-storage prefix, a separate cache key namespace and its own admin host (`{slug}.sandbox.{base-domain}`).
- Sandbox creation wizard: clone mode (`structure`, `content`, `content + media references`), entity selection with row caps, and a masking profile applied during the copy.
- Refresh (re-clone selected entities from production while keeping sandbox-only artifacts) and reset (drop and recreate at the chosen clone mode).
- Testing inside the sandbox for plugins, themes, workflows, AI agents and database changes, including pending-migration preview (statements plus dry-run stats) and apply-inside-sandbox.
- Promotion to production as a reviewable plan (artifact manifest, migration plan, settings diff), with approval, execution and rollback of the latest successful promotion.
- Drift visibility: which artifacts differ from production and by how much.
- Automatic expiry (TTL) with a T-48h warning and a grace period, plus manual extension.
- Audit and events across the whole lifecycle.

Out:

- Sandboxes that write to production data — every sandbox works on a copy; there is no write-through.
- More than one production environment per organization (multi-region production is REQ-035).
- Anonymous public previews of sandbox content (REQ-018) — sandbox hosts are authenticated.
- Per-developer ephemeral branches and per-pull-request environments.
- Copying binary media into sandboxes: media rows are copied as references to the production objects; uploads made inside the sandbox stay in the sandbox prefix.
- Continuous replication — clones are point-in-time snapshots, not streams.

### Screens (UI)

Routes (`apps/admin/app/environments/*`, feature dir `apps/admin/features/environments/`):

```text
/environments                    ← list: production card + sandboxes
/environments/new                ← create-sandbox wizard
/environments/{id}               ← detail: Overview | Data | Migrations | Artifacts | Activity
/environments/{id}/migrations    ← migration preview and apply
/environments/{id}/promote       ← promotion review
/environments/{id}/settings      ← name, TTL, masking profile, delete
/promotions/{id}                 ← promotion status, approvals, step log, rollback
```

- Global environment banner: while the session is on a sandbox, a persistent amber bar shows the sandbox name and an `Exit to production` link, and mutating buttons carry a sandbox marker so a test action can never be mistaken for a production one.
- List: production card (status, region note, current artifact versions) above a sandbox table with columns Name, URL, Status, Clone mode, Created, Expires (with days remaining), Size (rows), Drift (artifact count), Created by. Filters: status, expiring soon, creator, clone mode. Bulk: extend TTL, refresh, delete (typed confirmation per name).
- Create wizard — Basics: slug-validated name, TTL (7 / 14 / 30 days); Data: clone mode radio, entity multi-select with row caps and an estimated size, masking profile select with a field preview; Confirm: summary with space estimate and TTL plus an explicit notice that the copy contains masked customer data. `structure` mode skips the Data step.
- Detail → Overview: status, URL with copy button, created/expires, resources (schema name, storage prefix label, cache namespace), and quick actions Refresh, Reset, Extend, Delete, Promote, plus a "what changed since clone" summary card.
- Detail → Data: per-entity table (Entity, Production rows, Sandbox rows, Clone mode, Cap applied, Last synced) with a `Refresh entities` selection; columns containing personal data carry a mask badge.
- Detail → Migrations: pending migrations (version, name, applied in sandbox?, applied in production?), selecting one shows statements and dry-run results (tables and rows affected, expected locks), then `Apply in sandbox` and `Queue for promotion`.
- Detail → Artifacts: artifacts table (Kind, Ref, Sandbox version, Production version, Status: same / ahead / behind / conflict) with `Promote` checkboxes; conflicts block promotion with an inline explanation.
- Detail → Activity: lifecycle events and audit entries with actor and timestamp.
- Promote: two-pane review — left, the plan grouped by kind (Migrations, Plugins, Themes, Workflows, AI agents, Settings) with per-item version transitions and risk labels (schema change, breaking theme, permission change); right, a required checklist (drift acknowledged, migration dry-run passed, rollback point noted), a maintenance-window select and a typed confirmation of the target name. Conflicting items are deselected and marked `Resolve first`.
- Promotion detail: status timeline, approver panel, step log with durations, and `Rollback` enabled only for the most recent successful promotion.
- Form validation: slug-safe names 3–40 chars, unique per organization; TTL bounds 1–90 days; at least one entity when the mode includes content; the masking profile must cover every field flagged as personal data or the wizard blocks with a link to edit the profile.
- Empty state: no sandboxes → illustrated panel with `Create sandbox` and a three-line explainer. Loading: clone progress with live row counts and a `Cancel` that cleans up. Error: a failure card naming the failed step with retry; the environment stays `failed` until deleted.
- Keyboard: `Ctrl+K` covers environment navigation and Promote/Refresh (permission-checked); `g e` environments, `g m` migrations for the current environment; in the review pane `j`/`k` move between items, `x` toggles, `Enter` opens detail.
- Mobile: production card and sandbox rows become cards; the promotion review collapses to one column (plan, then checklist) with a sticky footer; typed confirmations get a copy-the-name helper so they stay usable on a touch keyboard.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/environments` | List production and sandboxes with counts | `platform.environments.read` |
| POST | `/api/v1/environments` | Create a sandbox (clone mode, entities, masking, TTL) | `platform.environments.create` |
| GET | `/api/v1/environments/{id}` | Detail: status, resources, expiry, artifact summary | `platform.environments.read` |
| PATCH | `/api/v1/environments/{id}` | Rename, change TTL, change masking profile | `platform.environments.manage` |
| POST | `/api/v1/environments/{id}/refresh` | Re-clone selected entities from production | `platform.environments.manage` |
| POST | `/api/v1/environments/{id}/reset` | Drop and recreate at the chosen clone mode | `platform.environments.manage` |
| POST | `/api/v1/environments/{id}/extend` | Extend TTL within bounds | `platform.environments.manage` |
| DELETE | `/api/v1/environments/{id}` | Delete the sandbox with its schema and storage prefix | `platform.environments.delete` |
| GET | `/api/v1/environments/{id}/data` | Per-entity copy status and sizes | `platform.environments.read` |
| GET | `/api/v1/environments/{id}/migrations` | Pending and applied migrations for the sandbox | `platform.environments.read` |
| POST | `/api/v1/environments/{id}/migrations/{version}/preview` | Statement list plus dry-run stats | `platform.environments.manage` |
| POST | `/api/v1/environments/{id}/migrations/{version}/apply` | Apply inside the sandbox only | `platform.environments.manage` |
| GET | `/api/v1/environments/{id}/artifacts` | Artifact diff against production | `platform.environments.read` |
| POST | `/api/v1/promotions` | Create a promotion plan (sandbox → production) | `platform.promotions.create` |
| GET | `/api/v1/promotions/{id}` | Plan, status and step log | `platform.promotions.read` |
| POST | `/api/v1/promotions/{id}/approve` | Approve, recording the approver | `platform.promotions.approve` |
| POST | `/api/v1/promotions/{id}/execute` | Execute an approved plan | `platform.promotions.execute` |
| POST | `/api/v1/promotions/{id}/rollback` | Roll back the latest promotion | `platform.promotions.rollback` |
| POST | `/api/v1/promotions/{id}/cancel` | Cancel before execution | `platform.promotions.create` |

Conventions: the admin session carries its environment context (`X-Omnion-Environment: {id}`) for reads and writes; promotion endpoints always act on production and ignore that header. `execute` requires `Idempotency-Key`.

### Data model

Migration `database/migrations/0014_environments.sql`.

`environments`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `uuid pk` | |
| `organization_id` | `uuid not null` | fk `organizations` |
| `name` | `text not null` | unique per organization |
| `slug` | `text not null` | unique per organization; used in the sandbox host |
| `kind` | `text not null` | check in (`production`,`sandbox`) |
| `status` | `text not null` | check in (`creating`,`active`,`refreshing`,`failed`,`expiring`,`expired`,`deleting`) |
| `database_schema` | `text not null unique` | physical schema name |
| `storage_prefix` | `text not null` | object-storage namespace |
| `cache_namespace` | `text not null` | cache key prefix |
| `base_host` | `text` | sandbox host name |
| `clone_source_id` | `uuid` | fk `environments`, the production row |
| `clone_mode` | `text` | check in (`structure`,`content`,`content_media_refs`) |
| `masking_profile` | `jsonb` | `{entity: {field: strategy}}` |
| `artifact_manifest` | `jsonb` | last known artifact versions in this environment |
| `expires_at` | `timestamptz` | null for production |
| `created_by` | `uuid not null` | fk `users` |
| `created_at` / `updated_at` | `timestamptz not null default now()` | |
| `deleted_at` | `timestamptz` | soft delete until the purge finishes |

Indexes: unique `(organization_id, name)`, unique `(organization_id, slug)`, `(organization_id, kind)`, `(status, expires_at)` for the expiry sweeper.

`environment_clones`: `id uuid pk`, `environment_id uuid not null`, `source_environment_id uuid not null`, `mode text not null`, `status text not null` check in (`queued`,`copying`,`masking`,`succeeded`,`failed`,`cancelled`), `entities jsonb`, `rows_copied bigint not null default 0`, `bytes_copied bigint not null default 0`, `started_at`, `finished_at`, `error text`. Index `(environment_id, started_at desc)`.

`environment_migrations`: `environment_id uuid not null`, `version text not null`, `name text not null`, `status text not null` check in (`pending`,`previewed`,`applied`,`failed`), `preview jsonb`, `applied_at timestamptz`, `error text`, primary key `(environment_id, version)`.

`promotions`: `id uuid pk`, `organization_id uuid not null`, `source_environment_id uuid not null` fk `environments`, `target_environment_id uuid not null` fk `environments`, `title text not null`, `status text not null` check in (`draft`,`pending_approval`,`approved`,`executing`,`succeeded`,`failed`,`cancelled`,`rolled_back`), `migration_plan jsonb`, `artifact_manifest jsonb`, `settings_diff jsonb`, `maintenance_window text`, `approved_by uuid`, `approved_at`, `executed_by uuid`, `executed_at`, `rollback_of uuid` fk `promotions`, `created_by uuid not null`, `created_at`, `error text`. Indexes: `(organization_id, created_at desc)`, `(target_environment_id, status)`.

`promotion_items`: `id bigserial pk`, `promotion_id uuid not null` fk `promotions` on delete cascade, `kind text not null` check in (`migration`,`plugin`,`theme`,`workflow`,`ai_agent`,`setting`), `ref text not null`, `from_version text`, `to_version text`, `risk text` check in (`normal`,`schema_change`,`breaking`,`permission_change`), `status text not null` check in (`pending`,`applied`,`skipped`,`failed`), `error text`. Index `(promotion_id, kind)`.

Isolation is mechanical rather than tabular: each sandbox schema is created from the production migration set, grants are scoped to that schema, and no sandbox application role holds credentials for production data.

### Events

Emitted: `environment.created`, `environment.clone.completed`, `environment.clone.failed`, `environment.refreshed`, `environment.expiring` (T-48h), `environment.expired`, `environment.deleted`, `environment.migration.applied`, `promotion.requested`, `promotion.approved`, `promotion.executed`, `promotion.failed`, `promotion.rolled_back`. Payloads carry ids, names, counts, artifact versions and the actor — never row data.

Consumed: `plugin.version.published`, `theme.version.published`, `workflow.version.published`, `ai_agent.version.published` — used to refresh the artifact manifest and recompute drift so the promotions screen stays accurate without polling; `organization.created` to provision the production environment row.

Webhook relevance: yes — the promotion lifecycle is exactly what change-management tooling subscribes to, and `promotion.requested` additionally drives the approval surface (REQ-059) so an approver is notified outside the admin.

Audit: create, refresh, reset, extend, delete, sandbox migration apply, and every promotion transition with actor, target environment, plan hash and maintenance window.

### Acceptance criteria

- [ ] Every organization has exactly one production environment row, provisioned at organization creation.
- [ ] Creating a sandbox yields an isolated schema, storage prefix and cache namespace; a query from the sandbox session cannot read production tables.
- [ ] `structure` copies no rows; `content` copies rows for the selected entities up to the caps; `content_media_refs` copies media rows as references without duplicating objects.
- [ ] Row counts in the Data tab match the sandbox tables, and truncated copies show the applied cap.
- [ ] Masking is applied during the copy: masked fields in the sandbox never contain a production value (asserted on a known source value).
- [ ] Refresh re-copies the selected entities and leaves sandbox-only artifacts (plugins, themes, workflows tested there) intact.
- [ ] Reset returns the sandbox to a fresh copy of the chosen mode and clears sandbox-only artifacts after confirmation.
- [ ] Pending migrations show statements and dry-run stats, and `Apply in sandbox` changes only the sandbox schema.
- [ ] A migration that fails mid-way is marked `failed`, leaves the sandbox usable, and is never promotable.
- [ ] Drift is accurate: publishing a plugin version in the sandbox makes the Production and Sandbox versions differ in the Artifacts tab within seconds.
- [ ] A promotion plan lists migrations, plugins, themes, workflows, AI agents and settings diffs with per-item risk labels and no row data.
- [ ] A conflicting artifact blocks approval until it is deselected or resolved, with an inline explanation.
- [ ] Approval and execution are separate permissions, and a two-person policy rejects self-approval regardless of the UI.
- [ ] Execution applies items in order, records per-item status, and reaches `succeeded` only when every item applied; a failure stops the run and marks the promotion `failed`.
- [ ] The promotion records the plan hash, approver, executor and window, and production artifact versions match the plan afterwards.
- [ ] `Rollback` on the latest successful promotion restores previous artifact versions and links through `rollback_of` on a new record.
- [ ] TTL expiry emits `environment.expiring` at T-48h and then destroys the schema and prefix at expiry, marking the row `expired`.
- [ ] Deleting a sandbox removes its schema and storage prefix and never touches production data or shared media objects.
- [ ] The sandbox banner is present on every admin page while the session is on a sandbox, and mutating actions are visibly marked.
- [ ] Production counts and artifact versions are unchanged after the full walkthrough except for what the plan applied.

### QA plan

Browser walkthrough:

1. `/environments` on a seeded organization → production card plus empty sandbox state; the wizard rejects an invalid slug and a zero-entity selection.
2. Create a sandbox (`content` mode, three entities, 14-day TTL, default masking) → progress completes and status reaches `active` with accurate row counts in the Data tab.
3. Open the sandbox host → the amber banner shows the sandbox name; run a workflow that creates a record → the record exists in the sandbox and not in production.
4. Enable a plugin version and publish a workflow version inside the sandbox → the Artifacts tab shows `ahead` rows.
5. Apply a pending migration in the sandbox after reviewing statements and dry-run stats → production still lists it as pending.
6. `/environments/{id}/promote` → the plan lists the migration and artifacts with risk labels; approving with a conflicting item selected is blocked with an explanation.
7. Approve with an approver account, then execute with another account → the step log completes and production shows the new versions and applied migration.
8. Compare production counts and versions before and after → identical to the plan, with no row-data changes outside the migration.
9. Roll back the promotion → production returns to the previous versions and a new record links via `rollback_of`.
10. Refresh the sandbox with one entity → counts update and sandbox-only artifacts remain.
11. Set a short TTL in settings → after the sweeper runs, an expiring notice appears, then the environment disappears and is gone from the delete list.
12. Delete a sandbox explicitly → schema and prefix removed, production untouched, and the audit log shows both the manual delete and the expiry path.
13. Mobile 390×844 → list renders as cards and the promotion review is single-column with the name-copy helper for typed confirmation.

Visual check: sandbox versus production is unambiguous (banner, badge, button markers, never colour alone); risk labels in the plan are readable and distinct; typed destructive confirmations look unmistakably destructive; status and drift chips pair icon with label.

### Slices

1. **Environment registry + clone.** Migration, production provisioning at organization creation, sandbox creation with `structure` and `content` modes, schema/prefix/namespace isolation, list and detail screens.
   Done: a created sandbox shows accurate row counts, is reachable at its host, and cannot read production rows (isolation test passes).
2. **Sandbox migrations + artifact tracking.** Migration preview and apply inside the sandbox, artifact manifest refresh on publish events, the Migrations and Artifacts tabs.
   Done: applying a migration changes only the sandbox, and a published artifact appears as drift without a manual reload.
3. **Promotion + rollback.** Plan builder with risk labels and conflicts, approval/execute split, step log, rollback record, approval notification path.
   Done: the walkthrough promotion and rollback steps pass, production versions match the plan, and the audit trail is complete.
4. **Lifecycle polish.** Masking profiles with field preview, refresh and reset, TTL warning and expiry sweeper, drift summary card, sandbox banner, mobile layout.
   Done: masking is verified on a known source value, expiry emits and cleans up correctly, and the banner plus markers appear across the admin.

### Risks / notes

- Cloning is the expensive operation: cap total copied rows, run the copy in bounded resumable batches, and always clean up a failed clone so no orphan schema survives.
- Masking defaults matter more than options: personal-data fields (email, phone, national id, address, payment references) are masked unless a field is explicitly excluded, and every exclusion is audited.
- Connection pools multiply per environment: sandbox pools are small and separate so a sandbox cannot exhaust the production pool.
- Migration safety: previews must be generated from the real statement set; destructive statements (drop column, type narrowing) need an extra acknowledgement, and lock expectations are shown in the dry-run.
- Same-artifact principle: promotion carries immutable, hashed artifact versions (docs/05-VERSIONING.md §17) and never rebuilds in production, otherwise rollback cannot be guaranteed.
- Conflict semantics need one clear rule (production wins until the operator chooses) with a visible explanation; silent last-write-wins would erode trust in the review step.
- Storage duplication: sandbox uploads live in the sandbox prefix and are deleted with it; media references must never be rewritten in place, or production assets would move.
- Expiry surprises: warn at T-48h in-app and through the webhook, and keep a grace period (default 24h) before the schema is dropped.
- Approvals must resist convenience: when the organization policy requires two people, the API rejects self-approval even if a client ignores the UI.
