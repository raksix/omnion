# REQ-095 — Workflow Versioning, Sharing & Permissions

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows` + `crates/permissions`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Team-safe automation.

- Workflow versions: draft/published, version history, diff between versions, restore.
- Per-workflow permissions (view/run/edit) and folder-level organisation with tags.
- Sharing: transfer ownership, grant a team run-only access, publish to the org library.
- Sub-workflow library: call a workflow from another with typed inputs/outputs.
- Audit of who ran/changed what, and a lock while someone is editing.

## Implementation spec

### Scope (in / out)

**In**

- **Draft and published states** — a workflow has one mutable **draft** and any number of immutable **versions**. Triggers, schedules and webhook calls always execute the **published** version; a manual run may explicitly run the draft (labelled as such in the trace) for testing. Publishing validates first (REQ-092 validate endpoint) and records what the version needs (required permissions, credential kinds, outbound hosts) so a warning can name what the publisher or the organization is missing.
- **Version history** — list of versions with number, actor, note, node count, checksum and publish time; a **diff** between any two versions rendered as a change list (nodes added, removed, renamed, parameter changes with old and new values, edge changes) plus a raw JSON view; **restore** copies an older version's content into the draft (or publishes it as a new version) — history is never rewritten.
- **Editing lock** — a soft lock on the draft: acquiring it shows a collaborator banner to everyone else, holders heartbeat and the lock expires (default 15 minutes); another editor may take over with a reason, which is audited and announced. The lock is advisory, not a security control.
- **Folders and tags** — an organization-scoped folder tree (create, rename, move, archive, path uniqueness) with workflows placed in a folder, plus tags for cross-cutting grouping. Lists filter by folder subtree and by tag.
- **Per-workflow permissions** — explicit grants on a workflow, on a folder, or at organization level, with roles `view`, `run`, `edit`, `manage`. Access is the union of grants that apply to the caller; for the same subject a more specific scope wins over a broader one, and holding `workflows.manage` organization-wide (REQ-006) grants manage everywhere. Grants carry an optional expiry and are revocable.
- **Sharing flows** — a Share dialog (subject search, role, optional expiry, inheritance shown as "via folder …"), team run-only access for operational handover, **transfer ownership** (owner confirms, requires manage on both sides and is audited), and **publish to the org library**: a visibility flag that lists a workflow for everyone with read access, granting no rights beyond the organization default.
- **Sub-workflow library** — a workflow can be marked **callable** with a typed contract: named inputs (type, required, default, description) and named outputs (bindings into the callee's node data). Callers pick it from the node picker, get save-time type checking (REQ-092), and runs record the parent/child link (REQ-093). Calls bind to the **published** version at call time and record the contract checksum; publishing a contract-breaking change lists the affected callers and notifies them.
- **Audit** — every definition change, publish, restore, grant, revocation, transfer, lock action and library change writes to the audit trail (REQ-039) with actor and a diff summary, and the workflow's Audit tab reads it back with filters.
- **Migration safety** — existing workflows are backfilled: version 1 is created from the current definition and published, so behaviour does not change on upgrade.

**Out**

- Template catalogue and bundles — REQ-094.
- Organization-wide role and team administration — REQ-006 owns teams; grants accept a `team` subject from day one so no migration churn is needed when the team entity lands.
- Engine execution semantics, waits and approvals — REQ-091 and REQ-090.
- Marketplace listing rules and revenue — REQ-048 (the org library here is internal visibility only).

### Screens (UI)

- **`/automations`** — folder tree rail (with counts and archived section), tag filter chips, and new columns: Folder, Visibility (private, folder, library), Versions, Owner, Lock state. Bulk actions: Move to folder, Add tag, Archive, Export.
- **`/automations/[id]` (editor)** — header shows Draft or the published version number with a switch and a "Run draft" affordance that labels the run; unpublishing is impossible without publishing a replacement. Tabs: **Versions** (list, notes, diff to any other version, Restore, Publish), **Share** (grants table with subject, role, scope, expiry, Revoke; inheritance shown), **Audit** (actor, action, diff summary, time; filters), **Settings** (owner transfer, library visibility, callable contract editor, folder, tags, archive, delete). A lock banner names the current editor with Take over.
- **`/automations/[id]/versions/{n}`** — read-only version view with the graph, the node list, and a diff panel against a chosen other version, plus Restore and Publish actions that respect permissions.
- **`/automations/library`** — published workflows visible to the organization: name, owner, folder, tags, callable contract summary, last published, and a Run-only action where the caller holds run access.
- **`/automations/folders`** — manage the tree: create, rename, move, archive, restore, and a Permissions tab per folder with the same grant editor (inheriting down).
- **Sub-workflow picker (canvas)** — the call-workflow node lists callable workflows with their contracts, input mapping with types, and a contract-change warning when the callee's published version differs from the one recorded.
- **States** — empty, loading and error states; a workflow with no versions (pre-upgrade edge case) says so with a Publish action; lock and permission-degraded states render as inline banners, never as silent disabled buttons.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflows/{id}/versions` | Version history | `workflows.read` |
| POST | `/api/v1/workflows/{id}/versions` | Save the draft as a version | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/versions/{version}` | One version's definition | `workflows.read` |
| GET | `/api/v1/workflows/{id}/versions/diff?from=&to=` | Change list plus raw definition delta | `workflows.read` |
| POST | `/api/v1/workflows/{id}/versions/{version}/restore` | Restore content into the draft | `workflows.manage` |
| POST | `/api/v1/workflows/{id}/publish` | Publish a version after validation | `workflows.publish` |
| POST | `/api/v1/workflows/{id}/unpublish` | Move to draft-only (runs stop firing) | `workflows.publish` |
| POST | `/api/v1/workflows/{id}/lock` | Acquire, renew or take over the draft lock | `workflows.manage` |
| DELETE | `/api/v1/workflows/{id}/lock` | Release the draft lock | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/grants` | Grants that apply to this workflow | `workflows.read` |
| POST/PATCH/DELETE | `/api/v1/workflows/{id}/grants/{grant_id}` | Create, change role or expiry, revoke | `workflows.share` |
| POST | `/api/v1/workflows/{id}/transfer` | Transfer ownership | `workflows.share` |
| POST | `/api/v1/workflows/{id}/library` | Publish or remove from the org library | `workflows.publish` |
| GET | `/api/v1/workflows/library?folder=&tag=&q=` | Org library listing | `workflows.read` |
| GET/POST | `/api/v1/workflow-folders` | Folder tree, create | `workflows.read` / `workflows.manage` |
| PATCH/DELETE | `/api/v1/workflow-folders/{id}` | Rename, move, archive, restore | `workflows.manage` |
| GET/POST/DELETE | `/api/v1/workflow-tags` | Tag vocabulary and management | `workflows.read` / `workflows.manage` |
| GET/PUT | `/api/v1/workflows/{id}/callable` | Read or set the callable contract | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/callers` | Workflows calling this one, with versions | `workflows.read` |

Two new permission keys: `workflows.publish` (publishing, restoring, library visibility) and `workflows.share` (grants, revocations, ownership transfer). Publishing is deliberately separate from editing.

### Data model

Migration `database/migrations/0019_workflow_versions.sql` (next free number if taken).

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_versions` | id uuid pk, workflow_id uuid → workflows cascade, version int, definition jsonb, notes text, status text ('draft','published','archived'), checksum text, requires jsonb, created_by uuid → users set null, created_at, published_by uuid → users set null, published_at | unique `(workflow_id, version)`; index `(workflow_id, published_at desc)`; check `(status = 'published') = (published_at is not null)` |
| `workflow_folders` | id uuid pk, organization_id uuid → organizations cascade, parent_id uuid → workflow_folders set null, name text, path text, archived boolean default false, created_at, updated_at | unique `(organization_id, path)`; index `(organization_id, parent_id)`; check `length(btrim(name)) > 0`; `path` is the canonical slash path, maintained on move |
| `workflow_tags` | id uuid pk, organization_id uuid cascade, name text, created_at | unique `(organization_id, lower(name))` |
| `workflow_tag_links` | workflow_id uuid → workflows cascade, tag_id uuid → workflow_tags cascade | pk `(workflow_id, tag_id)`; index `(tag_id)` |
| `workflow_grants` | id uuid pk, workflow_id uuid → workflows cascade, folder_id uuid → workflow_folders cascade, subject_kind text ('user','team','organization'), subject_id uuid, role text ('view','run','edit','manage'), granted_by uuid → users set null, created_at, expires_at | check exactly one of workflow_id, folder_id is set; unique `(coalesce(workflow_id, folder_id), subject_kind, coalesce(subject_id, all-zero uuid))`; index on the workflow and folder columns |
| `workflow_locks` | workflow_id uuid pk → workflows cascade, holder_id uuid → users cascade, acquired_at, heartbeat_at, expires_at, note text | index `(expires_at)` for lock cleanup |
| `workflow_calls` | id uuid pk, caller_workflow_id uuid → workflows cascade, caller_node_id text, callee_workflow_id uuid → workflows cascade, callee_version int, contract_checksum text, created_at | unique `(caller_workflow_id, caller_node_id)`; index `(callee_workflow_id)` |

