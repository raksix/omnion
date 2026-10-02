# REQ-017 — Sandbox / Staging

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

The admin clicks **Create Staging Environment**:

```text
Production
     │
     └── Clone
          ↓
       Staging
```

Changes are tried there first:

```text
Staging
 ↓
Preview
 ↓
Approve
 ↓
Deploy to Production
```

## Implementation spec

Staging is a **content-and-configuration environment inside the same installation**: a second copy of an organization's addressable content that the panel can enter, edit and later
promote back.
It is deliberately not infrastructure duplication — one database, one deployment, two environments — and the spec says so everywhere the UI could imply otherwise.

### Scope (in / out)

**In**

- Environment records per organization: one `production` (created with the organization) and zero or more `staging` environments, each with a key, a name, a status and a staging host.
- **Clone**: copy pages, revisions, translations, menus/settings records and media *references* from production into a staging environment as a tracked job with progress and per-area
  counts.
- **Enter staging**: an environment switcher in the panel header that scopes content screens to the chosen environment, with a permanent, non-dismissible staging banner.
- **Changes view**: the diff between a staging environment and production — added, updated, deleted items per area, with author and timestamp, conflict flags when production moved on.
- **Promotion**: request → approve → apply a *frozen* change set to production in one transaction, with optimistic concurrency per row, an audit trail and a promotion history.
- Staging hosts are excluded from search engines and marked `noindex`; public preview rendering of staging content is REQ-018's business, not this one's.

**Out**

- Separate database instances, containers or clusters per environment (REQ-035 / REQ-036 cover infrastructure-level isolation); this request never provisions infrastructure.
- Application version promotion, rollback of builds, replica/CPU views (REQ-024 Deployment Center).
- Plugin, theme and workflow sandboxes for experiments beyond promotion (REQ-034).
- Billing, per-environment quotas, or user account separation — identity and roles are shared, and staging access is a permission, not a separate login.

### Screens (UI)

- **`/environments` — list.** Table columns: Name, Type (badge: production / staging), Status (active / cloning / error / archived), Content (pages + translations counts of the last
  clone), Staging host (copyable, copy button), Last clone (relative time + actor), Promotions (pending count linking to the detail tab), actions (Open staging, Re-clone, Promote,
  Archive). Filters: type, status, text search. Bulk: none (archive is per-row and destructive). Empty state when no staging environment exists:
  one-line explanation of what staging is and a `Create staging environment` button.
