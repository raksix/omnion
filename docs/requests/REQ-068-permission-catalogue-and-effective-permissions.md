# REQ-068 — Permission Catalogue & Effective Permissions

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The permission vocabulary and the "why can this user do that" answer.

- Catalogue screen: every permission key (content.pages.publish, media.upload, deployment.deploy …) grouped by module with description and bound endpoints.
- Explicit deny that overrides every inherited allow; default deny; precedence resolution order documented in the UI.
- Effective permissions viewer per user: resolved list with the source (role, group, scope, service account).
- Multiple roles per user merged correctly; conflict highlighting.
- Scope-aware catalogue: global, organization, site, department, module and resource scopes.

## Implementation spec

### Scope (in / out)

**In**
- Catalogue screen: every key grouped by module with description, scope kinds, bound endpoints (method + path), assignability, a dangerous flag, and how many roles use it; search across key and description; filters by module, scope, assignable and dangerous; CSV export.
- Key detail: the endpoint list each key guards, the roles that allow or deny it with counts, the modules it belongs to, and the migration that introduced it.
- Coverage report: a build-time comparison of the route table against the catalogue — guarded routes with no key, keys with no route, non-assignable keys referenced by a policy, and plugin-declared keys outside the catalogue. Drift is a CI failure, not a warning nobody reads.
- Precedence documentation: the resolution ladder is rendered from the same constant the resolver uses (`explicit deny` → `policy deny` → `explicit allow` → `policy allow` → `inherited allow` → `default deny`), with a worked example and links into the simulator (REQ-074).
- Effective permissions viewer for any subject (user, group, service account) at any scope: one row per key with effect, source (role chip, binding scope, provenance), path (explicit, inherited, policy), scope badges, and a conflict flag where a deny kills an allow; filters for effect, source, scope, module, `only differences from the parent scope` and `only conflicts`; text search; CSV export; `Explain` deep-links the key into the simulator.
- Multi-role merge shown honestly: every row names the winning source, and losing grants stay visible as "overridden by …", so "why can this user do that" has an answer instead of a merged blob.
- Comparison mode: pick subject A and subject B (or a role template) and read the diff — `only A`, `only B`, `both` — with counts.
- Cache and freshness: the resolved set is cached per (subject, scope) with a source hash, invalidated by role, binding, group, policy and catalogue changes; the panel shows `computed Ns ago` with a `Refresh` action, and the API accepts an explicit refresh parameter.

**Out**
- Authoring roles (REQ-067), policies (REQ-069), scope bindings (REQ-070), groups (REQ-071) or service accounts (REQ-072) — this REQ reads and explains, it never edits access.
- The interactive simulator experience (REQ-074) and approval-based grants (REQ-073); here only the API and the deep links.
- Free-text documentation of permissions beyond the catalogue description field.

### Screens (UI)

Nav: **Settings → IAM → Permissions**, plus an **Effective permissions** tab on the user detail screen.

| Route | Screen |
|---|---|
| `/settings/iam/permissions` | Catalogue grouped by module |
| `/settings/iam/permissions/{key}` | Key detail: endpoints, roles, modules |
| `/settings/iam/permissions/coverage` | Route ↔ catalogue drift report |
| `/settings/iam/permissions/precedence` | The resolution ladder, rendered from code |
| `/settings/iam/effective` | Subject picker, resolved set, compare mode |
| `/settings/iam/users/{id}` (tab) | Effective permissions for that user |