Adds to `workflows`: `published_version_id uuid → workflow_versions set null`, `folder_id uuid → workflow_folders set null`, `owner_id uuid → users set null`, `library_visible boolean default false`, `callable boolean default false`, `callable_contract jsonb`. The same migration backfills version 1 (published) for every existing workflow and sets `owner_id` from `created_by` where possible.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.version.created` | emitted | number, actor, node count |
| `workflow.version.published` | emitted | number, actor, requires summary |
| `workflow.version.restored` | emitted | source version, actor |
| `workflow.lock.acquired` / `.released` / `.expired` / `.taken_over` | emitted | holder, reason for takeover |
| `workflow.grant.created` / `.revoked` / `.expired` | emitted | subject kind and role, never subject personal data beyond the id |
| `workflow.ownership.transferred` | emitted | previous and new owner |
| `workflow.library.published` / `.unpublished` | emitted | visibility change only |
| `workflow.contract.changed` | emitted | callee, old and new checksum, caller count |
| `workflow.call.broken` | emitted | a caller references a contract that no longer matches |
| `workflow.execution.started` | emitted | now carries the version id it ran |

### Acceptance criteria

- [ ] Existing workflows are backfilled with a published version 1 whose content matches the pre-upgrade definition, and none of them changes behaviour after the migration.
- [ ] Triggers and schedules run the published version; editing the draft changes nothing until publish, and a "Run draft" run is labelled as such in the trace.
- [ ] Publishing runs validation and refuses with readable problems; a publish that needs a permission or credential the publisher lacks warns with the specific item.
- [ ] History lists every version with actor, note and checksum; the diff action renders node and parameter changes with old and new values for two non-adjacent versions.
- [ ] Restore brings an older version's content into the draft without rewriting history, and publishing the restored content creates a new version number.
- [ ] The lock banner appears for a second editor, Take over requires a reason, and the lock expires after its window with the expiry visible in the audit tab.
- [ ] Folder create, rename, move and archive work with path uniqueness enforced, and moving a workflow to a folder changes its effective access per the grant rules.
- [ ] A run-only grant lets a user run and read results but not edit, publish, or read the definition — verified with a real second account.
- [ ] A folder grant applies to its contents and the Share dialog states the inheritance explicitly.
- [ ] Revoking a grant takes effect on the next request, not after a restart, and an expired grant stops applying without manual cleanup.
- [ ] Transfer ownership requires confirmation on both sides, updates the owner shown in lists, and is audited with the previous owner.
- [ ] Publishing to the org library lists the workflow for readers and grants no run or edit rights by itself.
- [ ] A callable workflow exposes its typed contract in the picker; a caller with a mismatched input type is blocked at save with the field named.
- [ ] A call records the callee's published version and contract checksum; publishing a contract-breaking change lists affected callers and emits the notification event.
- [ ] Sub-workflow runs link parent and child executions, and the child's trace is reachable from the parent.
- [ ] Every publish, restore, grant change, transfer and lock takeover appears in the Audit tab with actor and diff summary.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the grant-precedence and contract-check tests fail when their logic is removed.

### QA plan

The walkthrough must: as the owner, create a folder and a tag, move a workflow in, edit the draft, publish version 2, view the diff against version 1, restore version 1, and publish again to see version 3; open the editor in a second session as another user and read the lock banner, take over with a reason, and find the takeover in the audit tab; grant view, then run, then edit to a second account and prove each level by attempting the actions; grant a team a folder role and confirm inheritance; revoke and confirm the immediate effect; transfer ownership and confirm both confirmations; publish a workflow to the library and open it from `/automations/library` as a third reader account; mark a workflow callable, call it from another with typed inputs, then publish a contract-breaking change and follow the caller warning; finally open the parent run and reach the child trace.

The visual check must see: the diff view readable at 1280px with old and new values distinguishable without colour alone, the folder rail not truncating long names, grant rows showing scope and expiry, and the lock banner not covering the primary save action.

### Slices

1. **Versions, diff, restore, lock** — `workflow_versions` with backfill, publish/validate, version list and diff screens, restore, draft lock with takeover and heartbeat expiry, audit entries.
   *Done when:* publish, diff, restore and lock are exercised end to end, and the backfill keeps a pre-upgrade workflow running unchanged.
2. **Folders, tags and drafts** — folder tree with paths and moves, tags and tag filters, list columns and bulk move/tag/archive.
   *Done when:* the tree round-trips through move and archive and lists filter by subtree and tag.
3. **Grants, sharing and library** — grant model and precedence, Share dialog, folder inheritance, expiry, transfer of ownership, org library visibility and listing.
   *Done when:* a run-only account provably cannot read the definition and a folder grant reaches its contents.
4. **Callable contracts** — contract editor, picker integration with type checking, `workflow_calls` records, contract-change detection and caller notification, parent/child trace links.
   *Done when:* a contract-breaking publish lists every caller and a caller run links to its child execution.

### Risks / notes

- Version history is the trust anchor: never mutate a version row; restore and update always create new versions.
- The lock is advisory — do not build correctness on it. Conflicts resolve through the draft/publish model and the diff view.
- Grant precedence must live in one function used by every route; two implementations of "can this user run it" is how permission holes appear.
- Publishing must snapshot what a version needs so a downgrade in the organization's modules or permissions is caught before, not during, a run.
- Callable contracts must keep type checking at save time; a runtime-only check turns a design mistake into a production incident.
- The backfill is risky on a busy install: run it in the migration with a bounded statement, and verify by comparing a sample of pre- and post-migration definitions.
