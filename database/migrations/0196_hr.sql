-- Omnion · 0196 · HR: the people core (docs/requests/REQ-055, slice 1)
--
-- Employees, departments and the org chart they make. Nothing else yet: leave, attendance and
-- onboarding are slices 2-4 and every one of them is a row that points at an employee, so the
-- employee record and the rules that decide who may read it had to be right first.
--
-- Four decisions worth stating, because each of them is a way this data could have lied.
--
-- 1. **Payroll is not in the schema at all.** The request's risk note asks for salary, bank
--    details and tax filings to stay out, and "we will not render them" is not the same promise
--    as "they are not stored": a column that exists is a column a future export, a future report
--    or a future backup can carry. A later payroll request brings its own controls.
--
-- 2. **The personal fields live, and they are gated in code rather than here.** personal_email,
--    personal_phone, address and emergency_contact are real columns — an HR officer has to be
--    able to *store* an emergency contact, or the field is theatre. What the schema cannot do is
--    decide who may read them, so the column set is listed once in `modules::hr::model` and the
--    list, the detail and the export all drop the same keys. A gate that lived only in the
--    schema would be bypassed by every route that selects the row.
--
-- 3. **Termination is a status, never a delete.** `hr.employees` is referenced by leave requests,
--    by attendance and (in slice 4) by onboarding items, so a hard delete would either cascade
--    a person's whole history away or fail on a restrict. The record stays, its status becomes
--    `terminated`, and the end date is recorded. That is also what makes a turnover report
--    possible at all.
--
-- 4. **The seed is a TRIGGER on `organizations`, not a body of this migration.** REQ-051 wrote
--    the CRM pipeline seed as a call inside the `insert` that created the function, so every
--    organization born after that statement owned no pipeline and the board answered 404 for a
--    week. Accounting fixed the same mistake in 0167. It is not repeated a third time: a function
--    created by this migration is owned by this migration, and the trigger is what applies it, so
--    a tenant created while this migration is already installed still gets a leave catalogue and
--    a root department.
--
-- Additive throughout: no existing table is altered, and nothing is seeded for an organization
-- that already exists. Those are backfilled at the end from the same function the trigger calls,
-- so an installation upgrading into this migration is in the same state as one created after it.

-- ---------------------------------------------------------------------------
-- Departments
-- ---------------------------------------------------------------------------

create table hr_departments (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    code text,
    -- The tree is a parent pointer rather than a materialized path: a path would have to be
    -- rewritten for every descendant on every move, and moving a department is a screen action.
    parent_id uuid references hr_departments (id) on delete restrict,
    -- A department head is an **employee**, not a user: an organization may hire a manager who
    -- has no platform account at all, and the chart must still be able to name them. `null` until
    -- an employee exists to fill it.
    manager_employee_id uuid,
    description text,
    active boolean not null default true,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    -- A department may not be its own parent. The *descendant* case is refused in the service
    -- (a constraint cannot see the subtree), and this is only the fixed point.
    check (parent_id is null or parent_id <> id),
    unique (organization_id, lower(name))
);

comment on table hr_departments is
    'Departments as a tree. A department with members or children is renamed or merged, never deleted.';

create index hr_departments_org_parent_idx
    on hr_departments (organization_id, parent_id, lower(name));

-- ---------------------------------------------------------------------------
-- Employees
-- ---------------------------------------------------------------------------

