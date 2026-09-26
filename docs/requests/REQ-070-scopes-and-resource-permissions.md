# REQ-070 — Scopes & Resource-Level Permissions

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Granting exactly the right slice.

- Scope model: global, organization, site, department, module, single resource.
- Scoped role assignment UI (assign a role inside a scope, see all assignments of a user).
- Resource allow/deny lists (this page, this media folder, this site).
- Cross-site isolation guarantees with tests (no leakage between sites or organizations).
- Per-site role variation (same user, different role per site).

## Implementation spec

> **Where:** `crates/permissions` (scope model, resource grants, resolution) · `apps/api/src/routes/iam.rs` (scope and grant surface) · `apps/admin/features/iam/scopes/**` (new screens) · **Migration:** `database/migrations/0016_scopes_and_resource_permissions.sql` (next free number at land time; the ledger is append-only — take the next free slot if one is already used) · **Admin routes:** `/settings/iam/scopes`, `/settings/iam/scopes/{id}` · **Permission family:** `iam.scopes.*`, `iam.bindings.*`, `iam.resource-grants.*`, `sites.read` · **Depends on:** REQ-006 (the binding model this request deepens), REQ-067 (role picker reused in the scope assign drawer), REQ-068 (catalogue keys and effective-permission view), REQ-069 (ABAC evaluates after scope, so the two must not diverge), REQ-074 (the simulator consumes the same resolution function).

### Scope (in / out)

**In**

- **Every scope level becomes real.** `role_bindings.scope_type` grows from `global | organization | site` to also carry `department`, `module` and `resource`; each level stores exactly the identifiers it needs (`organization_id`, `site_id`, `department_id`, `module_key`, `resource_type` + `resource_id`) and a shape check refuses a half-filled row.
- **Departments** are a per-organization tree (parent link, depth ≤ 6) with membership. A binding at `department` applies to every member of that department and, by inheritance, of its child departments — never to a sibling.
- **Module scope** binds to a named product surface (`content`, `media`, `commerce`, `workflows`, …) so a role can be granted inside one module only; the module key is validated against the module registry, not a free string.
- **Resource scope** binds to one resource family (`page`, `page_tree`, `media_folder`, `site`, `form`, `product`) plus an identifier or a path glob (`/blog/*`). Globs are matched with the documented rules (`*` = one path segment, `**` = any depth, longest-prefix wins) and are evaluated the same way for single-resource checks and for list filtering.
- **Resource allow/deny lists** — `resource_grants` attaches an `allow` or `deny` entry to a subject (user, group or service account) for a resource pattern, independent of roles. Precedence inside a scope: `resource deny > resource allow > role deny > role allow > inherited > default deny`. A resource deny beats a role allow at the same or narrower scope, and the qualifying scope is named in the 403.
- **Scoped assignment UI:** assign a role inside a chosen scope from a drawer (subject → role → scope → window), and a user detail view that lists every binding of that user grouped by scope with the source (direct, group, service account) and the expiry.
- **Per-site role variation:** one user may hold `Editor` on `acme.com` and `Viewer` on `shop.acme.com`; the site switcher re-resolves the session so the answer follows the site actually addressed.
- **Cross-tenant and cross-site isolation:** resolution is always anchored to one organization and one site; a query that would return a binding from another organization is a hard failure in tests, not a silent filter. A seeded isolation matrix (≥ 200 cases) asserts no leakage.
- **List filtering stays SQL-expressible:** `crates/permissions` exposes a scope predicate builder so `/api/v1/pages`, `/media` and friends filter with the same rules the guard uses.

**Out (tracked elsewhere)**

- Condition trees, attribute registry and policy builder → REQ-069; role matrix editor and role versions → REQ-067; catalogue metadata and the effective-permission screen → REQ-068; time-boxed approval windows → REQ-073; the simulator UI → REQ-074; organization hierarchy and tenant lifecycle → REQ-005; ABAC attributes that read resource data (`resource.owner_id`) beyond the two shipped request attributes → REQ-069 slice 2.

