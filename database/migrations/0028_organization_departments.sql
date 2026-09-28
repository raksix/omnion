-- Omnion · 0028 · organization departments and department-scoped roles (REQ-005, slice 2)
--
-- Memberships (0019) answered "who belongs to this organization". This migration adds the
-- structure *inside* an organization, which the IAM level promised (docs/07-IAM.md §6) and that
-- `Scope::Department` has been able to express since 0011 without anything to point at:
--
-- * `departments` is an organization-scoped tree. `key` is stable (renaming a department must
--   not silently re-scope the roles bound to it) and shaped like a site key, because a role
--   binding stores a department as its `resource_id` string — the same string the resolver
--   compares against. A stable key is therefore a requirement, not a nicety.
-- * `department_members` says who is in which department.
--
-- No existing table loses a column and no constraint is tightened on one, so the migration is
-- safe to apply on a populated installation (docs/05-VERSIONING.md). `role_bindings` already
-- accepts `scope_type = 'department'` and carries the key in `resource_id`; the index added here
-- is the one that read.

create table departments (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    parent_id       uuid        references departments (id) on delete set null,
    key             text        not null,
    name            text        not null,
    description     text        not null default '',
    status          text        not null default 'active',
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    -- A department is addressed by key inside role bindings, so the key follows the same shape a
    -- site key does and cannot be renamed into something a binding would stop matching.
    constraint departments_key_format check (key ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    constraint departments_name_length check (char_length(name) between 1 and 120),
    constraint departments_description_length check (char_length(description) <= 1000),
    constraint departments_status_check check (status in ('active', 'archived')),
    -- A department is never its own parent. The deeper cycles (A → B → C → A) are refused by the
    -- store, which walks the ancestor chain inside the same transaction; the check here only
    -- catches the one-row mistake the database can see for free.
    constraint departments_not_own_parent check (parent_id is null or parent_id <> id)
);

-- The key is the address a role binding stores, so it is unique per organization.
create unique index departments_org_key_key on departments (organization_id, key);

create index departments_org_parent_idx on departments (organization_id, parent_id);
create index departments_org_status_idx on departments (organization_id, status);

-- Who is in which department. A person can be in several; the primary key keeps it to one row
-- per pair, so joining twice is a no-op rather than a duplicate grant.
create table department_members (
    department_id uuid        not null references departments (id) on delete cascade,
    user_id       uuid        not null references users (id) on delete cascade,
    created_at    timestamptz not null default now(),
    primary key (department_id, user_id)
);

-- The read behind "which departments is this person in", and behind the member drawer.
create index department_members_user_idx on department_members (user_id);

-- The liveness index of `role_bindings` keys on the subject, so a department binding is found
-- the same way as any other. This index answers the other half: "which roles are bound to this
-- department", which is what the Departments tab and the drawer read.
create index role_bindings_department_idx on role_bindings (organization_id, resource_id)
    where scope_type = 'department' and revoked_at is null;
