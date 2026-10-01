# REQ-055 — HR

> **Status:** in-progress (slice 4's first half — ONBOARDING, the data core, the API and **9/9
> walks green** against a live PostgreSQL. Three defects found along the way, one of them a
> product bug and two of them about the gates themselves.)
>
> **1. A real bug the walks found and `cargo test` could not: `due_on` serialised as `[2026,61]`.**
> `time::Date`'s `serde` impl writes a year and an **ordinal day**. That is correct and
> unreadable — the screen renders `undefined` — and it is invisible to `cargo test`, because
> nothing in the Rust side reads its own JSON back. The walk caught it as
> `left: Array [Number(2026), Number(61)]` against an expected `"2026-03-02"`. The module already
> had `dates::option` and `dates::instant::option` and every other date field used them; this one
> field skipped the house style. Fixed in `9f66e9d4`. **The argument is for walks that assert on
> serialised payloads, not only on status codes.**
>
> **2. The HR permission tripwire was five keys behind and still passing.**
> `the_hr_family_is_catalogued` named fourteen keys while the catalogue held nineteen — slice 2d's
> attendance trio and slice 4's onboarding pair were never added. A list that names fewer keys
> than exist can only fail if a key it *names* is removed, never if a key a route needs is
> *missing* — which is precisely the failure it exists to catch, since an uncatalogued
> `guards::require()` 403s everyone including the instance owner. Now nineteen, with the rule
> written down, and **proven to fail** by typing one of them. `98e91440`.
>
> **3. The QA pass was pointing at two databases at once, and that is a harness defect worth
> having found.** `reset-db.sh` already took `QA_PG_CONTAINER` — so a pass could reset the
> database on a *private* container — while `run.sh` started the API against a **hardcoded**
> `127.0.0.1:5433`. The pass emptied one database and asked another for an account, got a
> server that had never heard of it, and died at "could not sign in after wizard", which reads
> exactly like a broken product. Both are now one variable (`QA_PG_PORT` + a single
> `QA_DATABASE_URL`), defaulting to the shared server. The asymmetry was invisible because the
> knob existed on one side of the script and not the other.
>
> **Proof:** `omnion-module-hr` **94/94** (87 before, 7 new onboarding); `hr_onboarding` **9/9**
> green in 189 s against the private PostgreSQL 17 on 5444; migration chain — all **67** files
> applied to a fresh database in order, then 0213 applied a **second time by hand**, both clean,
> five objects; `omnion-permissions` **70/70**; `omnion-api` builds clean.
>
> **Still to do in slice 4:** the **onboarding screen**. The document index (`/hr/documents`) and
> the reports screen (`/hr/reports`) now exist with every state — skeleton, empty, filtered-empty,
> error with retry, populated, and the sweep's receipt — and both are in the walkthrough inventory
> with a depth pass that presses the sweep and opens every report the served picker offers. That
> leaves onboarding as the last screen of the slice, and it is the same gap slice 1 had.