- **`/environments/new` — create wizard (3 steps).** Step 1: Name (1–64 chars) and key (lowercase slug, unique per organization, auto-derived, editable); Step 2: clone options —
  checkboxes for Pages & revisions, Translations, Menus & navigation records, Site settings, Theme selection, Workflow definitions, with a live estimate ("~412 rows · ~18 MB of metadata,
  media files are referenced, not copied") and a `Exclude archived pages` toggle; Step 3: summary + `Create and clone`. The wizard blocks on: empty name, invalid key, duplicate key, zero
  areas selected. Nesting limit:
  a staging environment can never be cloned to another staging environment — the option is not offered.
- **`/environments/[id]` — detail.** Header: name, type badge, status, host, primary actions (Open staging, Re-clone, Promote, Archive). Tabs: Overview (clone metadata, per-area counts,
  storage note, host, who requested the clone), Changes (the diff table), Promotions (history + in-flight promotion), Activity (audit entries for the environment). Changes table columns:
  Item (title, links to the record), Area (Page / Translation / Menu / Setting / Theme), Change (added / updated / deleted badges), Last change by, Last change at, Conflict (badge when
  production changed since the clone). Filters: area, change type, conflicts only, text. Bulk:
  select non-conflicting rows → `Promote selection`.
- **Promotion dialog.** Shows the frozen change set summary (counts by area), a conflict list when any, the requester, and — above 25 changes — a typed confirmation of the environment
  name. Primary button `Request promotion` (creates a `pending_approval` record) or `Approve and deploy` for a caller holding the deploy permission.
  Progress replaces the dialog with a step timeline (validate → apply → audit → done) that survives a refresh.
- **Header environment indicator.** `AppShell` header gains an environment chip next to the site switcher: `Production` (neutral) or `Staging — not public` (warning colour, with `Exit to production`).
  While a staging environment is active, every content screen shows a thin warning top border and the banner text is repeated at the top of the page.
- **States.** Loading: wizard estimate and Changes tab use skeletons. Error during clone: status `error`, the Overview tab shows the failing area, the error string and a `Retry clone`
  button. Clone in progress: a progress bar with `items_done / items_total` that updates without a manual refresh; a cancel action marked as destructive.
  Empty Changes: "No changes since the clone."
- **Keyboard / mobile.** `/environments` supports `/` search, `n` new environment, `Enter` open, `Esc` close dialog.
Below `lg` the tables become cards, the wizard becomes a single scrolling form, the promotion dialog becomes a full-height sheet with the primary action pinned at the bottom, and the
environment chip stays visible in the sticky header.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/environments` | List environments of the organization | `deployment.read` |
| POST | `/api/v1/environments` | Create a staging environment and start its clone | `deployment.preview` |
| GET | `/api/v1/environments/{id}` | Environment detail, per-area counts, job state | `deployment.read` |
| POST | `/api/v1/environments/{id}/clone` | Re-clone from production (discards staging changes after confirmation) | `deployment.preview` |
| GET | `/api/v1/environments/{id}/clone-jobs` | Clone job history and current progress | `deployment.read` |
| POST | `/api/v1/environments/{id}/clone-jobs/{job_id}/cancel` | Cancel a running clone | `deployment.preview` |
| GET | `/api/v1/environments/{id}/changes` | Diff vs production; `area`, `change`, `conflicts`, `cursor` | `deployment.preview` |
| POST | `/api/v1/environments/{id}/promotions` | Request a promotion of selected changes (frozen change set) | `deployment.deploy` |
| GET | `/api/v1/environments/{id}/promotions` | Promotion history of the environment | `deployment.read` |
| GET | `/api/v1/promotions/{id}` | One promotion: change set, conflicts, step log | `deployment.read` |
| POST | `/api/v1/promotions/{id}/approve` | Approve and apply the frozen change set to production | `deployment.deploy` |
| POST | `/api/v1/promotions/{id}/cancel` | Cancel a pending promotion | `deployment.preview` |
| DELETE | `/api/v1/environments/{id}` | Archive a staging environment (content kept, host released) | `deployment.rollback` |

Errors are named: `environment_not_found`, `environment_key_taken`, `staging_nesting_refused`, `clone_already_running`, `promotion_conflict` (with the conflicting item ids),
`self_approval_refused`, `environment_not_staging`.

### Data model

Migration `0012_environments.sql` (number is a placeholder — renumber to the next free slot):

- `environments` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null references organizations(id) on delete cascade`, `key text not null`, `name text not null`,
  `type text not null check (type in ('production','staging'))`, `status text not null default 'active' check (status in ('active','cloning','error','archived'))`,
  `cloned_from_environment_id uuid references environments(id) on delete set null`, `cloned_at timestamptz`, `staging_host text`, `created_by uuid references users(id) on delete set null`,
  `created_at`, `updated_at`. Constraints: `key ~ '^[a-z0-9]([a-z0-9-]{0,53}[a-z0-9])?$'`, `length(btrim(name)) between 1 and 64`, `unique (organization_id, key)`, and a partial unique
  index guaranteeing exactly one production environment per organization: `create unique index environments_single_production_key on environments (organization_id) where type = 'production'`.
  `staging_host` gets a unique index where not null.
  Existing organizations are backfilled with their production environment inside the same migration.
- `environment_clone_jobs` — `id uuid pk`, `environment_id uuid not null references environments(id) on delete cascade`, `status text not null default 'pending' check (status in ('pending','running','done','failed','cancelled'))`,
  `areas text[] not null`, `items_total integer not null default 0`, `items_done integer not null default 0`, `error text`, `started_at timestamptz`, `finished_at timestamptz`,
  `created_by uuid`, `created_at`.
  Index `(environment_id, created_at desc)`.
- `promotions` — `id uuid pk`, `environment_id uuid not null references environments(id) on delete cascade` (source staging), `target_environment_id uuid not null references environments(id) on delete cascade`,
  `status text not null default 'pending_approval' check (status in ('pending_approval','approved','running','done','failed','cancelled'))`, `changes jsonb not null` (the frozen change
  set: item id, area, operation, base snapshot hash), `conflicts jsonb not null default '[]'::jsonb`, `requested_by uuid`, `approved_by uuid`, `approved_at timestamptz`, `step_log jsonb not null default '[]'::jsonb`,
  `error text`, `created_at`, `updated_at`, `finished_at`.
  Index `(environment_id, status, created_at desc)`, partial index on `(status) where status in ('pending_approval','running')`.
- Environment ownership on content: `alter table pages add column environment_id uuid references environments(id) on delete cascade`, backfilled to the organization's production
  environment and made `not null` after the backfill; same column and treatment for `menus`, `site_settings` and `translations` (translations are resolved through the row's environment
  when a row is written by a staging context).
  New composite indexes `(environment_id, site_id)`, `(environment_id, updated_at desc)`.

### Events

- **Emitted:** `environment.created`, `environment.clone.started`, `environment.clone.completed` (with per-area counts), `environment.clone.failed` (with the failing area),
  `environment.archived`, `promotion.requested`, `promotion.approved`, `promotion.completed`, `promotion.failed`, `promotion.conflict` (with conflicting item ids).
- **Consumed:** `page.published`/`page.updated` inside a staging environment advance the Changes diff cache; nothing else is consumed.
- **Webhook relevance:** promotion lifecycle events are the CI/CD signal — an endpoint subscribed to `promotion.*` can trigger a build, a cache purge or a smoke test after a deploy, and
  `environment.clone.completed` tells a test runner that fresh staging data is ready. Production promotion deliberately does **not** re-emit `page.published` for every copied row (that
  would flood subscribers);
  one `promotion.completed` carries the affected ids.

### Acceptance criteria

- [ ] A new organization gets exactly one `production` environment; a second production insert fails at the database (partial unique index proven in a test).
- [ ] `POST /api/v1/environments` creates a staging environment with status `cloning` and returns immediately; the clone job reaches `done` and per-area counts match the production
  counts.
- [ ] Clone copies pages, revisions, translations, menus, site settings, theme selection and workflow definitions, and copies **no** media blobs (verified by storage object count before
  and after).
- [ ] Clone is idempotent: re-cloning an unchanged environment produces the same counts and no duplicate rows (natural keys are unique per environment).
- [ ] Staging nesting is refused with `staging_nesting_refused` for a staging source.
- [ ] Editing a page in staging leaves the production row byte-identical (asserted by comparing `updated_at` and revision hashes).
- [ ] `GET /api/v1/environments/{id}/changes` lists the edited page as `updated`, a new page as `added`, a deleted page as `deleted`, each with author and timestamp.
- [ ] A production edit made after the clone marks the item `Conflict`, and promoting a change set that contains conflicts is refused with `promotion_conflict` listing item ids.
- [ ] Promotion of a clean change set applies every item in one transaction: production pages match staging content afterwards, and `promotion.completed` carries the same item count.
- [ ] A failure injected mid-apply leaves production unchanged (transaction rolled back) and the promotion status `failed` with a readable error.
- [ ] Self-approval is refused for a requester without the deploy permission, and the same person holding the deploy permission can approve (both paths covered by tests).
- [ ] Promotion keeps a history row with requester, approver, timestamps and the frozen change set, visible in the Promotions tab.
- [ ] `promotion.*` events arrive at an endpoint subscribed to `promotion.*` within the delivery window.
- [ ] The environment chip appears in the panel header while staging is active, the staging banner cannot be dismissed, and staging hosts answer with `X-Robots-Tag: noindex`.
- [ ] All new routes answer `403` without their permission and `404` for another organization's environment.
- [ ] Archive releases the staging host and leaves the content readable in the archived state.
- [ ] The QA walkthrough visits `/environments`, `/environments/new` and `/environments/[id]` with zero high findings.

### QA plan

The walkthrough creates a staging environment from a seeded production site, watches the clone progress to `done`, opens staging, edits one page and adds another, then visits
`/environments/[id]` → Changes and confirms both items with the right change types. It continues through `Request promotion` → approve → progress → `done`, then switches back to
production and confirms the edited and added pages are live. It then re-clones, confirms the staging edits are gone after the confirmation dialog, and finally archives the environment.
Controls exercised: wizard steps 1–3 with validation errors, `Exclude archived pages`, estimate display, Changes filters, conflicts-only toggle, typed confirmation, cancel action,
Promotions tab, mobile banner at 390 px. The visual check must see:
a staging banner that visibly differs from production, a progress bar that actually moves, a Changes table with real titles and author names, a conflict badge on a deliberately
conflicted item, and no dead buttons or placeholder text.

### Slices

1. **Environment model + clone.** Migration, environments CRUD (list/create/get/archive), clone job + runner, `/environments` list screen with status and counts.
*Done line:* an operator creates a staging environment, the clone finishes, and the list shows real per-area counts with an honest progress state while it runs.
2. **Staging context + changes.** Environment chip and banner, content screens scoped by environment, `changes` diff endpoint, `/environments/[id]` with Overview and Changes tabs.
*Done line:* editing a page in staging appears in Changes as `updated` with the editor's name, and production is untouched.
3. **Promotion.** Promotion records, approve endpoint, frozen-change-set apply with conflict detection, Promotions tab, `promotion.*` events. *Done line:* a requested promotion is
approved, applied atomically, visible in history, and emitted to a subscribed endpoint.
4. **Hardening.** Clone cancel/retry, `noindex` on staging hosts, large-site batching, conflict refresh, error states and the archive path. *Done line:* a cancelled clone leaves no
partial environment marked active, and a conflicted promotion is refused with item-level detail.

### Risks / notes

- Honesty in the UI matters more than features here: staging is a *content* environment inside one installation. The wizard and banner say so; claiming infrastructure isolation would be
  a lie.
- Clone cost grows with site size: the runner batches by area with a configurable page size, writes progress after each batch, and refuses to clone beyond a configured row ceiling (with
  a clear message) instead of locking the database.
- Media is referenced, not duplicated — staging shows the same files. Editing a media *record* in staging is allowed; replacing the underlying file is not part of this request.
- Promotion conflicts are expected, not exotic; the UI must lead with them rather than hiding them behind a failure toast.
- Concurrent promotions on one environment are serialized: a second request while one is `running` is refused with a named error.
- The frozen change set is the same-artifact principle from `docs/05-VERSIONING.md` §17 applied to content: what the approver saw is exactly what runs.