### Screens (UI)

Nav entries are added to the existing IAM section: **Scopes · Resource grants**.

| Route | Screen |
|---|---|
| `/settings/iam/scopes` | Scope explorer — tree `organization → site → department` beside a table of bindings at the selected node with counts and expiries |
| `/settings/iam/scopes/{id}` | Scope detail — tabs `Bindings`, `Members`, `Resource grants`, `Effective permissions` |
| `/settings/iam/resource-grants` | Allow/deny list entries with filters (subject, resource family, effect, status) and a create drawer |
| `/settings/iam/users/{id}` (extension) | `Roles & bindings` tab grouped by scope with the source badge and the assign drawer trigger |

- **Scope assign drawer:** subject picker (user / group / service account), role picker reusing the REQ-067 grouped picker, scope picker (level → target, where `resource` reveals family + identifier or glob), optional window, then a preview line: *“Ahmet will get `content.pages.update` on `acme.com` → `/blog/*`”*. Saving is one request; an invalid glob, an unknown module key or a missing target fails the whole save with a field-level error.
- **Scope explorer** columns `Node`, `Level`, `Bindings`, `Members`, `Expiring ≤ 7 days`; row action opens the node, the breadcrumb switches level, and the tree keeps the collapse state per organization in local storage. Follows the admin patterns: `EmptyState` when a node has no bindings, `LoadingTable` skeleton on first paint, inline error with retry.
- **Resource grants** columns `Subject`, `Resource`, `Pattern`, `Effect`, `Added by`, `Status`; the effect is a chip with AA-contrast colour, deny sorts first, and the row shows the narrowest matching role that the entry overrides. Bulk action: revoke selected.
- **User detail grouping:** bindings are grouped by scope level (organization → site → department → module → resource), each group header showing the source (direct / through group _Marketing_ / service account) and a `+N` chip for collapsed entries; expired bindings stay visible with an `expired` chip and a revoke action.
- **States, keyboard, mobile:** `/` focuses the filter, `j`/`k` move row focus, `enter` opens, `esc` closes the drawer, `?` opens the shortcut sheet; below `lg` the tree becomes a breadcrumb and the binding table becomes cards with the same actions.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/iam/scopes` | Scope tree for an organization (sites, departments, module keys) | `iam.scopes.read` |
| GET | `/api/v1/iam/scopes/{scope_type}/{scope_id}/bindings` | Bindings attached to one scope node | `iam.bindings.read` |
| GET, POST | `/api/v1/iam/bindings` | List with scope filters / create a binding at any level (subject, scope, window) | `iam.bindings.read` / `iam.bindings.manage` |
| DELETE | `/api/v1/iam/bindings/{id}` | Revoke a binding (writes the audit entry, keeps the row) | `iam.bindings.manage` |
| GET | `/api/v1/iam/users/{id}/bindings` | Every binding of a user grouped by scope with its source | `users.read` |
| GET, POST | `/api/v1/iam/departments` | Department tree with member counts / create or move a node | `iam.scopes.read` / `iam.scopes.manage` |
| PUT, DELETE | `/api/v1/iam/departments/{id}/members` | Replace or clear membership | `iam.scopes.manage` |
| GET, POST | `/api/v1/iam/resource-grants` | Allow/deny list with filters / create an entry | `iam.resource-grants.read` / `iam.resource-grants.manage` |
| DELETE | `/api/v1/iam/resource-grants/{id}` | Revoke an entry | `iam.resource-grants.manage` |
| POST | `/api/v1/iam/resource-grants/test` | Does this pattern match this path (returns match + which rule) | `iam.resource-grants.read` |
| GET | `/api/v1/iam/scope-check` | Resolve one `(user, action, site?, path?)` triple and answer the qualifying scope | `iam.bindings.read` |
| GET | `/api/v1/sites/{id}/members` | Users holding any binding inside a site with their role per site | `sites.read` |

Every route carries `guards::require("<key>")`; handlers additionally verify that the addressed site or department belongs to the caller's organization and answer `404` (never `403`) for a foreign identifier, so scope existence is not leaked.

### Data model

Additive; the `scope_type` broadening is expand-then-contract per docs/05-VERSIONING.md (add the wider check as `not valid`, backfill, then validate; the narrow check is dropped in the same migration because `role_bindings` is not covered by any public contract).

```text
role_bindings   + department_id uuid references departments (id) on delete cascade,
                  module_key text, resource_type text, resource_id text, resource_path text,
                  granted_reason text, created_by uuid references users (id) on delete set null
                · scope_type check widened to the six levels; shape check per level
                · index role_bindings_scope_idx (scope_type, organization_id, site_id, department_id)
                  where revoked_at is null
                · index role_bindings_resource_idx (organization_id, resource_type, resource_id)
                  where revoked_at is null and scope_type = 'resource'
                · index role_bindings_org_site_idx (organization_id, site_id) where revoked_at is null
