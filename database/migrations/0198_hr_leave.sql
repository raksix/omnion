-- Omnion · 0198 · HR: leave end to end (docs/requests/REQ-055, slice 2)
--
-- Balances and requests. The leave catalogue itself is in 0196, because a seed that created the
-- rows a later table referenced would have been a seed this migration could not prove; here the
-- two tables that read it arrive.
--
-- Four decisions worth stating, because each of them is a way this data could have lied.
--
-- 1. **The balance is RECOMPUTED, never incremented.** `used_days` and `pending_days` are a
--    projection of the request rows, not a counter someone adds to. The request's risk note says
--    this outright ("balance math must have one owner"), and the concrete failure an
--    increment-in-place design has is quiet: a request cancelled, rejected or re-approved moves
--    the number by whatever the code at the time added, and after a year the card reads 3 days
--    left against a list of requests that sum to 9. Every writer in the module recomputes from
--    `hr_leave_requests` inside the same transaction as the status change, so the card and the
--    list cannot disagree.
-- 2. **`numeric(6,2)`, and days are half-days, not days.** A half-day is 0.5 and the column has
--    to say so; `numeric(6,2)` also makes the subtraction exact, which `float8` would not (0.1 +
--    0.2 is a balance card that reads 0.30000000000000004 and a support ticket about it).
-- 3. **The overlap is a range check the database can also enforce.** `starts_on <= ends_on` is a
--    `check`, but "no two requests of one employee may overlap" needs the *other* row, so it is
--    left to the service inside a transaction — with the exclusion constraint deliberately NOT
--    used: a GiST range index would refuse an overlap for *any* status, including two REJECTED
--    requests sitting next to each other in an employee's history, which is not a conflict at all.
-- 4. **Cancelling keeps the row.** `cancelled_at` plus `leave_status = 'cancelled'`, for the same
--    reason termination does in slice 1: the row is the employee's history and a balance that
--    can be audited needs the request that moved it.
--
-- Additive throughout. 0196 is not altered: `hr_leave_types` already exists and is read, not
-- reshaped, so an installation upgrading into both migrations lands in the same state as one
-- created after them.
-- ---------------------------------------------------------------------------

-- ---------------------------------------------------------------------------
-- Balances
-- ---------------------------------------------------------------------------

create table hr_leave_balances (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    employee_id uuid not null references hr_employees (id) on delete cascade,
    leave_type_id uuid not null references hr_leave_types (id) on delete cascade,
    balance_year integer not null,
    -- What the organization promised. Seeded from the type's `annual_days` when the row is first
    -- written, and only then editable: an entitlement that silently re-read the catalogue on
    -- every read would make a mid-year change to a leave type rewrite everybody's history.
    entitled_days numeric(6, 2) not null default 0,
    -- Projections of the request rows. Recomputed by the service, never incremented here.
    used_days numeric(6, 2) not null default 0,
    pending_days numeric(6, 2) not null default 0,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    -- A year outside this range is a typo, not a policy.
    check (balance_year between 2000 and 2200),
    -- The two projections can never be negative. A balance is recomputed from requests, so a
    -- negative `used` means a bug in the recomputation rather than a person who took too much
    -- leave — and the database is the last place that bug should be caught.
    check (used_days >= 0),
    check (pending_days >= 0),
    check (entitled_days >= 0)
);

-- One row per employee, per type, per year. A second row for the same triple is not "an extra
-- balance", it is a second answer to the same question, and which one a card reads is arbitrary.
create unique index hr_leave_balances_employee_type_year_key
    on hr_leave_balances (employee_id, leave_type_id, balance_year);

comment on table hr_leave_balances is
    'Per-employee, per-type, per-year leave entitlement. used_days and pending_days are projections of hr_leave_requests, recomputed by the service.';

-- The balance card reads one employee across every type, which is the self-service query and the
-- most frequent read in the module.
create index hr_leave_balances_employee_year_idx
    on hr_leave_balances (employee_id, balance_year);

-- ---------------------------------------------------------------------------
-- Requests
-- ---------------------------------------------------------------------------

create table hr_leave_requests (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    employee_id uuid not null references hr_employees (id) on delete cascade,
    leave_type_id uuid not null references hr_leave_types (id) on delete restrict,
    starts_on date not null,
    ends_on date not null,
    -- The working days the request actually consumes, computed by the service from the
    -- organization's working days and stored so the number shown before submit and the number
    -- charged to the balance are the same number rather than two calculations of it.
    days numeric(6, 2) not null,
    half_day boolean not null default false,
    reason text not null default '',
    attachment_media_id uuid,
    -- `pending`, `approved`, `rejected` or `cancelled`.
    leave_status text not null default 'pending',
    -- The approval REQ-059 would own. Nullable and unread on purpose: leave must stay usable
    -- without that request installed, and the acceptance criteria ask for a direct decision with
    -- the same audit trail. A column whose value nothing reads is worse than no column.
    approval_request_id uuid,
    decided_by uuid,
    decided_at timestamptz,
    decision_comment text,
    cancelled_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    check (leave_status in ('pending', 'approved', 'rejected', 'cancelled')),
    check (ends_on >= starts_on),
    -- A request for no days is not a request; it is a form somebody opened and left.
    check (days > 0),
    -- A half-day only exists as a half-day. `from = to` is what makes 0.5 meanable, and a range
    -- that is "half" would be a rate the schema cannot represent.
    check ((half_day = false) or (starts_on = ends_on)),
    -- A decision that carries no time is not a decision that happened.
    check ((leave_status in ('pending', 'cancelled')) or decided_at is not null),
    -- A cancelled request is one that was cancelled, and nothing else may be.
    check ((leave_status = 'cancelled') = (cancelled_at is not null))
);

comment on table hr_leave_requests is
    'A leave request and its decision. The row is the employee''s history: cancelling and rejecting both keep it.';

-- The list screen's main query: one organization's requests, by status, soonest first.
create index hr_leave_requests_org_status_idx
    on hr_leave_requests (organization_id, leave_status, starts_on);

-- The overlap check and the employee's own request list both read one employee's ranges. This is
-- the index that makes "does this clash" cheap rather than a scan of every request in the
-- organization.
create index hr_leave_requests_employee_range_idx
    on hr_leave_requests (employee_id, starts_on, ends_on)
    where leave_status in ('pending', 'approved');

-- The absence calendar (the REQ's month grid) reads approved ranges for a window, so the index
-- is partial on exactly that status rather than carrying the rejected rows forever.
create index hr_leave_requests_absence_calendar_idx
    on hr_leave_requests (organization_id, starts_on, ends_on)
    where leave_status = 'approved';
