# REQ-067 — Role Management UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/permissions`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The screen that makes the permission system usable.

- Role registry list: name, description, colour, icon, member count, scope badges, search.
- Create/edit role form with a grouped permission picker (allow / deny toggles, module groups).
- Preset roles shipped: Super Admin, Editor, SEO Manager, Support, Viewer, Employee.
- Role hierarchy: drag-order priority, hierarchy-protected changes (cannot outrank yourself).
- Role inheritance (derived roles extend a parent), multi-level chains, effective diff view.
- Role versioning and audit timeline: who changed what, diff between versions.
- Deletion guards: self-lockout prevention, owner/administrator existence invariants.

## Implementation spec

### Scope (in / out)

**In**
- Role registry screen: name, key, type (system / custom), colour, icon, priority, member count, permission counts (allowed / denied), parent role, scope badges, updated-at; search, type filter, sortable columns, CSV export, and a real empty state with the primary action.
- Create and edit form: name, key (slug, immutable after first save), description, colour, icon, priority, parent role, and a grouped permission picker with tri-state cells (`allow` / `deny` / `inherit`), per-module accordions, search, counts in the header (`allowed N · denied M · inherited K`), grant-all / clear-group, and an atomic save with a diff preview of exactly what changes.
- Preset roles as templates: Super Admin, Editor, SEO Manager, Support, Viewer, Employee — each instantiates as a normal custom role the customer then owns; the base ladder (Owner, Administrator, Manager, Moderator, Editor, Member) stays system and cannot be edited into a template.
- Role hierarchy: drag-order priority writing stable 100-step gaps, priority input `0–1000`, and hierarchy-protected changes — an actor can never set a role at or above their own highest priority, nor edit or delete a role above them; the refusal names the rule.
- Inheritance: single parent per role, multi-level chains rendered as a chain view with descendants, cycle and self-inheritance refused with a field error, inheritance toggle (`inherit_permissions`) kept separate from the link, and an **effective diff** view showing what the child adds or removes against the resolved parent set.
- Role versioning: every save writes a version with a snapshot; the History tab shows a timeline (who, when, what changed) and a side-by-side diff between any two versions; restore writes a new version rather than rewriting history.
- Deletion and mutation guards: system roles cannot be deleted; a role referenced by bindings, child roles or templates returns `409` with the referencing counts; the Owner role's identity fields are protected; at least one Owner binding and one Administrator binding must survive every change, checked in the same transaction; self-lockout (removing the caller's last management source) is refused with the invariant named.
- Audit: every role, priority, inheritance and permission change lands in the audit log with a field-level before/after diff.

**Out**
- The permission vocabulary itself and the effective-permissions viewer (REQ-068); ABAC policies (REQ-069); bindings for groups and service accounts as first-class screens (REQ-071, REQ-072); the interactive simulator (REQ-074).
- A second editor for the same state: the matrix in the IAM core screens and this screen read and write the same endpoints, never parallel ones.
- Template authoring or role exports beyond duplicate and CSV export.

### Screens (UI)

Nav: **Settings → IAM → Roles**.

| Route | Screen |
|---|---|
| `/settings/iam/roles` | Registry list with search, filters, drag-order toggle |
| `/settings/iam/roles/new` | Create dialog/page with preset picker and the grouped picker |
| `/settings/iam/roles/{id}` | Tabs Permissions · Members · Inheritance · History |
| `/settings/iam/roles/{id}/diff?from=&to=` | Version diff (also reachable from History) |