- **Catalogue.** Sticky module headers with counts; rows: Key (monospace, copy button), Description, Scope badges (G, O, S, D, M, R), Endpoints count (expandable inline), `dangerous` badge, Roles (allow/deny counts). Filters in a left rail: module checkboxes, scope multi-select, `assignable` / `not assignable`, `dangerous only`; search matches key and description. Toolbar: `Export CSV` (the current filter set, not the page). Empty state explains that the catalogue is seeded from the platform's own code, so an empty screen means a seeding failure — not a normal state.
- **Key detail.** Header: key, description, module, introduced-in migration, `dangerous` badge. Endpoints table: Method, Path, Source (`route` / `plugin`). Roles table: Role, Effect (`allow` / `deny`), Organization, Bindings count. `Copy key` and `Open in simulator` actions. A warning block appears when a key is not assignable and a stored role entry references it.
- **Coverage.** Three sections: `Guarded routes without a catalogue key` (method, path), `Keys without a guarded route` (key), `Non-assignable keys used by roles or policies` (key, holder). Each row links to its source; the header shows the counts and the last build that produced the report. A green state names the build id, so a stale report is visible.
- **Effective viewer.** Subject picker (type, search, recent subjects) plus a scope picker; header strip: subject chip, scope, `computed Ns ago`, `Refresh`, and a conflict counter. Table: Permission, Effect (green allowed / red denied, neutral when absent), Source (role chip, binding scope, provenance), Path, Scope, Conflict. Filters and search apply server-side; `only-conflicts` and `only-differences` are one click. Row expands to the chain: binding → role → entry → via → (policy, when one applied). Compare mode splits the picker into A/B and adds a `Side` column plus counts. Empty state for a subject with no bindings: `This subject inherits default deny for every key` with a link to their bindings.
- **Precedence.** One static page: a vertical ladder with the six rungs, a plain-language sentence per rung, a worked example ("explicit deny in one role beats an explicit allow in another; an inherited allow applies unless denied; nothing bound means default deny"), and `Try this in the simulator`.
- **States, keys, mobile.** Skeletons, error strip with retry, and a stale-cache notice; keys: `/` search, `j`/`k` rows, `enter` expands, `y` copies the key, `c` toggles conflicts, `Esc` closes. Below `lg` the catalogue becomes module cards with expandable key rows, the effective table becomes a card list keeping effect and source visible, and the precedence ladder stacks without horizontal scroll.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/iam/permissions` | Catalogue with filters: module, scope, assignable, dangerous, search | `iam.permissions.read` |
| GET | `/api/v1/iam/permissions/{key}` | Key detail: endpoints, roles using it, module, migration | `iam.permissions.read` |
| GET | `/api/v1/iam/permissions/coverage` | Route ↔ catalogue drift report with build id | `iam.permissions.read` |
| GET | `/api/v1/iam/permissions/precedence` | The ladder and its text, served from the resolver's own constant | `iam.permissions.read` |
| GET | `/api/v1/iam/subjects/{id}/effective-permissions` | Resolved set with sources at a scope (`?scope=&refresh=`) | `iam.permissions.read` |
| GET | `/api/v1/iam/effective-permissions` | Own resolved set — the documented permission-less exception | signed-in session |
| GET | `/api/v1/iam/effective-permissions/diff` | Compare two subjects or a subject and a template | `iam.permissions.read` |
| POST | `/api/v1/iam/effective-permissions/explain` | Resolution chain for (subject, key, resource), including any policy stage | `iam.permissions.read` |

Responses carry per-row `source` objects (`role_id`, `role_key`, `binding_scope`, `via`, `policy_id`), so the viewer never re-derives provenance client-side. The self endpoint stays permission-less for the same reason `GET /api/v1/me` does: it answers what the caller may do, not what anyone else may do.

### Data model

Migration: `0121_permission_catalogue.sql` (reserved band 0116–0125 for the identity & access wave; the ledger is append-only — take the next free number if taken). The base `permissions` table lands with `0002_iam.sql` and stays the projection of `crates/permissions::catalogue`; this migration adds metadata plus two new tables.

- `permissions` += `module` text not null default 'core', `scope_kinds` text[] not null default '{global,organization,site}', `is_assignable` bool not null default true, `is_dangerous` bool not null default false, `sort_order` int not null default 100, `docs` text not null default '', `deprecated_at` timestamptz null — check `scope_kinds` is non-empty and a subset of the known scope vocabulary; GIN index on `scope_kinds`; index (module, sort_order).
- `permission_modules` (key text pk, label text not null, description text not null default '', icon text, position int not null default 100) — seeded from the same code constant that groups the catalogue; the panel never invents a group.
- `permission_endpoints` (id uuid pk, permission_key text not null → permissions on delete cascade, method text not null, path text not null, source text in ('route','plugin') default 'route', created_at) — unique (method, path, permission_key); index (permission_key). Rows are generated by a build step that imports the router, so the list cannot drift by hand.
- `effective_permission_cache` (subject_type text in ('user','group','service_account'), subject_id uuid, scope_key text not null default 'global', grants jsonb not null default '{}', denials jsonb not null default '{}', conflicts jsonb not null default '[]', source_hash text not null, computed_at timestamptz not null default now(), expires_at timestamptz not null) — primary key (subject_type, subject_id, scope_key); index (expires_at).
- Seeding: a migration step backfills `module`, `scope_kinds` and the two flags for every existing key from the code constant, and `permission_endpoints` from the generated route map; a parity test asserts code ↔ table equality both ways.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `iam.catalogue_updated` | Catalogue projected from code at migrate/startup | `added`, `removed`, `changed` key counts, `build_id` |
| `iam.catalogue_drift_detected` | Coverage run found a mismatch | `kind` (`route_missing_key`,`key_missing_route`,`plugin_unknown_key`), `items` count |
| `iam.effective_cache_invalidated` | A role, binding, group or policy change dropped cached sets | `subject_type`, `subject_id`, `reason` |
| `iam.permission_conflict_detected` | A cached resolution first showed a deny killing an allow | `subject_type`, `subject_id`, `permission_key` |

Consumed: `iam.role_permissions_changed`, `iam.role_priority_changed`, `iam.binding_created`, `iam.binding_revoked`, `iam.policy_changed`, `iam.group_membership_synced` — each drops the affected cache rows. Webhook relevance: none of these are webhook-worthy except `iam.catalogue_drift_detected`, which the CI gate consumes; payloads carry ids, keys and counts, never descriptions of who holds what.

### Acceptance criteria

- [ ] `0121` applies on a fresh and on a populated database; every existing catalogue row is backfilled with a module and scope kinds; the parity test passes in both directions.
- [ ] The catalogue screen renders every key grouped by module with description, scope badges, endpoint count and role usage counts.
- [ ] Search, module, scope, assignable and dangerous filters narrow exactly the matching rows, and CSV export returns the filtered set with every column visible on screen.
- [ ] A key detail shows the real method/path pairs it guards; adding a guarded route without a key makes the coverage report red and fails the CI gate.
- [ ] Removing a route whose key remains produces the `key without a route` row instead of silently passing.
- [ ] The precedence page renders the ladder from the same constant the resolver uses — changing the constant changes the page with no second edit.
- [ ] The effective viewer lists every key for a subject with the winning source named (role, binding scope, provenance) and the losing sources listed as overridden.
- [ ] An explicit deny in one role beats an explicit allow in another in both the viewer and a guard call for the same subject and key.
- [ ] No binding for a key renders as denied with reason `no source`, never as an empty cell that reads like an error.
- [ ] A key granted by two roles shows one winning row and both sources; the merge never duplicates the key.
- [ ] A conflict row is flagged, counted in the header, and included in `only-conflicts`.
- [ ] Compare mode shows `only A`, `only B` and `both` with correct counts for two subjects that share one role.
- [ ] A role, binding or policy change invalidates the affected cached set: the viewer shows a fresh `computed Ns ago` after the change and the new verdict matches the guard.
- [ ] A scope change (organization vs site) changes the resolved set for a site-scoped binding exactly as the guard does.
- [ ] `GET /api/v1/iam/effective-permissions` answers for the caller without a permission key, while another subject's set requires `iam.permissions.read` (403 otherwise).
- [ ] The viewer answers for a 5k-binding organization inside the request budget (p95 < 300 ms on a cached read), and every new screen renders at 390 px with zero high findings.

### QA plan

The walkthrough must visit `/settings/iam/permissions` (search a key, open its detail, expand the endpoint list, toggle the dangerous filter, export CSV), `/settings/iam/permissions/coverage` (read the three sections; in the QA build the report is green and names the build), `/settings/iam/permissions/precedence` (read the ladder and follow `Try this in the simulator`), and `/settings/iam/effective` (pick a user with two roles, read a row's chain, toggle `only-conflicts`, compare two subjects, use `Explain` and land on the simulator with the key pre-filled). Visual check: module grouping is obvious, scope badges are legible, the allow/deny colours meet AA contrast, an overridden row reads as overridden rather than missing, the conflict flag is unmistakable, the freshness line shows a real timestamp, and screenshots `page-iam-permissions`, `page-iam-permission-detail`, `page-iam-coverage`, `page-iam-effective`, `page-iam-precedence`, `mobile-iam-permissions` are produced.

### Slices

1. **Catalogue metadata and coverage.** Migration with the new columns and the module seed, the endpoint generation step, the coverage report and its CI gate, the catalogue screen with filters and export, key detail. *Done when:* acceptance 1–5 pass and the catalogue plus coverage screens are in the walkthrough inventory.
2. **Effective viewer and cache.** Resolved-set API with per-row sources, the (subject, scope) cache with invalidation listeners, the viewer with scope picker, filters, chains and the self endpoint. *Done when:* acceptance 7–10, 13–16 pass and the guard and viewer agree for a scripted matrix of subjects and scopes.
3. **Conflicts, compare, precedence.** Conflict computation and highlighting, compare mode and its diff endpoint, the precedence page served from the resolver constant, `Explain`, and the polish pass (keyboard, mobile, empty states). *Done when:* acceptance 6, 11–12 pass, the precedence page changes with the constant, and the wave's QA report shows zero high findings.

### Risks / notes

- **The catalogue is code, the table is a projection.** Any hand edit to the table is lost on the next projection — the seeding step must be idempotent and the parity test must fail loudly, or rows and reality drift apart.
- **Endpoint lists must be generated.** A hand-maintained method/path list goes stale the first busy week; the build step reads the router, and the coverage gate makes drift a red build.
- **One resolver.** The viewer, the guard and the diff all call the same evaluation path (REQ-067, REQ-069) — a second "effective" implementation is how a viewer starts lying politely.
- **Cache correctness over speed.** Every write that can change a resolution must invalidate within the same transaction or via a bus listener with a bounded staleness that the UI states; a stale green row is worse than a slow page.
- **Conflict semantics.** A conflict is a deny killing an allow on the same key — not two allows from different roles. Keeping that definition in one place prevents a UI that cries wolf.
- **Privacy.** Another subject's effective set is read-guarded and the CSV export obeys the same scope rules; the self exception never widens into a directory listing.
- **Scope vocabulary is shared.** `scope_kinds` uses the same tokens as the binding model (REQ-070); introducing a second spelling in the UI would silently break scope filters.
- **Large tenants.** Lists and counts must page and index; rendering the full key set with per-key role joins in a loop will not survive an enterprise catalogue.