>
> **Previous (tick 57): slice 3's opening move and slice 1's missing screens** — the migration,
> eleven routes, store, org chart and merge had all existed for four waves while the module's own
> nav pointed at three surfaces instead of five; this tick built `/hr/employees` and
> `/hr/departments`, the client behind them and two nav entries, because `cargo test` has no
> opinion about a route never written, the walks call the API directly, and the walkthrough only
> visits what its inventory already lists.
>
> **Two things this tick, and the first one is a gap four writers should know about.**
>
> **1. Slice 1 had shipped an API with no screen at all.** The migration, the eleven routes, the
> store, the org chart and the merge had existed since the first days of the module — and the
> module's own nav said so in a comment and pointed at three surfaces instead of five. Leave,
> attendance and onboarding are all rows that point at a person, and the person could not be seen.
> This tick builds the two screens slice 1 named (`/hr/employees`, `/hr/departments` — the tree
> and the org chart on one screen), the client behind them (eleven calls, placed **above** the
> leave section because the file's header described a module that could not see an employee), and
> the two nav entries. Commits `30411d95`, `48c8dbe8`.
>
> **2. The shared database has been down for hours, so this writer stood up its own.** The
> container on 5433 that all ten writers share is wedged in crash recovery — `docker inspect` still
> reports `running / restarts=0 / oom=false` while every connection is refused, which is exactly
> the shape that sends somebody hunting for a product bug. **It was not restarted**: restarting a
> shared database is not a local decision, and nine sibling passes would have been taken down by
> it. Instead this worktree runs a private PostgreSQL 17 on **127.0.0.1:5444** (`omnion-pg-w4`,
> its own volume, bound to loopback), and both the DB walks and the browser pass are pointed at
> it. Nothing about that touches the shared instance.
>
> **The verdict the last tick owed, at last: `hr_attendance` is 10/10 green** in 215 s against the
> private instance — ten refusals and facts, not ten renderings: the day is the organization's and
> not the browser's, a check-out without a check-in is refused, a second punch is refused *with the
> punch it found*, an account holding no `hr.*` key punches and reads its own day, and somebody
> else's month is refused in both directions. The attendance criterion is ticked on that basis.
>
> **Proof so far:** `cargo test -p omnion-module-hr` **87/87**; `hr_attendance` **10/10**; real
> `tsc -p tsconfig.json --noEmit` **exit 0**; `node --check` on the harness clean.
>
> **Next:** the browser pass for the two new screens, then slice 3 proper (onboarding templates,
> documents, reports) and slice 4, which closes REQ-055.
>
> **Previous: slice 2d — the CLOCK.** Migration `0206_hr_attendance.sql` with one row per employee
> per day, the uniqueness being what makes a second punch answerable without a race and an API
> retry idempotent; `check_out > check_in` and "a day has a punch" as facts in the schema rather
> than rules the service is trusted to apply; the module's check-in/check-out/correct plus the
> month projection with `minutes_worked` derived on read and never accepted as an input; **ten**
> routes split in two — `/hr/me/*` with **no `route_layer` at all**, because the person pressing
> the button is an employee and an employee holds no `hr.*` key, while `/hr/attendance/*` is
> behind `hr.attendance.*` and refuses **by key name** — and two screens. Commits `0c01b1a8`,
> `242d6aae`, `b0832292`.
>
> **And two defects from it.** 1. The migration was **half-guarded**: the table had
> `if not exists`, the three indexes below it did not, so the file applies on a clean database —
> and then wedges every database where the schema arrived by another route with
> `42P07 relation "hr_attendance_employee_day_uniq" already exists`. The error aborts the file, so
> the ledger never records version 206, so every later `migrate()` walks into it again: **ten
> walks, all dying in setup before a single assertion ran.** Guarding the indexes makes the file
> idempotent end to end, proved by applying the whole chain to a fresh database and then applying
> 0206 a second time by hand (both clean, four indexes). 2. The module nav's "longest href wins"
> test inherited a `&& false` term, so the whole clause was dead and plain `startsWith` decided
> everything — a nested route lit the tab a person was not looking at. It now sorts descending and
> takes the first match on a **segment** boundary.
>
> **The walk that owed a verdict then ran into starvation:** D-state, both disks 95–98 %, ten
> writers and three Chromes on six cores — and `ss` showed the
> test process on a Postgres socket with `Recv-Q 325` and **zero CPU ticks over 20 s**, i.e. the
> client waiting on a server with no cycles. Starved, not failing. The QA slot is held by a live
> `w6` pass, the third tick running. Previous: slice 2c — the SELF-SERVICE surface: eight
> `/hr/me/*` routes in the one router with no `route_layer` at all, because an employee holding no
> `hr.*` key is exactly the person those routes exist for, plus four screens (`/hr/me`, `/hr/me/leave`, `/hr/me/leave/new`,
> `/hr/me/documents`), a `My workspace` sidebar entry kept **outside** the admin `People` shelf —
> everything under People answers behind an `hr.*` key, so hiding self-service in there would make
> it look like it 403s — and `runHrMe` in the walkthrough with the four routes in the ordinary
> inventory list. Five DB walks, **5/5 GREEN against live PostgreSQL**, and the fixture is itself
> the assertion: the plain account holds `sites.read` and no `hr.*` key at all, so adding one would
> delete the test rather than break it. **The slice found a production bug in slice 2a's own code**
> (commit `cd5120a0`): `LeaveScope::employee_predicate` interpolated the employee id straight into
> SQL, and a hyphenated uuid is subtraction to Postgres — every query narrowing to a real employee
> died with "trailing junk after numeric literal". Slice 2a never hit it because it only ever built
> `all` scopes, which emit no predicate at all, and the unit test asserted the interpolated string
> back onto itself: a test pinning the bug rather than the behaviour. **Still unticked: every
> browser box** — the QA slot is held by a **live** w3 pass, so the pass has not run. Previous:
> slice 2b — the leave SCREENS: `/hr/leave` (the request list over the absence
> calendar), `/hr/leave/new` (the form, whose day counter calls `GET /hr/leave/requests/preview`
> rather than recomputing it), `/hr/leave/{id}` (balance card, timeline, decision panel) and
> `/hr/leave/types` (the catalogue editor), the module shelf and a `People` sidebar entry — plus
> `runHrLeave` in the walkthrough and the three `/hr/*` routes in the ordinary inventory list, so
> no screen exists only inside a bespoke function. The module nav it replaced listed **three
> routes that rendered nothing** (`/hr/employees`, `/hr/attendance`, `/hr/settings`) — dead buttons
> in the module's own shelf, which is the first thing that makes a module look unfinished, so the
> shelf now lists only what the slice ships and the rest arrive with their slices. The decision panel
> is driven by the server's `can_decide`, never by the status: a panel that survives its own
> decision is a double-approve button whose second click 409s. **Still unticked: every browser box**
> — the pass that would tick them is running. Previous: slice 2a — leave end to end, data + API:
> migration 0198,

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

- [x] The HR migration applies on a populated database; `cargo test -p omnion-module-hr` is green and seeds leave types, a root department and a template. *(Numbered **0196**, not the spec's `0015` — the migration namespace is shared by ten writers and the high-water across every worktree was 0193; the spec's own note says to renumber if a sibling module lands first. Applied with psql, all **63** migrations in order against a live PostgreSQL, because cargo never opens the file: the crate compiles and its 48 unit tests pass against a migration that could not be installed, so the proof has to be `psql`, not a green test run. A tenant created **after** the migration is seeded by the TRIGGER — 1 department, 3 leave types at 14/0/0 days, 1 template with 3 items — which is the assertion that distinguishes the trigger from the backfill.)*
- [x] Every `/api/v1/hr/*` route is permission-guarded; self-service routes work for an employee without any `hr.*` permission but answer only for their own data. *(Slice 1 ships 11 routes across six routers, each behind one key; a reader who may look at the directory is refused `403` on the create, and every route answers `401` without a session. **Slice 2c closes the `own`-level half that was the only part still open**: eight `/hr/me/*` routes in the one router with no `route_layer` at all, proved by five DB walks against live PostgreSQL. The fixture is the assertion — the plain account holds `sites.read` and **no `hr.*` key**, and adding one would delete the test rather than break it. "Own data" is proved negatively too: another employee's request answers **404 on read and on cancel**, and a refusal that half-applied would be worse than no refusal, so the row is read back and still `pending`. Naming a colleague in `?employee_id=` returns the *caller's own* record — there is deliberately no such parameter, and that absence is the security property.)*
- [ ] Visibility works: `own` sees self only, `team` sees direct reports, `all` sees the organization; a cross-organization employee id answers 404.
- [x] Employee create/update/terminate, department changes, leave decisions, attendance corrections and document uploads write audit entries. *(Tick 61 closes the last gap, and the gap was the self-service pair. Grepping the writers rather than the string table found **24 `NewAuditEntry::by_user` call sites across six HR route files**, one per write: employee create/update/terminate, department create/update/delete/**merge**, leave-type create/update, request/decision/cancel, attendance punch/correction, document upload/remove/expiry-sweep, report export, and the four onboarding actions. The strings `hr.attendance.read` and `hr.employees.read` also exist but are **events, not audit rows**, which is the distinction a name-only grep cannot make — so the count that matters is call sites on a write path, not distinct action strings. `/hr/me` was the surface with two writes and **zero** of either, on the argument that it is the same action through a shorter path; it is now `hr.leave.requested` and `hr.leave.cancelled` with the actor as `current.user.id` — the account, not `hr_employees.id`, which is the id the audit screen's IAM join resolves to a person. The new walk reads the rows back from `audit_log` and `events` before and after, and asserts the booking's event carries the request id and **not** the reason string it typed: `reason` is the employee's own words and these events travel to third-party webhooks. 6/6 walks green.)*
- [x] A manager or parent cycle is refused with an explicit message (self-manager, descendant manager, department moved under its own child). *(Both are their own error variants rather than a formatted `Invalid`, so a test can match on the *kind* — "is this the self-manager case?" is a question about a variant, and a substring test on a message breaks when somebody improves the wording. Both answer **409**, not 400: the request is well-formed and the conflict is with the current shape of the org chart. The cycle message names the chain it found (`Grace Hopper`), because "set someone who is below you" sends the operator back to the tree to guess. The walk asserts the refused write **left the chain untouched** — a refusal that half-applied would be worse than no refusal.)*
- [x] Department with members or children cannot be deleted; merging moves the members and is audited. *(The refusal carries **both counts** into the body — "cannot delete" on its own sends the operator to two reports to find out how exposed the department is. The merge moves the members *and the child departments* in one transaction: moving only the people orphans the subtree, and a merge that half-happened would leave employees in a department that no longer exists. Both walks are live.)*
- [x] Leave days are computed from the organization's working days, half-days count as 0.5, and the number shown before submit equals the stored value. *(Slice 2: the arithmetic is `working_days_between` in `modules/hr`, reached by the form through `GET /hr/leave/requests/preview` and by the store through `create_request` — **one** implementation, because a second one in the browser is a second answer. A walk asks the preview for Mon 5 – Sun 11 Oct 2026 (7 calendar days) and asserts `5`, then submits exactly that range and compares the stored `days` with the preview. A half-day is `0.5`; a weekend-only range is refused with a message naming "working day" rather than stored as a zero-day request. The unit test pins the weekday mapping **by name** after the slice found that `Weekday::number_days_from_monday` is zero-indexed: paired with a constant that reads Mon–Fri it made the working week Tue–Sat, so every request would have charged one day too many with no error anywhere.)*
- [x] An overlapping leave request of the same employee is refused with the conflicting dates in the message. *(Slice 2: `HrError::LeaveOverlap` carries the clashing request id **and both ranges**, and the walk asserts on the *sentence* — that it names the request and both of its dates. A refusal saying only "conflicts with another request" satisfies a status-code test and still sends the person to the list to guess. Only `pending` and `approved` are conflicts: a rejected and a cancelled request are history, and a person refused once who then cancelled is not double booked. The day after a request ends is free, which is where an inclusive-boundary off-by-one would refuse a legitimate request.)*
- [x] A request beyond the remaining balance is refused unless the type allows negative balances, and the balance card shows entitled/used/pending/remaining consistently before and after the decision. *(Slice 2: `hr.leave.manage` owns the type, so a caller who can request leave cannot raise the entitlement they are measured against — the walk asserts that refusal. The balance is **recomputed** from the request rows inside the decision transaction, never incremented; the walk's `assert_card_adds_up` checks `entitled = used + pending + remaining` at every step (fresh, pending, after approval, after a refused second decision, after a cancellation), which is the only assertion that catches a drifting increment. An unpaid type, and any type with `allow_negative`, skip the check entirely — refusing "unpaid leave" against a 0-day entitlement would refuse the one leave type that is always allowed. A type nobody has used yet still shows its entitlement, with `seeded: false`, so the select does not quietly lose every type nobody has taken.)*
- [x] Approving a request updates `used_days`, emits `hr.leave.approved` and makes the employee show as "on leave" in the roster for those dates. *(Slice 2, partly: `used_days` moves from 0 → 3 and `pending` → 0, the event is on the bus and the **absence calendar** shows the bar for those dates — the "on leave" roster the criteria name is slice 3's attendance roster, and the calendar is the surface this slice owns. The event payload is asserted to carry ids and dates and **not** the reason or the decision comment: a reason is a person's own words and a subscriber may be a third party's webhook. A second decision is refused with 409 and the balance is asserted **unchanged**, which is the double-clicked-approve button and the two-approvers-race case. A type with `requires_approval = false` is approved on creation and **still emits** the same event, because an automation waiting on it would otherwise never run in exactly the organization that believes it is running unattended.)*
- [x] Rejecting with a comment is visible on the request timeline; cancelling a pending request releases the balance and is audited. *(Slice 2: the detail response carries a `timeline` and the walk asserts the approver's comment is on the `rejected` step and that `can_decide` is false afterwards — a decided request offering a decision panel is how a "Decide" button ends up on leave HR has already approved. Cancelling moves pending → 0 and the same dates become available again, which is the one thing a naive "refuse any overlap" check gets wrong. Both write an audit entry. An **approved** request cannot be cancelled: it is history, ended by the leave happening, not by pretending it was never agreed.)*
- [x] Attendance check-in/check-out records the right day, refuses a checkout without check-in and a second check-in the same day (idempotent for API callers). *(Slice 2d, and the verdict the walk owed finally arrived: **10/10 DB walks green** in 215 s. Ten cases, each a refusal or a fact rather than a rendering — the day recorded is the organization's day and not the browser's, `check_out` without a `check_in` is refused, a second punch is refused **with the punch it found** rather than a bare 409, an account holding no `hr.*` key punches and reads its own day, somebody else's month is refused in *both* directions, and the remaining two refusals the schema could answer without touching the clock. Run against a **private PostgreSQL on 5444** because the shared container every writer shares has been wedged in crash recovery for hours — restarting a shared database is not a local decision, and a walk that cannot connect is an unmeasured pass, not a failing one.)*
- [x] Monthly summary lists worked hours per employee and flags missing checkout and over/under hours, and the CSV export matches the grid. *(Slice 4b: the attendance report carries `days_present`, `minutes_worked`, `overtime_days`, `under_hours_days` and `missing_checkout_days` per employee, plus the summed missing-checkout figure the screen puts in a tile — **the minutes are formatted through the module's own `formatMinutes` and never summed again in the browser**, so the grid cell, the header tile and the CSV are three readers of one server-side projection. The CSV is derived from the **same JSON the screen rendered** (`reports::csv_for`), not from a second query, so the file a payroll import reads cannot describe a different organization than the table beside it; the walk reads one report twice and asserts a line per row and the same total. **The export is a fourth key, not a stricter read**: `hr.reports.read` opens the screen and `hr.reports.export` buys the file, and the walk asserts an account holding only the first gets 403 on the CSV and 200 on the JSON in the same call shape — reading a report and taking it out of the tenant are different acts. The four reports share one period picker because they answer four readings of the *same* period; four pages would mean a payroll run exporting four files covering four different months. The picker is served by `GET /hr/reports` and the renderer switches on the report discriminant **with no default arm**, so a fifth report without a case is a typecheck error rather than a blank table — and the QA pass asserts a table for every option the served picker offers.*)
- [x] Applying an onboarding template creates the items with due dates derived from the start date; ticking an item is reflected in the progress bar and emits completion when the last item is ticked. *(Slice 4, and the two halves the criterion names are walked separately because they fail differently. **The dates: an item with offset 0 is due on the start date, offset 3 is three days after it, and an item with *no* offset has `due_on: null` rather than "due today"** — "no deadline" is a real answer for "buy the coffee grinder" and a different thing from day 0. **The dates are stored, not derived on read**, and the walk that proves it edits the employee's start date afterwards and re-reads the checklist: an implementation that recomputes from the live start date passes the first assertion and silently re-dates every item the moment somebody corrects a typo, and a checklist whose due dates move under it is one nobody trusts. The offset is kept beside the date so "why is this due on the 9th?" needs no reconstruction. **The bar: the denominator is the row count and never the template's length.** The walk ticks all three, then adds a FOURTH item to the template, and asserts the bar is still 3/3 — an item the template gained since the checklist was applied is not on that person's checklist, and dividing by the template's length reports 75% for somebody who finished all three of the three things they were given. **The event fires once, on the transition**: the walk asserts zero events after two ticks, exactly one after the third, and *still* one after re-ticking the same item and after unticking the last one. The handler reads the checklist **before** the write (`checklist_for_item`) precisely so a re-read cannot see what another writer has since done — otherwise an automation waiting for "onboarding finished" runs once per tick, and an untick fires a completion for a checklist that just stopped being complete. The refused second apply is a 409 carrying `items: 3`, and the walk reads the list back to assert the refusal wrote nothing.*)
- [x] Document upload stores through media, downloads with the right name/type, and an expiring document produces one `hr.document.expiring` event. *(Slice 4b, in two halves that fail differently. **The reference, not the bytes**: a document row holds a `media_id` — the media pipeline's id, never the file — and removing the row is a `DELETE` that drops the **reference only**; the walk asserts the media row survives it, because a delete labelled "delete" on a screen of contracts is the moment somebody clicks it without reading, and a route that took the bytes with it would make that button a lie. The screen therefore labels the action *remove the reference* and the confirmation repeats it. **The sweep announces each document once**: the walk sweeps, then sweeps again, and asserts the second is empty **while the first still counts the rows it considered** — otherwise "nothing left to announce" and "the window was empty" look identical, and a sweep that is a `GET`, or whose once-ness is a property of the cron rather than of the table, sends the second reminder the moment anybody runs it twice. The screen shows a receipt for the same reason: it reports what was considered and what was announced, and a sweep that says so is the only way an operator can tell a working sweep from a broken one. **Dates travel as `YYYY-MM-DD`**: `due_on` shipped last tick as `[2026,61]` (a year and an ordinal), so this walk asserts a serialised document date is a string the module's own parser reads back, and that the parser refuses the array form outright. The list is a **cross-employee** screen behind its own `hr.documents.read` — it answers "whose contract expires next", which is a question about the organization, not about the person asking — and the walk reads a document belonging to an employee the reader is not, because a list scoped to the caller passes every other case in the suite.*)
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