- **List.** Columns: Name (colour dot + icon + link, `system` badge for base roles), Key, Priority, Members, Permissions (`allowed N · denied M`), Inherits, Updated. Filters: type, has-members, scope, search (250 ms debounce). Sort by name / priority / members / updated. Toolbar: `New role`, `Reorder`, `Export CSV`. Row actions: Edit, Duplicate, Delete (opens the blocker dialog when refused). Reorder mode adds drag handles, disables sorting, and shows a `Save order` / `Cancel` footer; the server rejects any move that would place a role at or above the caller's own priority, marking the row and naming the rule.
- **Create.** Preset cards (Super Admin, Editor, SEO Manager, Support, Viewer, Employee) above `Start from scratch`; picking a preset pre-fills the picker with that template's allow/deny set and a parent role suggestion, and instantiates on save as a custom role with the customer's own key.
- **Permissions tab.** Left rail: module groups (content, media, users, plugins, deployment, iam, tenancy, webhooks, events, ai, search, workflows, audit, and any module added later) with counts; main pane: one row per permission key with description and a tri-state segmented control. Header: search, `Grant all`, `Clear group`, allowed/denied/inherited counters. Sticky footer: `Save` (disabled until dirty), `Discard`, `Preview diff` (opens the add/remove diff). Validation: unknown key, duplicate entry, priority outside `0–1000`, inheritance cycle, and a parent that is the role itself are refused field-by-field; a failed save changes nothing.
- **Members tab.** Bindings for this role: Subject (user / group / service account), Scope chip (global, organization, site, department, module, resource), Granted by, Expires; filters and a `member count` header. `Add member` opens the binding dialog (subject picker, scope picker, optional expiry) and writes through the shared bindings endpoint.
- **Inheritance tab.** Parent picker (search, excludes self and descendants), current chain rendered as a vertical ladder with the resolved priority order, descendants list with `Reparent` shortcuts, the `inherit permissions` switch, and the effective diff panel: `inherited N`, `adds +K` in green, `removes −K` in red, each line with the source role named.
- **History tab.** Timeline newest-first: version number, actor, timestamp, change kind, changed field chips; `View` opens the version snapshot; `Compare` opens the side-by-side diff (added green, removed red, changed amber) with `Restore this version` (writes a new version and requires confirmation naming what will change). Entries deep-link to the audit log entry.
- **States, keys, mobile.** Skeleton rows, empty states per tab (`No members yet` with `Add member`), error strip with retry; the picker keeps unsaved state in a draft that survives a palette open. Keys: `/` search, `j`/`k` rows, `enter` open, `e` edit, `space` cycles the focused picker cell, `g r` returns to the list, `Esc` closes. Below `lg` tables become cards, the picker becomes one accordion per module with the same tri-state control, and the sticky footer stays visible.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/iam/roles` | List with counts and filters · create a role | `iam.roles.read` · `iam.roles.manage` |
| GET · PATCH · DELETE | `/api/v1/iam/roles/{id}` | Detail with chain and counts · update fields · delete while unreferenced | `iam.roles.read` · `iam.roles.manage` |
| PUT · POST | `/api/v1/iam/roles/{id}/permissions` · `/permissions/preview` | Replace the allow/deny set atomically · diff preview | `iam.roles.manage` |
| PATCH | `/api/v1/iam/roles/order` | Batch priority write from drag order (hierarchy-checked) | `iam.roles.manage` |
| POST | `/api/v1/iam/roles/{id}/duplicate` | Clone a role as a draft | `iam.roles.manage` |
| GET | `/api/v1/iam/roles/{id}/versions` (+`/{version}`, `/diff?from=&to=`) | History timeline · snapshot · diff | `iam.roles.read` |
| POST | `/api/v1/iam/roles/{id}/restore` | Restore a version as a new version | `iam.roles.manage` |
| GET | `/api/v1/iam/roles/{id}/references` | Bindings, children and template references (blocker pre-check) | `iam.roles.read` |
| GET | `/api/v1/iam/role-templates` · POST `/role-templates/{key}/instantiate` | Preset list · instantiate a preset into a custom role | `iam.roles.read` · `iam.roles.manage` |
| GET | `/api/v1/iam/invariants` | Owner/administrator existence state per organization (banner) | `iam.roles.read` |

`DELETE` answers `409` with a `blockers[]` body (bindings N, child roles M, template reference) rather than a bare refusal. Every mutation re-checks the actor's own highest priority server-side — the UI restriction is a convenience, never the control.

### Data model

Migration: `0120_role_management.sql` (reserved band 0116–0125 for the identity & access wave; the ledger is append-only — take the next free number if taken). The base `roles`, `role_permissions` and `role_bindings` tables land with `0002_iam.sql`; this migration is additive.

- `roles` += `color` text null check (`^#[0-9a-f]{6}$`), `icon` text null (icon-name from the shared icon set), `updated_by` uuid null → users on delete set null, `version` int not null default 1; index (organization_id, priority desc, name).
- `role_versions` (id uuid pk, role_id uuid → roles on delete cascade, version int, change_kind text in ('created','updated','permissions','priority','inheritance','restored'), changed_fields text[] default '{}', snapshot jsonb not null, permissions jsonb not null, actor_user_id uuid null → users on delete set null, created_at timestamptz) — unique (role_id, version); index (role_id, created_at desc).
- `role_templates` (key text pk, name text, description text, color text, icon text, priority_hint int, parent_key text null, permissions jsonb not null — key → `allow`/`deny` map, position int) — seeded with Super Admin, Editor, SEO Manager, Support, Viewer and Employee; templates are read-only at runtime.
- `roles` writes: a `BEFORE UPDATE` trigger or the application transaction writes a `role_versions` row for every change to fields, permissions, priority or inheritance — the version row and the change commit together or not at all.
- Index (inherits_role_id) exists from `0002`; add index (organization_id, is_system) for the list filter.
- Invariant checks run inside the same transaction as the mutation: `count(role_bindings)` per organization for the owner role must stay ≥ 1, and the administrator role likewise; the self-lockout check compares the caller's set before and after.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `iam.role_created` · `iam.role_updated` · `iam.role_deleted` | Lifecycle | `role_id`, `key`, `organization_id`, changed field names |
| `iam.role_permissions_changed` | A permissions save committed | `role_id`, `allowed_delta`, `denied_delta`, `actor_user_id` |
| `iam.role_priority_changed` | Drag order or priority edit | `role_id`, `from`, `to` |
| `iam.role_inheritance_changed` | Parent set, cleared or inherit switched | `role_id`, `parent_role_id`, `inherit_permissions` |
| `iam.role_version_created` · `iam.role_restored` | Versioning | `role_id`, `version`, `restored_from` |
| `iam.role_template_instantiated` | A preset became a custom role | `template_key`, `role_id` |
| `iam.role_guard_blocked` | A guard refused a change | `role_id`, `guard` code (`self_lockout`, `hierarchy`, `owner_invariant`, `referenced`) |