```

New tables: `departments` (id, organization_id, parent_id self-reference, key unique per organization, name, position, created/updated) · `department_members` (department_id, user_id, is_manager, added_by, created_at; primary key `(department_id, user_id)`) · `resource_grants` (id, organization_id, subject_type `user|group|service_account`, subject_id, resource_type, pattern, effect `allow|deny`, reason, created_by, expires_at, revoked_at, created_at) · `scope_resolution_cache` (optional, derived: subject_id, organization_id, site_id, path, decision, computed_at — invalidated by `iam.binding_*` and `iam.resource_grant_changed`, never consulted before the live path on a cache miss).

Migration `database/migrations/0016_scopes_and_resource_permissions.sql`: creates `departments`, `department_members` and `resource_grants`, widens `role_bindings`, adds the four indexes, and seeds one `Departments` root per existing organization only if a department tree is later required by the UI — no fabricated membership rows. Commented in the `0002` style, append-only, with the `-- migrate:down` block omitted (released migrations are forward-only).

### Events

- Emitted: `iam.binding_created`, `iam.binding_revoked` (payload now carries `scope_type` and the target identifiers), `iam.department_created`, `iam.department_member_changed`, `iam.resource_grant_changed`, `iam.scope_denied` (state-changing `/api/v1` calls only), `iam.scope_check_run`.
- Consumed: `site.created` seeds the site node in the scope tree; `site.deleted` revokes bindings that address it (one `iam.binding_revoked` per row); `user.created` provisions the personal binding row only once per REQ-006; `iam.binding_created` and `iam.resource_grant_changed` invalidate `scope_resolution_cache` rows for the subject.
- Payloads carry ids and the scope path (`organization/acme/site/acme.com/resource/page:/blog/*`) — never a rendered rule set. `iam.scope_denied` is sampled before it reaches webhooks, since a large page list can emit one per row; the security centre (REQ-012) subscribes to `iam.resource_grant_changed`.

### Acceptance criteria

- [ ] `0016_scopes_and_resource_permissions.sql` applies on a fresh and on a populated database (with existing `scope_type` values preserved); `cargo test --workspace` is green.
- [ ] A binding can be created at each of the six levels; a half-filled or level-mismatched row is refused by the check constraint and the API returns a field-level error naming the missing part.
- [ ] An organization binding applies on every site of that organization; a site binding applies on that site only, and the same user gets a different role on a second site with the site switcher reflecting it on the next request.
- [ ] A department binding applies to members of the department and of its child departments, never to a sibling department; leaving the department drops the permission on the next request.
- [ ] Glob rules are documented and tested: `/blog/*` matches `/blog/post-1` and not `/blog/2026/post-1`, `/blog/**` matches both, an unknown pattern with `?` or a backslash is refused at save time.
- [ ] A resource allow on `page:/legal/*` does not leak into `/blog/*`, and a resource deny on `page:/hr/*` beats an inherited role allow for the same key — the 403 names both the qualifying scope and the winning entry.
- [ ] Cross-tenant isolation: a seeded matrix of ≥ 200 (user, action, organization, site, path) cases resolves identically through the guard, the list predicate and `GET /api/v1/iam/scope-check`, with zero cross-organization hits.
- [ ] `GET /api/v1/iam/scope-check` and the guard return the same verdict for every catalogue key (property test over the seeded matrix).
- [ ] The user detail grouping shows direct, group-inherited and service-account bindings with the correct source label; an expired binding is visible, marked `expired` and does not count.
- [ ] Assigning a role inside a scope writes one audit entry with the before/after (scope, role, window), and the drawer preview text matches what the API actually granted.
- [ ] List filtering uses the same predicate builder: a user scoped to `/blog/*` sees exactly the pages the guard lets them update, and page counts in the header match the rows.
- [ ] The scope tree never shows a node from another organization, and `404` (not `403`) answers a foreign site or department identifier.
- [ ] Resource-grant test endpoint agrees with runtime matching for a fixture of ≥ 50 patterns (including nested globs and trailing slashes).
- [ ] Revoking a binding or a resource grant takes effect on the next request without a restart and without a cache flush command.
- [ ] Every screen has empty, loading and error states; the resource-grants table renders ≥ 500 rows without layout breakage; zero high findings in the QA pass.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam/scopes`, `/settings/iam/scopes/{id}` and `/settings/iam/resource-grants` (desktop) plus `/settings/iam/scopes` (mobile). The script must open the scope explorer, expand a site node, open the assign drawer, attempt an invalid glob (`/blog/[`) and assert the field-level error, then complete a valid assignment scoped to `/blog/*`, reload and assert the binding is listed with its source; create a resource deny for `/hr/*`, confirm the deny chip sorts first, then open the user detail tab and assert the grouping order. Screenshots `page-iam-scopes`, `page-iam-scope-detail`, `page-iam-resource-grants`, `mobile-iam-scopes`. The visual check looks for an honest tree (no fake counts), a readable preview line in the drawer, AA-contrast effect chips, and no horizontal scroll on mobile.

### Slices

1. **Scope levels and departments.** Migration, widened binding shape with per-level validation, department tree + membership, scope explorer and assign drawer, per-site variation. *Done when:* acceptance 2–4 and 11 pass, and the walkthrough creates a department-scoped binding and a second per-site binding for the same user.
2. **Resource scopes and allow/deny lists.** Glob matcher with its rule set and tests, `resource_grants` CRUD, resource scope in the binding model, grant test endpoint, resource-grants screen, 403 reason text. *Done when:* acceptance 5–6 and 13 pass, and the deny-wins case is proven in an integration test.
3. **Isolation and the predicate builder.** Scope predicate builder shared by guard, lists and `scope-check`, seeded isolation matrix, cross-tenant tests, cache invalidation, audit entries and events. *Done when:* acceptance 7–9 and 14 pass and the seeded matrix runs in CI.

### Risks / notes

- **One resolution path.** Guard, list predicate, `scope-check` and (later) the simulator must call the same `crates/permissions` function; a second implementation is a release blocker because scope rules drift silently and only a security review catches it.
- **Glob semantics are a contract.** Document the pattern grammar in docs/07 §6/§10 and test it; `*` meaning one segment is the only rule that keeps the answer explainable to an administrator.
- **Expand-then-contract on `scope_type`.** Ship the wider check as `not valid`, backfill the new columns, then validate; keep the old shape accepted until one release later if any read path still assumes three levels. Do not drop `user_id` in this migration — REQ-006 keeps writing it for one release.
- **Performance.** Resource resolution on a page list is per-row work; benchmark against a seeded 5k-binding, 20k-page organization and keep list latency inside the existing request budget, otherwise fall back to the derived cache with event-driven invalidation.
- **No invented scope data.** The explorer shows counts computed from live bindings; a node with no bindings shows the `EmptyState`, never a placeholder.
- **Interaction with ABAC (REQ-069).** Scope resolves first and ABAC second; the simulator (REQ-074) must show the scope step before the policy step, and a policy that could be mistaken for a scope rule is a documentation item, not a runtime special case.
