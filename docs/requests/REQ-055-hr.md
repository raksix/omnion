# REQ-055 — HR

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/hr`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

People operations.

- **Employees** (profile, position, department, manager, start date, employment type) with documents.
- **Departments & org chart** (tree view).
- **Leave**: types (annual, sick, unpaid), balances, request → approve flow, calendar of absences.
- **Attendance**: check-in/out (manual or via API), monthly summary.
- **Onboarding checklist** templates applied per new employee.
- **Permissions**: HR role sees all; manager sees own team; employee sees self.
- **Events**: `hr.leave.requested`, `hr.leave.approved`, `hr.employee.joined`.

## Implementation spec

> **Module:** `modules/hr` (crate `omnion-module-hr`, workspace member) · **Migration:** `database/migrations/0015_hr.sql` (next free slot at build time) · **Admin routes:** `/hr/*` + self-service `/hr/me/*` · **Permission family:** `hr.*` · **Depends on:** core crates (`identity` for user links, `media` for documents, `notifications`, `approvals` REQ-059) + `modules/calendar` (REQ-057) for the absence overlay.

### Scope (in / out)

**In**

- Employees: employee number, name, work e-mail, phone, position, department, manager, employment type, start/end date, status (`active`, `on_leave`, `terminated`), location, optional link to a platform user account.
- Departments: hierarchy (parent/child), department manager, member counts; org chart as an accessible tree (no drag-only interaction).
- Employee documents: contract, ID, certificate, other — stored through the media pipeline with kind, validity date and download permission.
- Leave: types with yearly entitlement (annual, sick, unpaid, …), per-employee balances per year, request → approve/reject flow through REQ-059 (with a direct-decide fallback), absence calendar, half-day support, overlap detection.
- Attendance: daily check-in/check-out recorded manually, through the API (service account/key) or by import; monthly per-employee summary with worked minutes and exceptions (missing checkout, late).
- Onboarding: templates with ordered items (task, owner role, due offset in days), applied to a new employee, per-employee checklist with progress; task creation can also emit into REQ-056 projects.
- Visibility: `hr.employees.read` with `own` / `team` / `all` bindings; sensitive personal fields behind `hr.employees.sensitive.read`; every employee sees their own record and self-service screens.

**Out (tracked elsewhere)**

- Payroll, salary, bank details and tax filings → deliberately **not** in this wave (keeps the personal-data surface small; a `modules/payroll` request would bring its own controls).
- Recruitment/applicant tracking and performance reviews → future requests. Expense claims for employees → REQ-054 expenses. Task delivery work → REQ-056.
- Calendar UI and booking pages → REQ-057 (HR only emits absence data). E-signature of contracts → REQ-030.

### Screens (UI)

Module nav: **Overview · Employees · Departments · Leave · Attendance · Onboarding · Documents · Reports · Settings**, plus a `My workspace` group for self-service.

| Route | Screen |
|---|---|
| `/hr` | Overview: headcount by department/type, starters and leavers this quarter, today's absences, pending leave requests, onboarding in progress |
| `/hr/employees` | Employee list (table) |
| `/hr/employees/new`, `/hr/employees/{id}` | Employee create form / detail (Profile · Leave · Attendance · Documents · Onboarding · Notes tabs) |
| `/hr/departments` | Department tree + org chart toggle, department form, manager assignment |
| `/hr/leave` | Leave requests list + absence calendar (month) |
| `/hr/leave/new`, `/hr/leave/{id}` | Request form / request detail with decision panel |
| `/hr/leave/types` | Leave type editor with entitlement and approval settings |
| `/hr/attendance` | Today's roster + monthly grid per employee with corrections |
| `/hr/onboarding`, `/hr/onboarding/templates/{id}` | Onboarding board (who is in progress) / template editor |
| `/hr/documents` | Document list across employees (kind, employee, validity, uploaded) |
| `/hr/reports` | Headcount, turnover-lite, absence summary, attendance summary; CSV export |
| `/hr/settings` | Working days, default weekly hours, approval routing, self-service toggles, onboarding default template |
| `/hr/me`, `/hr/me/leave`, `/hr/me/attendance`, `/hr/me/documents`, `/hr/me/onboarding` | Self-service: my profile, my leaves (+ request), my clock-in/out, my documents, my checklist |

**Employees list** — columns: `No`, `Name` (link, avatar initials), `Position`, `Department`, `Manager`, `Type`, `Start date`, `Tenure` (computed), `Status` (badge), `Contact` (e-mail icon). Filters: search (name/number/e-mail), department (incl. sub-departments toggle), manager, type, status, start-date range, "on leave today". Bulk: assign department, assign manager, deactivate (terminate), export, send onboarding. Row actions: open, edit (drawer), add document, apply onboarding template, view leave. Shortcuts: `n` new employee, `/` search, `j`/`k` row move, `enter` open, `l` leave for selected, `?` help. States: skeleton table, empty state ("Add your first employee" + "Import from CSV"), error state with retry.

**Employee form** — fields: Employee no (required, unique per organization, auto-suggested `EMP-0001`), First name (required ≤80), Last name (required ≤80), Work e-mail (required, format, unique), Phone (`^\+?[0-9 ()-]{7,20}$`), Position (required ≤120), Department (required, combobox with create), Manager (combobox over employees, must not create a cycle — the API rejects self/descendant), Employment type (required select), Start date (required; start in the future is allowed and the record shows "starts soon"), End date (required when type = `contract`, ≥ start), Location, Link to user account (combobox over platform users without an employee record), Notes (≤2000). Sensitive block (hidden without the permission): personal e-mail, personal phone, address, emergency contact name/phone. Validation: block submit on error, focus first invalid field, warn (not block) when a manager is not in the same department.

**Employee detail** — Profile (all fields + audit trail link), Leave (balances for the current year with entitled/used/pending/remaining and the request list), Attendance (month grid with corrections), Documents (upload, kind, validity, download, remove with confirm), Onboarding (checklist with progress bar, tick/untick, notes), Notes.

**Departments / org chart** — left: tree with counts (`Engineering (12)`, children indented, keyboard arrow navigation); right: selected department form (name, code, parent, manager, description) plus its member list. Org chart toggle renders the same data as a nested card chart with a role=tree semantic, expandable nodes, keyboard support, and a "print/export PNG" is out of scope (screenshot guidance only). A department with children or members cannot be deleted — only renamed or merged (members move, audited).

**Leave** — request list columns: `Employee`, `Type`, `From`, `To`, `Days`, `Status` (badge), `Requested`, `Decided by`, `Comment`. Filters: status, type, department, date range (default current year), "pending only". Bulk: approve, reject (with one shared comment) — both permission-guarded. Absence calendar: month grid, one row per employee, leave as a bar (colours per type plus a text legend), today marked, hovering shows the request; leave outside the visible month continues with an arrow marker. Request form and detail: employee (default self; HR may pick another), type (required), from/to (required; `to ≥ from`), half-day toggle (only when from = to), days computed from the organization's working days and shown before submit, reason (≤500), attachment (optional). Submitting checks the balance (warns/refuses per type setting), blocks overlapping approved/pending leave of the same employee, then creates an approval routed to the manager. Detail shows an explicit timeline (requested → decided) and a decision panel with comment.

**Attendance** — header with `Today` and a clock-in/clock-out card for the signed-in employee; roster table of today (employee, status: working/on leave/absent, check-in, check-out, hours) and a monthly grid (rows employees, columns days, cell shows hours or a status glyph with a text label). Cell click opens the correction drawer (check-in, check-out, note, reason) which is audited. Filters: department, employee, period, exception type (missing checkout, overtime, under-hours). Exceptions are listed under the grid with counts. API-based check-in is documented on screen (a service account note, no secret shown).

**Onboarding** — board grouped by employee with progress bars; template editor: name, ordered items (title, owner role, due offset days, requires file), add/remove/reorder (drag handle plus `alt+↑/↓`), activate. Applying a template materialises the checklist with due dates computed from the start date. Items can be completed by the assigned role or HR with an audit entry; a request in REQ-059 is optional per item kind.

**Self-service (`/hr/me/*`)** — my profile (read-only except phone and address), my leave (balances, request form, cancel a pending request), my attendance (clock in/out + my month), my documents (my file list, download), my onboarding checklist. Every tile is real: no disabled controls and no data the caller may not see.

**Mobile:** lists become cards (name → department → status), the leave request form is single column with a date-range picker, the clock-in card is the first element on `/hr/me/attendance`, and the org chart switches to an indented list.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/hr/employees` | Employee list / create | `hr.employees.read` / `.create` |
| GET/PATCH/DELETE | `/api/v1/hr/employees/{id}` | Detail / update / terminate (soft) | `hr.employees.read` / `.update` / `.delete` |
| GET | `/api/v1/hr/employees/export` | CSV export of the filtered list | `hr.employees.export` |
| GET/POST | `/api/v1/hr/departments` | Department tree / create | `hr.departments.read` / `.manage` |
| PATCH/DELETE | `/api/v1/hr/departments/{id}` | Update / merge or delete (empty only) | `hr.departments.manage` |
| GET | `/api/v1/hr/org-chart` | Nested tree of departments + employees | `hr.employees.read` |
| GET | `/api/v1/hr/documents` | Documents across employees | `hr.documents.read` |
| POST | `/api/v1/hr/employees/{id}/documents` | Attach a document (media id) | `hr.documents.manage` |
| DELETE | `/api/v1/hr/documents/{id}` | Remove a document | `hr.documents.manage` |
| GET/POST | `/api/v1/hr/leave/types` | Leave types read / manage | `hr.leave.read` / `hr.leave.manage` |
| GET | `/api/v1/hr/leave/balances` | Balances for an employee/year | `hr.leave.read` (self allowed) |
| GET/POST | `/api/v1/hr/leave/requests` | List / create a request | `hr.leave.read` / `hr.leave.request` |
| GET | `/api/v1/hr/leave/requests/{id}` | Detail with decision timeline | `hr.leave.read` |
| POST | `/api/v1/hr/leave/requests/{id}/decision` | Approve / reject with comment | `hr.leave.approve` |
| POST | `/api/v1/hr/leave/requests/{id}/cancel` | Cancel a pending request (self or HR) | `hr.leave.request` |
| GET | `/api/v1/hr/leave/calendar` | Absence calendar for a period | `hr.leave.read` |
| GET | `/api/v1/hr/attendance` | Monthly grid / today's roster | `hr.attendance.read` |
| POST | `/api/v1/hr/attendance/check-in`, `/check-out` | Clock for the caller (or a target with permission) | `hr.attendance.record` |
| POST | `/api/v1/hr/attendance/corrections` | Correct a day (audited) | `hr.attendance.manage` |
| GET | `/api/v1/hr/attendance/summary` | Per-employee monthly summary + exceptions | `hr.attendance.read` |
| GET/POST | `/api/v1/hr/onboarding/templates` | Templates read / manage | `hr.onboarding.read` / `.manage` |
| POST | `/api/v1/hr/employees/{id}/onboarding` | Apply a template | `hr.onboarding.manage` |
| PATCH | `/api/v1/hr/onboarding/items/{id}` | Tick / untick an item (self allowed) | `hr.onboarding.manage` |
| GET | `/api/v1/hr/reports/{report}` | `headcount`, `turnover`, `absence`, `attendance` | `hr.reports.read` |
| GET | `/api/v1/hr/me` | The caller's own employee record | self (no permission) |

Attendance API keys: a service account (REQ-022) with `hr.attendance.record` may post check-in/out for employees it is allowed to act for; key scopes and last-used are visible in the developer portal, and the payload is idempotent per employee + day + kind.

### Data model

```text
hr_departments(id uuid pk, organization_id uuid not null, name text not null, code text,
  parent_id uuid references hr_departments(id) on delete restrict, manager_user_id uuid, description text,
  active boolean not null default true)
hr_employees(id uuid pk, organization_id uuid not null, employee_no text not null, user_id uuid,
  first_name text not null, last_name text not null, work_email citext not null, phone text, position text not null,
  department_id uuid not null references hr_departments(id) on delete restrict, manager_id uuid
    references hr_employees(id) on delete set null, employment_type text not null, start_date date not null,
  end_date date, employee_status text not null default 'active', location text,
  personal_email citext, personal_phone text, address text, emergency_contact text,
  notes text not null default '', created_at timestamptz, updated_at timestamptz)
hr_documents(id uuid pk, organization_id uuid not null, employee_id uuid not null, kind text not null,
  title text not null, media_id uuid not null, expires_on date, acknowledged_at timestamptz,
  uploaded_by uuid, created_at timestamptz)
hr_leave_types(id uuid pk, organization_id uuid not null, name text not null, code text not null,
  paid boolean not null default true, annual_days numeric(6,2) not null default 0, requires_approval boolean not null default true,
  allow_negative boolean not null default false, active boolean not null default true)
hr_leave_balances(id uuid pk, organization_id uuid not null, employee_id uuid not null,
  leave_type_id uuid not null, balance_year integer not null, entitled_days numeric(6,2) not null default 0,
  used_days numeric(6,2) not null default 0, pending_days numeric(6,2) not null default 0)
hr_leave_requests(id uuid pk, organization_id uuid not null, employee_id uuid not null, leave_type_id uuid not null,
  starts_on date not null, ends_on date not null, days numeric(6,2) not null, half_day boolean not null default false,
  reason text not null default '', attachment_media_id uuid, leave_status text not null default 'pending',
  approval_request_id uuid, decided_by uuid, decided_at timestamptz, decision_comment text, cancelled_at timestamptz,
  created_at timestamptz)
hr_attendance(id uuid pk, organization_id uuid not null, employee_id uuid not null, work_date date not null,
  check_in timestamptz, check_out timestamptz, minutes_worked integer, source text not null default 'manual',
  note text not null default '', corrected_by uuid, created_at timestamptz, updated_at timestamptz)
hr_onboarding_templates(id uuid pk, organization_id uuid not null, name text not null, items jsonb not null,
  active boolean not null default true)
hr_onboarding_items(id uuid pk, organization_id uuid not null, employee_id uuid not null, template_id uuid,
  position integer not null, title text not null, owner_role text, due_on date, requires_file boolean not null default false,
  done_at timestamptz, done_by uuid, note text not null default '')
```

Checks: `employment_type in ('full_time','part_time','contract','intern')`; `employee_status in ('active','on_leave','terminated')`; `leave_status in ('pending','approved','rejected','cancelled')`; `ends_on >= starts_on`; `days > 0`; `(half_day = false) or (starts_on = ends_on)`; `check_out is null or check_out > check_in`; `minutes_worked between 0 and 1080 or minutes_worked is null`; `balance_year between 2000 and 2200`; `parent_id <> id`; unique `(organization_id, employee_no)`, `(organization_id, lower(work_email))`, `(organization_id, lower(name))` on departments and leave-type `code`, unique `(employee_id, leave_type_id, balance_year)`, unique `(employee_id, work_date)` on attendance; `used_days >= 0`, `pending_days >= 0`.

Indexes: `hr_employees_org_department_idx (organization_id, department_id, employee_status)`, `hr_employees_org_name_idx (organization_id, lower(last_name), lower(first_name))`, `hr_employees_org_manager_idx (organization_id, manager_id) where employee_status = 'active'`, `hr_leave_requests_org_status_idx (organization_id, leave_status, starts_on)`, `hr_leave_requests_overlap_idx (employee_id, starts_on, ends_on)`, `hr_attendance_employee_date_idx (employee_id, work_date desc)`, `hr_documents_employee_idx (employee_id, created_at desc)`, `hr_onboarding_items_employee_idx (employee_id, position)`.

Department cycles are refused in the service (`parent_id` must not be a descendant), and manager cycles too (`manager_id` must not be a descendant of the employee in the reporting chain). Migration: `database/migrations/0015_hr.sql`, additive; seeds leave types (`Annual`, `Sick`, `Unpaid`, with 14/0/0 days as editable examples), one root department per existing organization, and a "New starter" onboarding template.

### Events

Emitted: `hr.employee.joined`, `hr.employee.updated`, `hr.employee.terminated`, `hr.department.changed`, `hr.leave.requested`, `hr.leave.approved`, `hr.leave.rejected`, `hr.leave.cancelled`, `hr.attendance.recorded`, `hr.attendance.corrected`, `hr.onboarding.applied`, `hr.onboarding.completed`, `hr.document.uploaded`, `hr.document.expiring` (validity within 30 days, once). Consumed: `approvals.request.decided` (REQ-059) settles leave; `user.created` (REQ-006) surfaces an "unlinked account" hint in the employees screen; `projects.task.completed` (REQ-056) can tick a matching onboarding item when the organization opts in.

Webhook relevance: `hr.leave.approved` is consumed by the calendar module (absence overlay, REQ-057); `hr.employee.joined` is the classic trigger for automations (create accounts, assign an onboarding template, notify the team); `hr.document.expiring` drives a reminder automation. Payloads carry ids and dates — never document bytes and never sensitive personal fields beyond the organization's own data.

### Acceptance criteria

- [ ] Migration `0015_hr.sql` applies on a populated database; `cargo test -p omnion-module-hr` is green and seeds leave types, a root department and a template.
- [ ] Every `/api/v1/hr/*` route is permission-guarded; self-service routes work for an employee without any `hr.*` permission but answer only for their own data.
- [ ] Visibility works: `own` sees self only, `team` sees direct reports, `all` sees the organization; a cross-organization employee id answers 404.
- [ ] Employee create/update/terminate, department changes, leave decisions, attendance corrections and document uploads write audit entries.
- [ ] A manager or parent cycle is refused with an explicit message (self-manager, descendant manager, department moved under its own child).
- [ ] Department with members or children cannot be deleted; merging moves the members and is audited.
- [ ] Leave days are computed from the organization's working days, half-days count as 0.5, and the number shown before submit equals the stored value.
- [ ] An overlapping leave request of the same employee is refused with the conflicting dates in the message.
- [ ] A request beyond the remaining balance is refused unless the type allows negative balances, and the balance card shows entitled/used/pending/remaining consistently before and after the decision.
- [ ] Approving a request updates `used_days`, emits `hr.leave.approved` and makes the employee show as "on leave" in the roster for those dates.
- [ ] Rejecting with a comment is visible on the request timeline; cancelling a pending request releases the balance and is audited.
- [ ] Attendance check-in/check-out records the right day, refuses a checkout without check-in and a second check-in the same day (idempotent for API callers).
- [ ] Monthly summary lists worked hours per employee and flags missing checkout and over/under hours, and the CSV export matches the grid.
- [ ] Applying an onboarding template creates the items with due dates derived from the start date; ticking an item is reflected in the progress bar and emits completion when the last item is ticked.
- [ ] Document upload stores through media, downloads with the right name/type, and an expiring document produces one `hr.document.expiring` event.
- [ ] The org chart renders the tree, navigates with the keyboard and matches the employee counts shown in the department list.
- [ ] `hr.employee.joined` and `hr.leave.approved` appear in the event feed with the documented payload and reach a subscribed webhook.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no placeholder employees.
- [ ] Mobile 390×844: employee list, leave request form, clock-in card and my-checklist are usable.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/hr`, `/hr/employees`, `/hr/employees/new`, `/hr/departments`, `/hr/leave`, `/hr/leave/types`, `/hr/attendance`, `/hr/onboarding`, `/hr/documents`, `/hr/reports`, and the self-service `/hr/me`, `/hr/me/leave`, `/hr/me/attendance` (desktop) plus `/hr/employees`, `/hr/me/attendance` (mobile). The script must: create a department (and a child) → create two employees, one reporting to the other → apply the seeded onboarding template and tick one item → create a leave type → request 3 days of annual leave with the second employee → decide it as approver → see the balance change and the absence calendar entry → clock in/out on `/hr/me/attendance` → open the monthly summary and the reports. It clicks every control on each screen (including the tree keyboard path, the decision dialog and the correction drawer).

Visual check: employees table shows avatars, department chips and status badges with text; the leave calendar shows absence bars with a legend and today marked; the balance card shows four numbers that add up; the org chart is a legible nested tree at 1440 px; the onboarding checklist shows a progress bar and ticked items styled distinctly (not colour alone). Screenshots: `page-hr-employees`, `page-hr-leave`, `page-hr-attendance`, `page-hr-org-chart`, `mobile-hr-me-attendance`. Zero high findings; no PII (personal e-mail/phone) visible in the default employee list or in screenshots; AA contrast on badges.

### Slices

1. **Employees + departments + org chart (data, API, screens).** Migration, seeds, CRUD with visibility levels and sensitive-field gating, employee form/list/detail, department tree, org chart, documents tab. Done when two employees in a parent/child department render in both the tree and the chart with correct counts.
2. **Leave end to end.** Types, balances, request form, overlap/balance checks, decision flow (REQ-059 or fallback), absence calendar, events. Done when a request is approved through the inbox, the balance moves, the calendar shows it and `hr.leave.approved` is emitted.
3. **Attendance + self-service.** Check-in/out (UI and API), monthly grid, corrections, summary and exceptions, `/hr/me/*` screens. Done when the QA walkthrough clocks in and out, the month grid shows the hours and a correction is audited.
4. **Onboarding + documents + reports.** Templates, per-employee checklists, document list with expiry events, HR reports and CSV export. Done when a template run completes and every report exports row-for-row with the table.

### Risks / notes

- **Personal data is the risk:** keep salary and bank details out of the schema entirely; gate personal contact fields behind a separate permission; never render them in list views, exports or screenshots. Bulk export requires its own permission and is audited.
- **Balance math must have one owner:** always recompute `used_days`/`pending_days` from the request rows inside the decision transaction; never increment in place, or the balance will drift.
- **Overlap and cycle rules live in SQL + service:** date-range overlap for the same employee, department cycles, manager cycles — each needs a test, because the panel cannot express all of them.
- **Attendance time zones:** store timestamptz, derive the work date in the organization's time zone, and document that the month grid uses the same rule (or the summary and the roster will disagree across midnight).
- **Approvals coupling:** leave must stay usable if REQ-059 is not installed (direct decision by `hr.leave.approve`), with the same audit trail either way.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
