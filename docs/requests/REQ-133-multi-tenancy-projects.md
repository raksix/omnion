# REQ-133 — Projects (Shared Automation & Credentials)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Organizing work for teams inside one installation.

- Project entity: a container for workflows, credentials and folders with its own members.
- Shared workflows and credentials inside a project; isolation between projects.
- Move a workflow between projects with dependency checks.
- Per-project usage limits and audit; project switcher in the automation screens.
- Delegated administration: a project owner manages their project without instance-wide rights.

## Implementation spec

> **Band:** migration `0133` reserved; append-only ledger — take the next free number if taken · **Layer:** core (`crates/workflows`, `crates/secrets`) + admin (`apps/admin/app/(automation)/automation/projects`) · **Disambiguation:** this is the automation container, not the delivery-tracking module of REQ-056 (`modules/projects`) — different navigation section, different tables, no shared identifiers, and neither references the other's records.

### Scope (in / out)

**In**

- Project entity: key (2–8 uppercase letters, unique per organization), name, description, colour and icon, owner, members, status (`active`, `archived`). A project lives inside one organization (REQ-005) and never spans organizations.
- Default project: every organization has one protected default project; all pre-existing workflows, credentials, folders, schedules and executions are backfilled into it during the migration, and any resource created without an explicit project lands there too. Nothing existing breaks and no code path deals with a nullable project.
- Container semantics: workflows, credentials, workflow folders, schedules and execution records carry `project_id`. The automation screens gain a project switcher in the header; lists, search and empty states all reflect the active project, and a breadcrumb keeps it visible.
- Isolation: listings, search and global views are project-scoped by default; a resource in another project returns `404` — not `403` — unless the caller holds an instance-wide permission. A workflow that references a credential from another project is refused at save time with the dependency named, so cross-project references are impossible rather than merely discouraged.
- Members and roles: `owner`, `editor`, `operator`, `viewer` plus instance administrators. The capability matrix (view, edit workflows, run and retry, manage credentials, manage members, manage limits) is defined once and shared by API and UI. At least one owner must remain, enforced on member removal and user deactivation alike.
- Delegated administration: a project owner manages their project's members, workflows, credentials and limits through the same API the panel uses. Instance-wide surfaces — users, settings, licence, other projects, instance audit — stay out of reach, and the boundary is enforced by the API, never by hiding buttons.
- Moving a workflow: `Move to project…` runs a dependency check over credentials used, sub-workflows, callers, schedules and triggers, inbound webhook subscriptions, published API routes and workflow templates. A dry-run report groups what would break; a dependency resident outside the target project refuses the move; the move itself is transactional, pauses and resumes the scheduler around the switch, and writes an audit row.
- Limits and usage: per-project maxima for workflows, credentials, runs per day and concurrent runs, defaulted from instance settings with per-project overrides. Soft warning at 80 percent, hard refusal at 100 percent with a message naming the limit and the project owner. Daily usage counters (runs, failures, compute time) appear on the project screen and export as CSV.
- Audit: every project-scoped mutation writes the audit row with its `project_id`; the project audit screen is that stream filtered, and the instance-wide audit gains a project column and filter.
- Archiving: archived projects are read-only — no new runs, no edits — keep their history, and can be restored or exported. Deletion requires a typed confirmation, a dependency check and an export-first hint.

**Out**

- Delivery task tracking (REQ-056), organizations and tenancy (REQ-005), per-site operations (REQ-120), and workflow versioning or sharing semantics (REQ-095).
- Cross-project resource sharing, nested projects, project hierarchies, project templates and per-project branding.
- Per-project billing: usage is metered for limits and visibility, never invoiced; instance billing stays where it lives today.

### Screens (UI)

Routes under the automation section (`apps/admin/app/(automation)/automation/projects/*`):

