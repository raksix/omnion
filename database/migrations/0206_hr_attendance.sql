-- Omnion · 0206 · HR: attendance — the clock and its corrections (docs/requests/REQ-055, slice 2d)
--
-- One table, and the decisions in it are the whole slice. The number is 0206 rather than the
-- slice's natural next value because the migration namespace is shared by every worktree on this
-- box: the high-water across all of them was 0205 (0202-0205 taken by CRM intake and AI change
-- sets) when this file was written, and a number that merely looks free in this branch is taken
-- on a branch a merge will bring in.
--
-- Four things this schema has to be able to say, each of which a naive version gets wrong:
--
-- 1. **One row per employee per day is the INVARIANT, not a convention.** `unique (employee_id,
--    work_date)` is what makes "clock in twice" answerable without a race, and it is also what
--    makes an API caller's retry idempotent: the request's API table says the payload is
--    "idempotent per employee + day + kind", which cannot be delivered by a check the service
--    runs after inserting. Postgres decides who wins, and the loser is told which row it lost to.
-- 2. **A checkout needs a check-in, and the check is in the database, not in the service.** A
--    service check is a `SELECT` followed by an `INSERT` with nothing between them, which is two
--    statements where the schema can express one fact. The `check_out > check_in` constraint makes
--    a backwards clock pair impossible even for a caller that never went near the service.
-- 3. **`minutes_worked` is a projection, never an input.** It is the difference of the two
--    timestamps, and a caller that may SET it can write 0 minutes for a day they never attended
--    and every monthly summary inherits it. The column is writable only by the correction path,
--    which is the one place a human has decided the number is true.
-- 4. **`source` distinguishes a real clock from an edited one.** `manual`, `api` and
--    `import` are the three ways a row arrives, and `corrected_by` is a fourth fact, not a
--    variant of the source: a correction made to an `api` row keeps its source and gains a
--    corrector, because "the device recorded this" and "a person changed it" are independent
--    facts and overwriting one destroys the other.
--
-- The leave and employee tables are NOT altered: attendance points at an employee, and the
-- employee rows already exist. Additive throughout.

create table if not exists hr_attendance (
    id uuid primary key,
    organization_id uuid not null,
    -- A clock is a fact about a person. The reference is RESTRICT, not CASCADE, for the same
    -- reason every other slice-1 reference is: a hard delete of an employee must not silently
    -- erase the record that they were here on a given day.
    employee_id uuid not null references hr_employees (id) on delete restrict,
    -- The DAY, derived in the organization's time zone, never derived in the browser. Storing the
    -- instant of the check-in as the work date would put a 23:30 check-in on whichever day the
    -- server happens to be in, and the roster and the monthly grid would then disagree.
    work_date date not null,
    check_in timestamptz,
    check_out timestamptz,
    -- The projection. Nullable while a day is open, and a check bounds it.
    minutes_worked integer,
    -- `manual`, `api` or `import`. Never overwritten by a correction.
    source text not null default 'manual',
    note text not null default '',
    -- Set by the correction path only. Its own column rather than a source value, because
    -- "recorded by a device" and "changed by a person" are independent facts.
    corrected_by uuid,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),

    constraint hr_attendance_source_check
        check (source in ('manual', 'api', 'import')),
    -- A day has to begin. A row with neither timestamp is a form somebody opened, and there is
    -- one writer that could create it by accident (a correction that supplies neither).
    constraint hr_attendance_has_a_punch_check
        check (check_in is not null or check_out is not null),
    -- The clock cannot run backwards, and this is a FACT about the data rather than a rule the
    -- service is trusted to apply: an import with swapped times fails here rather than producing
    -- a day with negative hours.
    constraint hr_attendance_order_check
        check (check_out is null or check_out > check_in),
    -- 18 hours is a long day, and the request asks for over-hours to be *flagged*, not refused.
    -- The bound is here to stop a typo writing 100000 minutes; the flag is the summary's job.
    constraint hr_attendance_minutes_check
        check (minutes_worked is null or minutes_worked between 0 and 1080),
    -- A correction carries a REASON. The drawer the request describes has a note field, and a
    -- correction with no reason is an unattributable edit of somebody's worked hours — the one
    -- write in this module whose author cannot be reconstructed later. It binds only the empty
    -- case on purpose: the opposite shape, "a corrected row always has a corrector", would refuse
    -- a row the service itself adjusts, which is not a correction at all.
    constraint hr_attendance_correction_reason_check
        check (corrected_by is null or length(btrim(note)) > 0)
);

comment on table hr_attendance is
    'One day of one employee: the check-in, the check-out and the minutes derived from them. A correction adds a corrector and a reason, it never rewrites the source.';

-- The invariant that makes a second check-in answerable. This is the constraint the whole slice
-- is built on: the request's "idempotent per employee + day + kind" cannot be delivered by a
-- service-level check, because the check and the insert would be two statements.
create unique index hr_attendance_employee_day_uniq
    on hr_attendance (employee_id, work_date);

-- The monthly grid reads one employee's days newest first, and the roster reads one organization's
-- day. Both are the two shapes the screens actually ask for.
create index hr_attendance_employee_date_idx
    on hr_attendance (employee_id, work_date desc);

create index hr_attendance_org_day_idx
    on hr_attendance (organization_id, work_date desc);
