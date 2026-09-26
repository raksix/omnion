# REQ-056 — Projects & Tasks

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/projects`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Delivery work tracking.

- **Projects** with members, client link (CRM), budget-lite, status.
- **Tasks**: assignee, due date, priority, tags, checklists, dependencies-lite.
- **Board view** (kanban by status), **list view** (filters/sorting), **timeline-lite** (by due date).
- **Timesheets**: log time per task, weekly grid, approval, export to invoicing.
- **Milestones** with progress rollup.
- **Events**: `projects.task.created`, `projects.task.completed`, `projects.timesheet.submitted`.

## Implementation spec

> **Module:** `modules/projects` (crate `omnion-module-projects`, workspace member) · **Migration:** `database/migrations/0016_projects.sql` (next free slot at build time) · **Admin routes:** `/projects/*` · **Permission family:** `projects.*` · **Depends on:** core crates + `modules/crm` (REQ-051) for the client link + `modules/accounting` (REQ-054) / `modules/sales` (REQ-052) for the timesheet → invoice handoff.

### Scope (in / out)

**In**

- Projects: name, key (`WEB`), client (CRM company), owner, members with roles, status (`planning`, `active`, `on_hold`, `completed`, `cancelled`), start/due dates, budget-lite (amount + optional hourly rate, no cost accounting), description.
- Tasks: title, rich-lite description, status (`todo` → `in_progress` → `review` → `done`, plus `cancelled`), priority (`low`/`medium`/`high`/`urgent`), assignee, due date, estimate in minutes, labels, checklist items, comments, one-level subtasks, finish-to-start dependencies-lite.
- Views: kanban board by status (drag + keyboard move, per-column counts), list view with filters/sorting/bulk edit, timeline-lite (bars by due date over a date axis, dependency arrows, zoom week/month).
- Milestones with due date and a progress rollup computed from their tasks (percentage + counts).
- Timesheets: log time per task (manual entry and a start/stop timer), weekly grid per user, submit → approve/reject, billable flag, locked after approval; export approved billable hours to a draft invoice line set in accounting.
- My-work screen: tasks assigned to me across projects, sorted by due date, with a "log time" action on each row.
- Events for task lifecycle and timesheet flow; notifications on assignment, mention, due-soon and status change.

**Out (tracked elsewhere)**

- Full Gantt editing (drag to reschedule, critical path) → future depth request; this REQ ships a read-oriented timeline with keyboard rescheduling (±1 day/week nudges).
- Accounting side of billing → REQ-054 (projects only produces approved billable hours and requests a draft). CRM deals → REQ-051. Knowledge/wiki pages per project → REQ-058. Real-time cursors/presence → REQ-041 (the module works with plain refetch until then).

### Screens (UI)

Module nav: **Overview · Projects · My work · Timesheets · Reports**.

| Route | Screen |
|---|---|
| `/projects` | Overview: my open tasks, projects at risk (past due tasks), this week's due work, hours logged this week, milestone status |
| `/projects/list` | Project list (table) |
| `/projects/{id}` | Project detail with view tabs: **Board · List · Timeline · Milestones · Members · Files · Settings** |
| `/projects/{id}/tasks/{taskId}` | Task detail as a right-hand drawer over the current view (deep-linkable route) |
| `/projects/tasks` | My work: all tasks assigned to the caller, grouped by due bucket (Overdue · Today · This week · Later · No date) |
| `/projects/timesheets` | Weekly grid (rows: my tasks; columns: Mon–Sun; footer: totals) + submit flow |
| `/projects/timesheets/approvals` | Approvals queue for team leads: per-user week, hours, billable split, approve/reject with comment |
| `/projects/reports` | Hours by project/user/period, task throughput, overdue rate, estimate-vs-actual; CSV export |
| `/projects/settings` | Statuses, priorities, default hourly rate, timer rounding, week start, timesheet lock policy |

**Project list** — columns: `Key`, `Name` (link), `Client` (CRM link), `Owner`, `Status` (badge), `Members` (avatars + count), `Tasks` (`done/total`), `Due` (red when past due and not completed), `Budget` (amount + `used-lite` = approved billable hours × rate), `Updated`. Filters: search (name/key/client), status (multi), owner, member, due range, "past due". Bulk: change status, add member, archive, export CSV. Row actions: open, edit (drawer), archive, duplicate as template. Shortcuts: `n` new project, `/` search, `enter` open, `g b` board, `g l` list, `?` help. States: skeleton table, empty state ("Create a project" + "Start from a template" once one exists), error state with retry.

**Project detail / Board** — columns per status with a count header; cards show key-task number, title, labels, assignee avatar, due date (amber ≤2 days, red overdue), checklist `2/5`, comment count. Drag between columns writes optimistically with rollback + toast; keyboard alternative: focus card, `ctrl/cmd + ←/→` moves status, `alt + ↑/↓` reorders inside a column. Column footer "+ Add task" creates inline (title only, `enter` to save, `esc` to cancel). Filters persist per project per user (assignee, label, priority, due range, search).

**Project detail / List** — columns: `#`, `Title` (link), `Status` (inline select), `Priority` (inline select with icon + text), `Assignee` (inline select), `Due`, `Estimate`, `Logged`, `Labels`, `Checklist`, `Updated`. Multi-select with `x`/`shift+click`, bulk edit side panel (status, priority, assignee, due date, labels, add/remove from milestone), CSV export of the filtered set. Sort by any column, saved per user.

**Project detail / Timeline** — date axis (week/month zoom), one row per task, bar from created/start to due date (no start date → bar starts at the project start), overdue bars red-tipped, dependency arrows between finish-to-start pairs, today line, milestone diamonds on the axis. Interactions: `←/→` nudge due date by one day, `shift+←/→` by one week (audited), click opens the drawer. A visible legend names every colour, so colour is never the only signal.

**Task drawer/detail** — fields: Title (required ≤200), Description (markdown-lite with toolbar, ≤8000), Status (select), Priority (select), Assignee (combobox over project members), Due date, Estimate (minutes, ≥0), Labels (≤10 chips), Milestone (select), Parent task (optional, one level, cannot create a cycle), Billable toggle. Sections: Checklist (add/tick/reorder/remove), Dependencies (`blocked by` / `blocks` lists; adding one that would create a cycle is refused with the offending path shown), Comments (markdown-lite, `@mention` notifies members), Activity (status/assignee/due changes with actor and relative time), Time log (minutes + date + note, feeding the timesheet). Delete with confirm; every mutation audited.

**Timesheets** — weekly grid: rows = the caller's tasks (optionally grouped by project), columns Mon–Sun of the selected week, cells accept `2h`, `90m` or `1.5` and normalise to minutes on blur; footer row totals per day and the week; a "timer" control at the top (start/stop, running time visible, writes one entry on stop). Submit seals the week (no edits until decided); Approval queue shows per-user weeks with hours, billable split, per-task breakdown and approve/reject with a comment. Approved weeks show a lock icon and a `Export to invoice` action (creates a draft invoice line set through accounting for billable hours, linking back to the timesheet).

**Milestones** — list per project: title, due date, status (`open`/`reached`/`missed`), progress bar from its tasks with `done/total` and percentage; the rollup is computed, never hand-set. Reaching a milestone (all tasks done or manual close with a comment) emits an event and notifies members.

**Mobile:** project list and my work are cards; the board becomes a status-tabbed list (one column at a time) with a status select on each card — no horizontal drag page; task drawer is full screen; the timesheet grid becomes a day-per-screen list with a week switcher.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/projects` | Project list / create | `projects.projects.read` / `.manage` |
| GET/PATCH/DELETE | `/api/v1/projects/{id}` | Detail / update / archive | `projects.projects.read` / `.manage` |
| GET/POST/DELETE | `/api/v1/projects/{id}/members` | Member list / add / remove | `projects.members.manage` |
| GET/POST | `/api/v1/projects/{id}/tasks` | Task list (filters, view=board\|list\|timeline) / create | `projects.tasks.read` / `.create` |
| GET/PATCH/DELETE | `/api/v1/projects/tasks/{id}` | Task detail / update / delete | `projects.tasks.read` / `.update` / `.delete` |
| POST | `/api/v1/projects/tasks/{id}/status` | Status change (board move) | `projects.tasks.update` |
| POST | `/api/v1/projects/tasks/{id}/reorder` | Reorder inside a column | `projects.tasks.update` |
| POST/DELETE | `/api/v1/projects/tasks/{id}/dependencies` | Add / remove a dependency | `projects.tasks.update` |
| GET/POST/PATCH/DELETE | `/api/v1/projects/tasks/{id}/checklist` | Checklist items | `projects.tasks.update` |
| GET/POST | `/api/v1/projects/tasks/{id}/comments` | Comments (edit/delete own) | `projects.tasks.read` / `.comment` |
| GET/POST | `/api/v1/projects/{id}/milestones` | Milestones list / create | `projects.milestones.read` / `.manage` |
| POST | `/api/v1/projects/milestones/{id}/close` | Close / reopen with a comment | `projects.milestones.manage` |
| GET/POST | `/api/v1/projects/timesheets` | Weekly grid read / log an entry | `projects.timesheets.read` / `.log` |
| POST | `/api/v1/projects/timesheets/submit` | Submit a week | `projects.timesheets.log` |
| POST | `/api/v1/projects/timesheets/decision` | Approve / reject a week with comment | `projects.timesheets.approve` |
| POST | `/api/v1/projects/timesheets/export-invoice` | Draft invoice from approved billable hours | `projects.timesheets.approve` |
| GET | `/api/v1/projects/my-tasks` | Tasks assigned to the caller | self |
| GET | `/api/v1/projects/reports/{report}` | `hours`, `throughput`, `overdue`, `estimate-vs-actual` | `projects.reports.read` |

Board/list/timeline are the same resource with different `view=` and filter parameters; every mutation returns the fresh row for optimistic replacement.

### Data model

```text
projects_projects(id uuid pk, organization_id uuid not null, key text not null, name text not null,
  description text not null default '', client_company_id uuid, owner_user_id uuid,
  project_status text not null default 'planning', starts_on date, due_on date,
  budget_amount numeric(14,2), currency char(3) not null default 'USD', hourly_rate numeric(12,2),
  archived_at timestamptz, created_by uuid, created_at timestamptz, updated_at timestamptz)
projects_members(id uuid pk, project_id uuid not null references projects_projects(id) on delete cascade,
  user_id uuid not null, role text not null default 'member', created_at timestamptz)
projects_tasks(id uuid pk, organization_id uuid not null, project_id uuid not null, parent_task_id uuid,
  number integer not null, title text not null, description text not null default '',
  task_status text not null default 'todo', priority text not null default 'medium',
  assignee_user_id uuid, due_on date, estimate_minutes integer, position numeric(20,10) not null,
  labels text[] not null default '{}', milestone_id uuid, billable boolean not null default false,
  completed_at timestamptz, created_by uuid, created_at timestamptz, updated_at timestamptz)
projects_task_dependencies(id uuid pk, task_id uuid not null, depends_on_task_id uuid not null,
  created_at timestamptz not null default now())
projects_checklist_items(id uuid pk, task_id uuid not null references projects_tasks(id) on delete cascade,
  position integer not null, title text not null, done_at timestamptz, done_by uuid)
projects_comments(id uuid pk, organization_id uuid not null, task_id uuid not null, author_user_id uuid,
  body text not null, edited_at timestamptz, created_at timestamptz not null default now())
projects_milestones(id uuid pk, organization_id uuid not null, project_id uuid not null, title text not null,
  due_on date, milestone_status text not null default 'open', closed_at timestamptz, closed_by uuid, comment text)
projects_timesheet_entries(id uuid pk, organization_id uuid not null, project_id uuid not null, task_id uuid,
  user_id uuid not null, work_date date not null, minutes integer not null, billable boolean not null default false,
  note text not null default '', created_at timestamptz, updated_at timestamptz)
projects_timesheet_weeks(id uuid pk, organization_id uuid not null, user_id uuid not null, week_start date not null,
  week_status text not null default 'draft', submitted_at timestamptz, decided_by uuid, decided_at timestamptz,
  decision_comment text, invoice_id uuid, total_minutes integer not null default 0, billable_minutes integer not null default 0)
```

Checks: `project_status in ('planning','active','on_hold','completed','cancelled')`; `task_status in ('todo','in_progress','review','done','cancelled')`; `priority in ('low','medium','high','urgent')`; `week_status in ('draft','submitted','approved','rejected')`; `priority`-independent `estimate_minutes >= 0`; `minutes > 0 and minutes <= 1440` on a time entry; `due_on >= starts_on` on projects; `parent_task_id <> id`; `task_id <> depends_on_task_id`; unique `(organization_id, key)`, unique `(project_id, number)` on tasks, unique `(task_id, depends_on_task_id)`, unique `(user_id, week_start)` on weeks, unique `(task_id, user_id, work_date, minutes, note)` is **not** used (identical entries are legitimate); time entries are soft-capped by policy at 24h/day in the service with a warning.

Indexes: `projects_tasks_project_status_idx (project_id, task_status, position)`, `projects_tasks_assignee_idx (organization_id, assignee_user_id, due_on)`, `projects_tasks_due_idx (organization_id, due_on) where task_status not in ('done','cancelled')`, `projects_timesheet_entries_user_date_idx (user_id, work_date)`, `projects_timesheet_entries_project_idx (project_id, work_date)`, `projects_members_user_idx (user_id)`, `projects_milestones_project_idx (project_id, due_on)`, `projects_comments_task_idx (task_id, created_at desc)`.

Ordering inside a column/project uses a fractional `position` (`numeric`), so a reorder writes one row instead of renumbering a column; a periodic (or on-demand) compaction normalises gaps. Dependency cycles are refused in the service by walking the `depends_on` graph. Migration: `database/migrations/0016_projects.sql`, additive; no seeds beyond a per-organization task-number sequence.

### Events

Emitted: `projects.project.created`, `projects.project.status_changed`, `projects.task.created`, `projects.task.assigned`, `projects.task.status_changed`, `projects.task.completed`, `projects.task.due_soon` (emitted once per task, 24 h before due), `projects.task.overdue` (once), `projects.milestone.reached`, `projects.timesheet.submitted`, `projects.timesheet.approved`, `projects.timesheet.rejected`, `projects.invoice.requested`. Payloads carry ids, project key, assignee, due date and (for timesheets) total/billable minutes. Consumed: `crm.deal.won` (REQ-051) can create a project from a template by rule; `hr.employee.terminated` (REQ-055) surfaces "reassign open tasks" on the project; `accounting.invoice.issued` links the created invoice back to the timesheet week.

Webhook relevance: `projects.task.completed` and `projects.timesheet.approved` are the names an external time-tracking or invoicing system subscribes to; `projects.task.overdue` is the documented trigger for a chase automation (notify assignee → notify project owner → create a follow-up task).

### Acceptance criteria

- [ ] Migration `0016_projects.sql` applies on a populated database; `cargo test -p omnion-module-projects` is green.
- [ ] Every `/api/v1/projects/*` route is permission-guarded; a project from another organization answers 404, and a non-member without `projects.projects.read` cannot open it.
- [ ] Project, task, milestone, comment and timesheet mutations write audit entries with before/after values.
- [ ] Board drag moves a task between columns, persists the status, updates column counts, and is equally possible with `ctrl/cmd + ←/→`.
- [ ] Reordering inside a column writes exactly one row (position change) and survives a reload with the same order.
- [ ] Task list filters (assignee, label, priority, due range, search) and sorting combine; bulk edit applies to the whole selection and is reversible by a second edit.
- [ ] Adding a dependency that would create a cycle is refused with the offending path in the message; a blocked task shows its blocker and a warning when moved to `in_progress`.
- [ ] Checklist progress (`2/5`) is correct on the card, the list and the drawer, and ticking the last item does not silently complete the task.
- [ ] Milestone progress equals its tasks' `done/total` and updates when a task changes status; closing a milestone with a comment emits `projects.milestone.reached`.
- [ ] Timesheet grid accepts `2h`/`90m`/`1.5`, normalises to minutes, totals per day and week match the entries, and the timer records exactly one entry with the elapsed rounded time.
- [ ] A submitted week is read-only until decided; approve/reject with a comment is reflected on the week with a lock icon and the reason visible.
- [ ] `Export to invoice` on an approved week creates a draft invoice whose lines sum to the billable hours × rate and links back to the week.
- [ ] `projects.task.completed` and `projects.timesheet.approved` appear in the event feed with the documented payload and reach a subscribed webhook.
- [ ] Assignment, `@mention` and due-soon produce notifications to the right user (one per event, no duplicates).
- [ ] My-work groups tasks by due bucket and every row can log time without leaving the screen.
- [ ] Timeline renders bars per task with a today line, milestone markers and a legend naming every colour; arrow nudge changes the due date and is audited.
- [ ] Reports return hours by project/user/period, throughput, overdue rate and estimate-vs-actual; CSV export matches the table.
- [ ] Global search finds projects and tasks by key/title; ⌘K offers "New task" and "Log time" gated by permission.
- [ ] Empty, loading and error states exist on every screen; no dead buttons, no placeholder tasks.
- [ ] Mobile 390×844: project list, status-tabbed board, task drawer and timesheet day view are usable.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/projects`, `/projects/list`, `/projects/tasks`, `/projects/{id}` (board), the list and timeline tabs, a deep-linked task drawer, `/projects/timesheets`, `/projects/timesheets/approvals`, `/projects/reports`, `/projects/settings` (desktop) plus `/projects/list` and `/projects/timesheets` (mobile). The script must: create a project linked to the CRM sample company → add a member → create three tasks inline on the board → drag/arrow-move one to `in_progress` and one to `done` → add a checklist and tick an item → comment with `@mention` → add a dependency → create a milestone and see progress → log time for two days → submit the week → approve it as a lead → run `Export to invoice` → open the reports. It clicks every control on each screen, including the column footers, the dependency picker and the timesheet cells.

Visual check: the board shows three to four columns with count headers and cards carrying labels, avatar, due-date chip and checklist counter; the list shows inline selects that look editable; the timeline shows bars with a visible today line and a colour legend; the timesheet grid shows a footer totals row that matches the entries; the milestone shows a progress bar with `done/total`. Screenshots: `page-projects-board`, `page-projects-list`, `page-projects-timeline`, `page-projects-timesheets`, `mobile-projects-timesheets`. Zero high findings; no clipped labels on cards at 1440 px; AA contrast on priority and due-date chips; board columns fit without page-level horizontal scrolling.

### Slices

1. **Projects, members, tasks CRUD + list view.** Migration, permission keys, project list/detail, task model with filters, sorting, bulk edit, audit, tests. Done when a project with tasks is created through the panel and every list filter returns verifiable rows.
2. **Board + drawer (checklist, comments, dependencies).** Kanban with drag + keyboard, inline task create, task drawer with markdown-lite description, checklist, comments with mentions, dependency cycle guard. Done when QA moves a card, ticks a checklist item, comments a mention and is blocked by a second dependency that would cycle.
3. **Timeline + milestones.** Timeline view with legend and nudge rescheduling, milestone CRUD with computed progress and close flow. Done when timeline bars and milestone percentages agree with the underlying tasks.
4. **Timesheets + approvals + invoice handoff + reports.** Weekly grid, timer, submit/decide with lock, export to invoice, my-work screen, reports and CSV. Done when a submitted week is approved and produces a draft invoice with matching totals, and the QA pass ticks the acceptance list to zero high findings.

### Risks / notes

- **Ordering:** fractional positions avoid column renumbering; document the compaction rule so two clients reordering at once converge (server recomputes neighbours on conflict and returns the authoritative positions).
- **Time zones and week start:** the week is defined by the organization's week-start setting; entries store a plain `work_date` (no timestamps), and the grid derives its columns from the same setting, or the grid and the report will disagree.
- **Timesheet immutability:** approved weeks are locked; corrections go through a reopen action that is audited and, once invoiced, only via a credit note on the accounting side.
- **Estimate vs actual needs one rule:** sum of logged minutes versus `estimate_minutes` per task, aggregated per project; keep the definition printed on the report.
- **Board performance:** a project with 1 000+ tasks must page per column (virtual scroll) rather than load everything; set the limit at build time so the screen cannot quietly become slow.
- **Dependency depth:** this wave ships finish-to-start only; anything else (rescheduling cascades, critical path) belongs to a later depth request, and the UI must not pretend otherwise.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