| Route | Screen |
|---|---|
| `/automation/projects` | Project list: key, name, members, workflows, runs (7d), limit usage, status |
| `/automation/projects/{id}` | Overview: counts, usage sparkline, recent runs, member summary |
| `/automation/projects/{id}/members` | Member table with roles, add and remove, ownership transfer |
| `/automation/projects/{id}/limits` | Overrides, current usage bars, threshold setting |
| `/automation/projects/{id}/audit` | Project-scoped audit stream with filters and CSV export |
| `/automation/projects/{id}/settings` | Name, key, description, colour, archive, export, delete |
| Automation header (all screens) | Project switcher: search, recents, `All projects` for instance admins only |

- **List screen.** Columns Key, Name, Members (avatars plus count), Workflows, Runs 7d (with failure rate), Limits (the highest usage bar), Status. Filters for search, status and member; create flow asks for key, name and owner and applies a sensible default limit set; row menu has Open, Archive, Transfer ownership and Delete. Instance admins additionally see an `Unassigned` counter, which must read zero after the backfill — a visible integrity check.
- **Switcher.** `p` opens it, typing filters, recents first; the selection persists per user and is written into URLs (`?project=` on lists, path segment on section routes) so shared links reproduce a view. When a deep link points to a resource outside the selected project, a banner explains it and offers `Switch project`.
- **Members.** Table of Name, Email, Role, Added, Last active in project. Role changes are inline with capability tooltips; adding searches instance users only; removal warns when the member owns workflows and offers reassignment to another member before proceeding. Ownership transfer is separate, requires two confirmations and writes an audit row — project ownership and workflow ownership are different things and the UI says so.
- **Limits.** Usage bars for runs today (with reset time in the project timezone), concurrent runs now, workflows and credentials; override editors show the instance default as a placeholder; an 80 percent crossing renders amber with a link to the owner.
- **Move dialog.** Target project picker (permission aware), a dependency report grouped by kind with per-item resolution hints (`Move credential first`, `Detach caller`), `Dry run` and `Move`. The move runs in a transaction; schedules pause before the switch and resume after.
- **States and mobile.** Skeletons, empty states with the primary action, error strips with retry; archived projects render read-only with a restore action; at 390 px tables become cards and the switcher becomes a sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/projects` | List projects the caller may see · create | `projects.read` · `projects.manage` |
| GET · PUT | `/api/v1/projects/{id}` | Read · update | `projects.read` · `projects.manage` |
| POST | `/api/v1/projects/{id}/archive` · `/restore` | Archive · restore | `projects.manage` |
| DELETE | `/api/v1/projects/{id}` | Delete (typed confirmation, dependency check) | `projects.manage` |
| GET · POST · PATCH · DELETE | `/api/v1/projects/{id}/members` | Member CRUD with roles | `projects.members.manage` (read: `projects.read`) |
| POST | `/api/v1/projects/{id}/transfer-ownership` | Transfer project ownership | `projects.manage` |
| GET · PUT | `/api/v1/projects/{id}/limits` | Read · override limits | `projects.limits.manage` |
| GET | `/api/v1/projects/{id}/usage` | Daily usage series and current counters | `projects.read` |
| GET | `/api/v1/projects/{id}/audit` | Project-scoped audit | `projects.audit.read` |
| POST | `/api/v1/workflows/{id}/move` | Move a workflow (`dry_run` supported) | `workflows.move` plus project management on both ends |
| GET | `/api/v1/projects?mine=1` | Projects the caller belongs to (switcher) | `projects.read` |
| GET | `/api/v1/workflows` · `/api/v1/credentials` · `/api/v1/workflow-folders` | Existing lists gain a `project_id` filter and scoped defaults | existing permissions |

Creating without `project_id` writes into the default project. The API never accepts a project the caller cannot see; management attempts return `403`, reads of invisible resources return `404`, and messages never reveal the existence of a foreign project. Scheduling and execution endpoints re-check project status, so an archived project refuses new runs at the source rather than relying on the UI.

### Data model

Migration `0133_automation_projects.sql`.

```sql
automation_projects (id uuid pk, organization_id uuid -> organizations, key text, name text, description text, color text, icon text,
  is_default bool default false, status text default 'active' in ('active','archived'), owner_user_id uuid -> users,
  created_by uuid -> users, created_at/updated_at)  unique (organization_id, key),
  unique (organization_id) where is_default
