-- Omnion · 0002 · IAM: permission catalogue, custom roles and scoped role bindings
--
-- v0 of the access-control model (docs/07-IAM.md). It carries: the granular permission
-- catalogue; fully custom roles with a priority value, an inheritance link and an explicit
-- allow/deny set; and role bindings, which attach a role to a user at a scope (global /
-- organization / site). Deciding on a request is RBAC only — the ABAC policy engine, approval
-- flows and the remaining scope levels (department/module/resource) arrive in later phases.
-- Released migrations are append-only (docs/05-VERSIONING.md).

-- The catalogue of every permission the platform knows. Rows are seeded from code
-- (`crates/permissions`, the single source of truth) so role entries can hold a foreign key.
create table permissions (
    key         text        primary key,
    category    text        not null,
    description text        not null,
    created_at  timestamptz not null default now(),
    constraint permissions_key_format check (key ~ '^[a-z][a-z0-9]*(\.[a-z0-9]+)+$'),
    constraint permissions_category_format check (category ~ '^[a-z][a-z0-9-]*$'),
    constraint permissions_description_not_blank check (length(btrim(description)) > 0)
);

-- Roles. `organization_id is null` marks a platform (system) role — the six base roles from
-- docs/07-IAM.md §3 — while an organization id marks a role the customer created and owns.
-- `inherits_role_id` is the inheritance link (docs/07-IAM.md §4) and `inherit_permissions`
-- turns it off without dropping the link.
create table roles (
    id                  uuid        primary key default gen_random_uuid(),
    organization_id     uuid        references organizations (id) on delete cascade,
    key                 text        not null,
    name                text        not null,
    description         text        not null default '',
    priority            integer     not null,
    inherits_role_id    uuid        references roles (id) on delete set null,
    inherit_permissions boolean     not null default true,
    is_system           boolean     not null default false,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint roles_key_format check (key ~ '^[a-z]([a-z0-9-]*[a-z0-9])?$'),
    constraint roles_name_not_blank check (length(btrim(name)) > 0),
    constraint roles_priority_range check (priority between 0 and 1000),
    constraint roles_no_self_inheritance check (inherits_role_id is null or inherits_role_id <> id)
);

-- One key per scope: platform roles are unique among themselves, customer roles per
-- organization.
create unique index roles_system_key_key on roles (key) where organization_id is null;
create unique index roles_organization_key_key on roles (organization_id, key)
    where organization_id is not null;
create index roles_inherits_role_id_idx on roles (inherits_role_id);

-- Explicit allow/deny entries (docs/07-IAM.md §5). A role can hold at most one entry per
-- permission, so the same key is never both allowed and denied inside one role.
create table role_permissions (
    role_id        uuid        not null references roles (id) on delete cascade,
    permission_key text        not null references permissions (key) on delete cascade,
    effect         text        not null,
    created_at     timestamptz not null default now(),
    primary key (role_id, permission_key),
    constraint role_permissions_effect_check check (effect in ('allow', 'deny'))
);

-- Role bindings attach a role to a user at a scope (docs/07-IAM.md §6, §16). `expires_at`
-- gives temporary roles: an expired binding stops counting without being deleted.
-- `site_id` receives its foreign key together with the sites table (P04) — until then the
-- identifier is stored as handed in by the caller.
create table role_bindings (
    id              uuid        primary key default gen_random_uuid(),
    role_id         uuid        not null references roles (id) on delete cascade,
    user_id         uuid        not null references users (id) on delete cascade,
    scope_type      text        not null,
    organization_id uuid        references organizations (id) on delete cascade,
    site_id         uuid,
    granted_by      uuid        references users (id) on delete set null,
    expires_at      timestamptz,
    revoked_at      timestamptz,
    created_at      timestamptz not null default now(),
    constraint role_bindings_scope_type_check check (scope_type in ('global', 'organization', 'site')),
    constraint role_bindings_scope_shape_check check (
        (scope_type = 'global' and organization_id is null and site_id is null)
        or (scope_type = 'organization' and organization_id is not null and site_id is null)
        or (scope_type = 'site' and site_id is not null)
    )
);

-- A user can hold the same role at the same scope only once while the binding is alive.
create unique index role_bindings_active_key on role_bindings (
    role_id,
    user_id,
    scope_type,
    coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid),
    coalesce(site_id, '00000000-0000-0000-0000-000000000000'::uuid)
) where revoked_at is null;
create index role_bindings_user_id_idx on role_bindings (user_id) where revoked_at is null;
create index role_bindings_role_id_idx on role_bindings (role_id);
create index role_bindings_organization_id_idx on role_bindings (organization_id);