Consumed: `iam.binding_created` / `iam.binding_revoked` refresh member counts; `iam.permissions_catalogue_updated` (REQ-068) re-validates stored permission keys and surfaces any that left the catalogue. Webhook relevance: security automations may react to `iam.role_permissions_changed`; payloads carry ids, counts and field names — never permission descriptions or user lists.

### Acceptance criteria

- [ ] `0120` applies on a fresh and on a populated database; existing roles and permissions are untouched; `cargo test --workspace` is green.
- [ ] The list shows real member and permission counts, and search, type filter and sorting narrow exactly the rendered rows.
- [ ] Creating a role from the Super Admin preset lands every template permission as an allow entry and the role is a customer-owned custom role, not a system one.
- [ ] The grouped picker cycles a cell `inherit → allow → deny → inherit`, header counts update live, and `Grant all` / `Clear group` affect only the selected group.
- [ ] Saving an unknown key, a duplicate entry, a priority outside `0–1000`, a self-parent or a cycle is refused with a field-level error and no partial write.
- [ ] The preview diff lists exactly the entries the save then applies.
- [ ] Drag reorder persists across reload; the server refuses a move that would place a role at or above the caller's own priority and the UI marks the row with the rule named.
- [ ] A Moderator cannot edit, delete or reposition a role above their own priority — the API answers 403 with the hierarchy reason even when the UI is bypassed.
- [ ] Inheritance chains render to their full depth; setting a cycle is refused; the effective diff shows inherited, added and removed keys with the source role named.
- [ ] Every save writes exactly one version row; the History timeline shows actor, time and changed fields; the diff of v(n-1) → v(n) matches the change made.
- [ ] Restore writes a new version that reproduces the chosen snapshot, and the timeline shows it as a restore.
- [ ] Deleting a role bound to a user is refused with the binding count; deleting a role with children is refused until they are reparented; deleting a system role is refused.
- [ ] Removing the last Owner or Administrator binding is refused in the same transaction as the change, naming the invariant, and `GET /iam/invariants` reflects the state in the banner.
- [ ] Removing the caller's own last role that carries `iam.roles.manage` is refused with the self-lockout message.
- [ ] Duplicate produces an independent role whose later edits do not affect the source.
- [ ] Every new screen renders at 390 px without horizontal scroll, the picker's unsaved state survives a palette open, and the walkthrough reports zero high findings.