automation_project_members (project_id uuid -> automation_projects on delete cascade, user_id uuid -> users on delete cascade,
  role text in ('owner','editor','operator','viewer'), added_by uuid -> users, created_at timestamptz)
  pk (project_id, user_id), index (user_id)
automation_project_limits (project_id uuid pk -> automation_projects on delete cascade, max_workflows int null, max_credentials int null,
  max_runs_per_day int null, max_concurrent_runs int null, warn_threshold int default 80, updated_by uuid -> users, updated_at timestamptz)
automation_project_usage_daily (project_id uuid -> automation_projects, day date, runs int default 0, failed_runs int default 0,
  compute_ms bigint default 0, workflow_count int, credential_count int)  pk (project_id, day), index (day)
-- existing tables gain a project_id column, backfilled and set not null:
--   workflows, credentials, workflow_folders, schedules, executions  (uuid -> automation_projects on delete restrict)
--   indexes: (project_id, updated_at desc) on workflows and credentials, (project_id, started_at desc) on executions
-- audit_logs gains project_id uuid null with index (project_id, created_at desc)
```

Notes: the migration creates the default project per organization first, backfills existing rows in batches, and only then applies the `not null` constraint — the same order runs in tests against a seeded database. Foreign keys from resources to projects use `on delete restrict`, so a project with dependencies cannot disappear silently; deletion requires the dependency check and a transfer of or deletion of the resources. The ledger is append-only (REQ-129); a redelivered migration fails checksum verification rather than re-running.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `automation.project.created` · `.updated` · `.archived` · `.restored` · `.deleted` | Lifecycle | `project_id`, `key`, `actor_user_id` |
| `automation.project.member.added` · `.removed` · `.role_changed` | Membership changes | `project_id`, `user_id`, `role` |
| `automation.project.ownership_transferred` | Two-confirmation transfer | `project_id`, `from_user_id`, `to_user_id` |
| `automation.project.limit.warning` · `.limit_exceeded` | 80 and 100 percent crossings, edge-triggered once per limit per period | `project_id`, `limit`, `current`, `max` |
| `workflow.moved` | A workflow changes project | `workflow_id`, `from_project_id`, `to_project_id`, `actor_user_id` |

Consumed: `user.deactivated` blocks until owned projects and workflows are reassigned; `workflow.deleted` refreshes dependency counts; `organization.member.removed` (REQ-005) removes project memberships in the same transaction; `workflow.run.completed` increments usage counters through the same batching path that already records run history.

Webhook relevance: yes — limit and ownership events are what an operations team subscribes to, and membership events feed provisioning automations. Payloads carry ids, keys and roles only: never workflow definitions, never credential references, never user email addresses.

### Acceptance criteria

- [ ] A fresh installation and an upgraded installation both end with exactly one default project per organization and zero unassigned resources — the integrity counter reads zero.
- [ ] Any resource created without an explicit project lands in the default project and is visible in every existing list.
- [ ] The switcher filters workflows, credentials, folders, schedules and executions; the selection survives navigation and is encoded in URLs so a shared link reproduces the view.
- [ ] A workflow in project A cannot be opened, executed or exported by a member of project B: the read returns `404` and never discloses the project name.
- [ ] A credential from project A cannot be referenced by a workflow in project B — save-time validation names both resources and refuses the save.
- [ ] Global search (REQ-002) returns only resources the caller may see, scoped to their projects, verified with two accounts.
- [ ] `Move to project…` dry run lists every dependency kind correctly; the move is refused while a cross-project dependency exists and succeeds after it is resolved.
- [ ] A move pauses the schedule, keeps the run history attached to the workflow, and writes an audit row naming both projects.
- [ ] Runs per day and concurrent-run limits are enforced at the engine boundary: the 80 percent warning fires once and the 100 percent refusal messages name the limit and the owner.
- [ ] Usage counters match the underlying run records for a day, and the CSV export reproduces the on-screen series.
- [ ] A project owner can manage members and workflows through the API, and every instance-wide endpoint (users, settings, licence, other projects) returns `403` for them from direct calls, not just from the UI.
- [ ] Removing the last owner of a project is refused with a message naming the remedy; transferring ownership requires both confirmations and is audited.
- [ ] Deactivating a user who owns a project or workflows blocks until reassignment completes.
- [ ] Archived projects refuse new runs at the API boundary with a message explaining the state, and restore returns them to service without changing their schedules.
- [ ] Project member role changes take effect without re-login: the very next request reflects the new role, in both directions (grant and revoke).
- [ ] The admin screens render at 390 px without horizontal scroll and the walkthrough reports zero high findings.

### QA plan

Two sessions: one instance administrator and one project owner. Create a second project, add members through the UI, then verify scoping in list, search and global search; open a foreign workflow by URL and by direct API call, expecting `404` both times; try to save a workflow referencing a foreign credential and read the refusal; move a workflow that depends on a credential — first the refused path, then move the credential, then the successful path with schedule pause and resume and audit rows; drive the runs-per-day limit with a test workflow until the warning fires and then until refusal; change a member's role and confirm the next request obeys it; attempt to remove the last owner; run the ownership transfer; archive the project, attempt a run, restore it; check the project audit filters and CSV; verify the switcher persists across navigation; repeat key screens at 390 px. Visual check: the switcher is visible on every automation screen, member avatars render, the limits screen explains the instance default, and archived rows read as read-only rather than broken.

### Slices

1. **Entity, membership and backfill.** Migration with the default project, CRUD, member roles, the switcher surfaces and URL state. *Done when:* acceptance 1–3 and 15 pass.
2. **Isolation enforcement.** Project scoping across every automation query, `404` semantics, save-time cross-reference validation and scoped search and export. *Done when:* acceptance 4–6 pass, including the two-account probes and the GraphQL path (REQ-130).
3. **Move with dependency checks.** Dependency checker, dry-run report, transactional move with scheduler pause and audit. *Done when:* acceptance 7–8 pass, including the refused path.
4. **Limits, usage and delegated administration.** Counters, enforcement and warnings, the role matrix, ownership transfer, project audit screen and archive guards. *Done when:* acceptance 9–14 and 16 pass, and the project-owner session cannot reach any instance-wide surface by API call.

### Risks / notes

- Scoping must fail closed everywhere: global search, exports, GraphQL (REQ-130), webhook and schedule listings are the usual places a single unscoped query leaks another project's data, so each needs an explicit scoped test.
- The name collides with the delivery-tracking module of REQ-056: navigation sections, docs and URLs must keep them apart, and neither may claim the generic word `projects` in shared copy.
- The default project must exist before any insert path runs; the migration order and a startup check enforce it, and the constraint protects it from deletion.
- Role changes must invalidate permission caches immediately — the cache key includes a membership revision, and a test proves grant and revoke both bite on the next request.
- Limit enforcement under concurrency can overshoot briefly; counters update atomically and the documented behaviour is "may exceed by in-flight work, corrected on the next tick", never silent unbounded overshoot.
- Deleting a project with dependencies is destructive: restrict foreign keys, typed confirmation, export-first hint and a dependency listing before the final action.
- Delegation must be narrower than it feels: project owners manage their project only, and the tests must attempt the widest calls a curious owner could make, including other projects' audit endpoints.
- Backfills on large installations take time: batched updates, migration before serving traffic, and a documented estimate so an upgrade window can be sized.