create table hr_employees (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    employee_no text not null,
    -- The platform account, when the employee has one. `null` is the normal case for a person who
    -- works somewhere but does not log in, so this is deliberately not a foreign key to `users`:
    -- the HR module is a module and the core is not its parent.
    user_id uuid,
    first_name text not null,
    last_name text not null,
    work_email text not null,
    phone text,
    position text not null,
    department_id uuid not null references hr_departments (id) on delete restrict,
    manager_id uuid references hr_employees (id) on delete set null,
    employment_type text not null,
    start_date date not null,
    end_date date,
    employee_status text not null default 'active',
    location text,
    -- The gated personal block. Stored, never served without `hr.employees.sensitive.read`.
    personal_email text,
    personal_phone text,
    address text,
    emergency_contact text,
    notes text not null default '',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    check (employment_type in ('full_time', 'part_time', 'contract', 'intern')),
    check (employee_status in ('active', 'on_leave', 'terminated')),
    -- An end date before the start is a data-entry slip the schema can catch for free.
    check (end_date is null or end_date >= start_date),
    -- A contract has to say when it ends, otherwise "this contract ended" is a question the
    -- record cannot answer. Not a check constraint: the dependency is on employment_type, and
    -- the service is what enforces it with a message the form can show.
    unique (organization_id, employee_no),
    -- Case-insensitive on the address, because a work e-mail is not case-sensitive in practice
    -- and two spellings of one mailbox would be two employees.
    unique (organization_id, lower(work_email))
);

comment on table hr_employees is
    'People. Termination is a status and an end date, never a delete; salary and bank details are not in this schema at all.';

create index hr_employees_org_department_idx
    on hr_employees (organization_id, department_id, employee_status);

create index hr_employees_org_name_idx
    on hr_employees (organization_id, lower(last_name), lower(first_name));

-- Partial: the manager filter is only ever asked about people who are still there, and the index
-- stays small for an organization with a long history of leavers.
create index hr_employees_org_manager_idx
    on hr_employees (organization_id, manager_id)
    where employee_status = 'active';

create index hr_employees_user_idx
    on hr_employees (organization_id, user_id)
    where user_id is not null;

-- The self-service routes resolve "my record" by user id on every request, and without this the
-- lookup is a sequential scan of the directory on the most frequent query in the module.
create unique index hr_employees_org_user_key
    on hr_employees (organization_id, user_id)
    where user_id is not null;

-- ---------------------------------------------------------------------------
-- Documents
-- ---------------------------------------------------------------------------

create table hr_documents (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    employee_id uuid not null references hr_employees (id) on delete cascade,
    kind text not null,
    title text not null,
    -- The bytes live in the media pipeline (REQ-010); this is the reference. A document row
    -- without a media id would be a file the platform cannot produce.
    media_id uuid not null,
    expires_on date,
    acknowledged_at timestamptz,
    uploaded_by uuid,
    created_at timestamptz not null default now(),
    check (kind in ('contract', 'id', 'certificate', 'other'))
);

comment on table hr_documents is
    'Employee documents by reference to the media pipeline. The bytes are never stored here.';

create index hr_documents_employee_idx
    on hr_documents (employee_id, created_at desc);

-- The expiry sweep reads the rows due in the next 30 days and nothing else, so the index is on
-- the future and the kind.
create index hr_documents_expiry_idx
    on hr_documents (organization_id, expires_on)
    where expires_on is not null;

-- The department head is an employee, and that reference can only be added once `hr_employees`
-- exists — hence the `alter` rather than the constraint in the `create table` above.
alter table hr_departments
    add constraint hr_departments_manager_employee_fk
    foreign key (manager_employee_id) references hr_employees (id) on delete set null;

-- ---------------------------------------------------------------------------
-- The seed: a leave catalogue, a root department and a starter template
-- ---------------------------------------------------------------------------
--
-- The leave **catalogue** and the onboarding **template** are here in slice 1 even though the
-- requests, balances and checklists that use them are not, because the seed has to be seedable
-- now: a migration that seeded what a *later* migration creates could not be proved, and the
-- acceptance criterion for this REQ asks for exactly these three seeds. The tables that
-- reference them (requests in slice 2, items in slice 4) arrive in their own migrations.

create table hr_leave_types (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    code text not null,
    paid boolean not null default true,
    -- `numeric(6,2)` and never an integer: half-days are half the point of the module, and a
    -- schema that could not store 0.5 would make the half-day toggle a lie.
    annual_days numeric(6, 2) not null default 0,
    requires_approval boolean not null default true,
    -- Whether a request may exceed what is left. Off by default because a balance is a promise
    -- the organization made in writing, and exceeding it silently is how a policy stops meaning
    -- anything.
    allow_negative boolean not null default false,
    active boolean not null default true,
    created_at timestamptz not null default now(),
    unique (organization_id, lower(code)),
    check (annual_days >= 0 and annual_days <= 366)
);