### QA plan

The walkthrough must click through `/settings/iam/roles` (search, filter, `New role` from the Editor preset, rename it, set colour and icon), the Permissions tab (cycle three cells through all three states, check the counters, `Preview diff`, save, reopen and verify persistence), the Inheritance tab (set a parent, switch inheritance off and on, read the effective diff), reorder mode (drag a role and read the refusal when it would outrank the actor), the History tab (open the v1 → v2 diff, restore v1 and confirm the new version), and the delete path twice — refused with a blocker list for a bound role, successful for an unbound one. Visual check: colour dots and icons render, system roles are visually distinct from custom ones, the tri-state control is legible at AA contrast with an unambiguous deny state, the diff shows green/red/amber rows, drag handles are visible only in reorder mode, and screenshots `page-iam-roles`, `page-iam-role-picker`, `page-iam-role-inheritance`, `page-iam-role-history`, `mobile-iam-roles` are produced.

### Slices

1. **Registry and picker.** Migration fields, list with counts and filters, create/edit form with the grouped tri-state picker, preset instantiation, atomic save with preview diff and full validation. *Done when:* acceptance 1–6 pass and the registry plus picker are in the walkthrough inventory.
2. **Hierarchy, inheritance, guards.** Drag-order endpoint with server-side hierarchy enforcement, chain and descendant views, effective diff, deletion blocker dialog, owner/administrator invariants and self-lockout. *Done when:* acceptance 7–9 and 12–14 pass, including a bypass attempt refused by the API.
3. **Versioning and history.** `role_versions` writes on every change, History timeline, snapshot view, side-by-side diff, restore, and audit entries with field-level diffs. *Done when:* acceptance 10–11 pass and a restore round-trip is exercised in the walkthrough.
4. **Templates and polish.** Preset rows seeded, duplicate, CSV export, keyboard map, mobile cards, empty/loading/error states on every tab, and the member-count listeners. *Done when:* acceptance 15–16 pass and the mobile pass shows zero high findings.

### Risks / notes

- **One editor, one state.** This screen, the IAM core matrix and the API all write the same endpoints; a second write path is a release blocker because the versions and audit would silently diverge.
- **Priority is security.** The hierarchy rule is enforced in the database transaction, not in the component; any request that could move a role above the actor must fail with the rule named.
- **Versions are append-only.** Restore never deletes a version; history must remain a complete record even after a restore, or the audit trail becomes a story of convenience.
- **Permission keys are catalogue-bound.** A key leaving the catalogue (plugin removed, module renamed) must be surfaced on the role, not dropped silently — the role keeps the entry and the UI flags it for review (REQ-068 owns the check).
- **Member counts on large organizations** need an indexed count path; loading a count per role in a loop will not survive a 5k-binding tenant.
- **Preset expectations.** Presets are starting points documented as editable copies; naming a preset after a system role would imply an authority it does not have.
- **Sync by design.** The effective diff view and the roles' resolved sets must come from the same resolver (REQ-068) — computing "effective" twice is how screens start disagreeing.
- **Accessibility of the tri-state control** (three states in one cell) needs a keyboard cycle and an announced label; it fails silently for screen-reader users otherwise.