comment on table hr_leave_types is
    'The leave catalogue: entitlement, whether approval is needed, and whether a negative balance is allowed.';

create table hr_onboarding_templates (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    -- The ordered items as one document. A template is edited and read as a whole — a row per
    -- item would make "reorder" three writes and "apply" a transaction over rows nobody edits
    -- individually — so the shape is validated in the service, where the message can name the
    -- item that is wrong.
    items jsonb not null default '[]'::jsonb,
    active boolean not null default true,
    created_at timestamptz not null default now(),
    unique (organization_id, lower(name)),
    check (jsonb_typeof(items) = 'array')
);

comment on table hr_onboarding_templates is
    'Reusable onboarding checklists. Applying one materialises its items onto an employee in slice 4.';

create or replace function hr_seed_organization()
returns trigger
language plpgsql
as $$
begin
    -- One root department per organization, so the employee form's required department field has
    -- something to offer on a tenant created a second after this migration installed.
    insert into hr_departments (organization_id, name, code, description)
    values (new.id, 'General', 'GEN', 'The organization''s root department.')
    on conflict (organization_id, lower(name)) do nothing;

    -- Three leave types as **editable examples**, not a policy: 14 days of annual leave is what
    -- most organizations start from, and a seed that encoded a law would be wrong in half the
    -- world. `unpaid` carries no entitlement at all, which is what "unpaid" means.
    insert into hr_leave_types (organization_id, name, code, paid, annual_days, requires_approval)
    values
        (new.id, 'Annual',  'ANNUAL',  true,  14, true),
        (new.id, 'Sick',    'SICK',    true,   0, true),
        (new.id, 'Unpaid',  'UNPAID',  false,  0, true)
    on conflict (organization_id, lower(code)) do nothing;

    -- The starter template the onboarding screen offers, with the three items everybody has to do.
    insert into hr_onboarding_templates (organization_id, name, items)
    values (
        new.id,
        'New starter',
        '[
            {"title": "Sign the contract", "owner_role": "hr", "due_offset_days": 0, "requires_file": true},
            {"title": "Collect identification", "owner_role": "hr", "due_offset_days": 3, "requires_file": true},
            {"title": "Equipment and accounts", "owner_role": "it", "due_offset_days": 5, "requires_file": false}
        ]'::jsonb
    )
    on conflict (organization_id, lower(name)) do nothing;

    return new;
end;
$$;

create trigger organizations_hr_seed
    after insert on organizations
    for each row
    execute function hr_seed_organization();

-- The upgrade path. An organization that exists now predates the trigger, so it has no root
-- department and no catalogue; the SAME function is what fills it, which is the point of putting
-- the rule in a function rather than in this migration's own body. Running this twice is a no-op
-- (`on conflict do nothing` on the two unique sets).
insert into hr_departments (organization_id, name, code, description)
select o.id, 'General', 'GEN', 'The organization''s root department.'
from organizations o
on conflict (organization_id, lower(name)) do nothing;

insert into hr_leave_types (organization_id, name, code, paid, annual_days, requires_approval)
select o.id, t.name, t.code, t.paid, t.annual_days, true
from organizations o
cross join (values
    ('Annual', 'ANNUAL', true,  14::numeric),
    ('Sick',   'SICK',   true,   0::numeric),
    ('Unpaid', 'UNPAID', false,  0::numeric)
) as t(name, code, paid, annual_days)
on conflict (organization_id, lower(code)) do nothing;

insert into hr_onboarding_templates (organization_id, name, items)
select o.id, 'New starter',
       '[
            {"title": "Sign the contract", "owner_role": "hr", "due_offset_days": 0, "requires_file": true},
            {"title": "Collect identification", "owner_role": "hr", "due_offset_days": 3, "requires_file": true},
            {"title": "Equipment and accounts", "owner_role": "it", "due_offset_days": 5, "requires_file": false}
        ]'::jsonb
from organizations o
on conflict (organization_id, lower(name)) do nothing;
