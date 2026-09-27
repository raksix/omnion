-- Omnion · 0011 · Advanced IAM: roles to the full model, subjects and scopes, MFA, security
-- policy, sign-in providers, provisioning and approvals (docs/07-IAM.md, REQ-006).
--
-- Additive by design (docs/05-VERSIONING.md): the v0 columns stay where they are and every new
-- column is nullable or defaulted, so a populated database keeps serving while the build loop
-- fills the surface in. The `role_bindings` subject/scope change is expand-then-contract —
-- `subject_type`/`subject_id` arrive next to `user_id`, are backfilled from it, and `user_id`
-- is kept for one release.
--
-- This migration carries the whole data model of the request: the role side it is used from the
-- first slice (role versions), the rest as the slices that need it land.

-- ---------------------------------------------------------------------------------------------
-- Extensions to the v0 tables
-- ---------------------------------------------------------------------------------------------

-- Sign-in bookkeeping and the ABAC subject attributes of an account (docs/07-IAM.md §11).
alter table users add column mfa_enforced boolean not null default false;
alter table users add column last_sign_in_at timestamptz;
alter table users add column failed_sign_in_count integer not null default 0;
alter table users add column locked_until timestamptz;
alter table users add column attributes jsonb not null default '{}'::jsonb;

-- Property lookups for ABAC conditions (`attributes->>'department' = 'sales'`).
create index users_attributes_idx on users using gin (attributes jsonb_path_ops);

-- Where a session ran, how it authenticated, and how it ended (docs/07-IAM.md §15).
alter table sessions add column device_id uuid;
alter table sessions add column auth_methods text[] not null default '{}';
alter table sessions add column absolute_expires_at timestamptz;
alter table sessions add column revoked_by uuid;
alter table sessions add column revoke_reason text;

-- Subjects and scopes (docs/07-IAM.md §6, §9, §14): a binding attaches a role to a user, a group
-- or a service account — one table, so a machine identity grants exactly the way a person does.
-- `resource_type`/`resource_id` carry resource-scoped bindings (a path glob such as `/blog/*`).
alter table role_bindings add column subject_type text not null default 'user';
alter table role_bindings add column subject_id uuid;
alter table role_bindings add column resource_type text;
alter table role_bindings add column resource_id text;

-- Backfill before the not-null is asked for: every existing binding is a user binding.
update role_bindings set subject_id = user_id where subject_id is null;

alter table role_bindings alter column subject_id set not null;
alter table role_bindings add constraint role_bindings_subject_type_check
    check (subject_type in ('user', 'group', 'service_account'));

-- Transitional shim (expand-then-contract): writers that predate the subject columns insert
-- `user_id` alone, so the row is completed from it. It is dropped together with `user_id` once
-- every writer sets `subject_type`/`subject_id` itself.
create or replace function role_bindings_fill_subject() returns trigger
language plpgsql as $$
begin
    if new.subject_id is null then
        new.subject_id := new.user_id;
    end if;
    return new;
end $$;

create trigger role_bindings_fill_subject
    before insert or update on role_bindings
    for each row execute function role_bindings_fill_subject();

-- Widen the scope ladder (department/module/resource join the v0 three). The old shape check is
-- replaced by one that still accepts every existing row.
alter table role_bindings drop constraint role_bindings_scope_type_check;
alter table role_bindings drop constraint role_bindings_scope_shape_check;
alter table role_bindings add constraint role_bindings_scope_type_check
    check (scope_type in ('global', 'organization', 'site', 'department', 'module', 'resource'));
alter table role_bindings add constraint role_bindings_scope_shape_check check (
    (scope_type = 'global' and organization_id is null and site_id is null
        and resource_type is null and resource_id is null)
    or (scope_type = 'organization' and organization_id is not null and site_id is null
        and resource_type is null and resource_id is null)
    or (scope_type = 'site' and site_id is not null and resource_type is null and resource_id is null)
    or (scope_type = 'department' and organization_id is not null and resource_id is not null
        and resource_type is null)
    or (scope_type in ('module', 'resource') and resource_id is not null
        and (site_id is not null or organization_id is not null))
);

-- ---------------------------------------------------------------------------------------------
-- Roles: version history with diffs (docs/07-IAM.md §4, §17)
-- ---------------------------------------------------------------------------------------------

-- Every role change appends one row: the role's own fields, its permission set as read at that
-- moment, who changed it and why. The diff of two consecutive rows is what the history tab draws.
create table role_versions (
    id                  uuid        primary key default gen_random_uuid(),
    role_id             uuid        not null references roles (id) on delete cascade,
    version             integer     not null,
    name                text        not null,
    description         text        not null default '',
    priority            integer     not null,
    inherits_role_id    uuid,
    inherit_permissions boolean     not null default true,
    permissions         jsonb       not null default '[]'::jsonb,
    change              text        not null,
    changed_by          uuid        references users (id) on delete set null,
    created_at          timestamptz not null default now(),
    constraint role_versions_change_check
        check (change in ('created', 'updated', 'permissions', 'duplicated', 'restored')),
    constraint role_versions_positive check (version >= 1),
    unique (role_id, version)
);

create index role_versions_role_idx on role_versions (role_id, version desc);

-- ---------------------------------------------------------------------------------------------
-- Sessions, devices and MFA (docs/07-IAM.md §15)
-- ---------------------------------------------------------------------------------------------

-- Known devices: the panel remembers a device (fingerprint hash, label, platform) and a trust
-- window, so a new device can raise a notice while a trusted one does not.
create table user_devices (
    id              uuid        primary key default gen_random_uuid(),
    user_id         uuid        not null references users (id) on delete cascade,
    fingerprint     text        not null,
    label           text        not null default '',
    platform        text        not null default '',
    browser         text        not null default '',
    first_seen_at   timestamptz not null default now(),
    last_seen_at    timestamptz not null default now(),
    trusted_until   timestamptz,
    revoked_at      timestamptz,
    created_at      timestamptz not null default now(),
    unique (user_id, fingerprint)
);

create index user_devices_user_idx on user_devices (user_id) where revoked_at is null;

-- Enrolled second factors: TOTP (secret ciphertext), WebAuthn/passkey (credential id, public
-- key, sign count, transports) and single-use recovery codes (hashed, one row per code).
create table mfa_factors (
    id              uuid        primary key default gen_random_uuid(),
    user_id         uuid        not null references users (id) on delete cascade,
    kind            text        not null,
    label           text        not null default '',
    secret_ciphertext text,
    credential_id   text,
    public_key      text,
    sign_count      bigint      not null default 0,
    transports      text[]      not null default '{}',
    confirmed_at    timestamptz,
    last_used_at    timestamptz,
    created_at      timestamptz not null default now(),
    revoked_at      timestamptz,
    constraint mfa_factors_kind_check check (kind in ('totp', 'webauthn', 'recovery')),
    constraint mfa_factors_confirm_shape check (
        (kind = 'totp' and secret_ciphertext is not null)
        or (kind = 'webauthn' and credential_id is not null and public_key is not null)
        or (kind = 'recovery')
    )
);

create unique index mfa_factors_credential_key on mfa_factors (credential_id)
    where credential_id is not null and revoked_at is null;
create index mfa_factors_user_idx on mfa_factors (user_id) where revoked_at is null;

create table mfa_recovery_codes (
    id          uuid        primary key default gen_random_uuid(),
    user_id     uuid        not null references users (id) on delete cascade,
    code_hash   text        not null,
    used_at     timestamptz,
    created_at  timestamptz not null default now(),
    unique (user_id, code_hash)
);

create index mfa_recovery_codes_user_idx on mfa_recovery_codes (user_id);

-- ---------------------------------------------------------------------------------------------
-- Groups, service accounts, policies (docs/07-IAM.md §8, §9, §11)
-- ---------------------------------------------------------------------------------------------

-- Teams: a group carries roles through ordinary bindings, so membership grants the way a personal
-- binding does.
create table groups (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null,
    slug            text        not null,
    description     text        not null default '',
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint groups_name_not_blank check (length(btrim(name)) > 0),
    constraint groups_slug_format check (slug ~ '^[a-z]([a-z0-9-]*[a-z0-9])?$')
);

create unique index groups_organization_name_key on groups (organization_id, lower(name));
create unique index groups_organization_slug_key on groups (organization_id, slug);

create table group_members (
    group_id    uuid        not null references groups (id) on delete cascade,
    user_id     uuid        not null references users (id) on delete cascade,
    added_by    uuid        references users (id) on delete set null,
    created_at  timestamptz not null default now(),
    primary key (group_id, user_id)
);

create index group_members_user_idx on group_members (user_id);

-- Machine identities: a prefix makes the key findable, the hash is what is compared.
create table service_accounts (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null,
    description     text        not null default '',
    prefix          text        not null,
    created_by      uuid        references users (id) on delete set null,
    last_used_at    timestamptz,
    expires_at      timestamptz,
    disabled_at     timestamptz,
    created_at      timestamptz not null default now(),
    constraint service_accounts_name_not_blank check (length(btrim(name)) > 0)
);

create unique index service_accounts_organization_name_key
    on service_accounts (organization_id, lower(name));
create unique index service_accounts_prefix_key on service_accounts (prefix);

create table service_account_keys (
    id                  uuid        primary key default gen_random_uuid(),
    service_account_id  uuid        not null references service_accounts (id) on delete cascade,
    prefix              text        not null,
    secret_hash         text        not null,
    label               text        not null default '',
    expires_at          timestamptz,
    last_used_at        timestamptz,
    revoked_at          timestamptz,
    created_at          timestamptz not null default now(),
    unique (prefix)
);

create index service_account_keys_account_idx on service_account_keys (service_account_id)
    where revoked_at is null;

-- ABAC policies (docs/07-IAM.md §11): evaluated after RBAC, deny wins, a missing attribute
-- compares as null. `conditions` is the condition tree the builder writes.
create table policies (
    id                  uuid        primary key default gen_random_uuid(),
    organization_id     uuid        not null references organizations (id) on delete cascade,
    name                text        not null,
    description         text        not null default '',
    effect              text        not null,
    priority            integer     not null default 500,
    conditions          jsonb       not null default '{"all":[]}'::jsonb,
    target_permissions  text[]      not null default '{}',
    enabled             boolean     not null default true,
    version             integer     not null default 1,
    created_by          uuid        references users (id) on delete set null,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint policies_effect_check check (effect in ('allow', 'deny')),
    constraint policies_name_not_blank check (length(btrim(name)) > 0),
    constraint policies_priority_range check (priority between 0 and 1000)
);

create index policies_org_enabled_idx on policies (organization_id, enabled, priority);
create index policies_target_permissions_idx on policies using gin (target_permissions);

create table policy_versions (
    id              uuid        primary key default gen_random_uuid(),
    policy_id       uuid        not null references policies (id) on delete cascade,
    version         integer     not null,
    effect          text        not null,
    priority        integer     not null,
    conditions      jsonb       not null,
    target_permissions text[]   not null default '{}',
    enabled         boolean     not null,
    changed_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    unique (policy_id, version)
);

-- ---------------------------------------------------------------------------------------------
-- Security policy, sign-in attempts and providers (docs/07-IAM.md §12, §13)
-- ---------------------------------------------------------------------------------------------

-- One row per organization: the document the security screen edits and every sign-in path reads.
-- Defaults are the conservative ones the request names.
create table security_policies (
    organization_id         uuid        primary key references organizations (id) on delete cascade,
    password_min_length     integer     not null default 10,
    password_require_classes integer   not null default 3,
    password_history        integer     not null default 5,
    password_expiry_days    integer     not null default 0,
    lockout_attempts        integer     not null default 10,
    lockout_minutes         integer     not null default 15,
    ip_allowlist            cidr[]      not null default '{}',
    ip_denylist             cidr[]      not null default '{}',
    session_idle_minutes    integer     not null default 120,
    session_absolute_days   integer     not null default 30,
    session_concurrent_max  integer     not null default 10,
    device_trust_days       integer     not null default 30,
    mfa_required            boolean     not null default false,
    updated_by              uuid        references users (id) on delete set null,
    updated_at              timestamptz not null default now(),
    constraint security_policies_password_length_range check (password_min_length between 8 and 128),
    constraint security_policies_password_classes_range check (password_require_classes between 1 and 4),
    constraint security_policies_password_history_range check (password_history between 0 and 24),
    constraint security_policies_password_expiry_range check (password_expiry_days between 0 and 730),
    constraint security_policies_lockout_attempts_range check (lockout_attempts between 3 and 50),
    constraint security_policies_lockout_minutes_range check (lockout_minutes between 1 and 1440),
    constraint security_policies_idle_minutes_range check (session_idle_minutes between 5 and 10080),
    constraint security_policies_absolute_days_range check (session_absolute_days between 1 and 365),
    constraint security_policies_concurrent_range check (session_concurrent_max between 1 and 100),
    constraint security_policies_device_days_range check (device_trust_days between 0 and 365)
);

-- Attempts are recorded per account and per address, so lockout and the security centre can both
-- read one table. No password material ever lands here.
create table sign_in_attempts (
    id          bigserial   primary key,
    email       text        not null,
    user_id     uuid        references users (id) on delete set null,
    organization_id uuid    references organizations (id) on delete set null,
    ip_address  inet,
    user_agent  text,
    outcome     text        not null,
    reason      text,
    created_at  timestamptz not null default now(),
    constraint sign_in_attempts_outcome_check
        check (outcome in ('success', 'failed', 'locked', 'blocked', 'mfa_required'))
);

create index sign_in_attempts_email_idx on sign_in_attempts (lower(email), created_at desc);
create index sign_in_attempts_ip_idx on sign_in_attempts (ip_address, created_at desc);
create index sign_in_attempts_created_idx on sign_in_attempts (created_at desc);

-- Sign-in providers per organization: OIDC, generic OAuth2 and SAML 2.0. Credentials live behind
-- `secret_ref` — an environment name or a secret-store key, never the value itself.
create table auth_providers (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    slug            text        not null,
    kind            text        not null,
    name            text        not null,
    config          jsonb       not null default '{}'::jsonb,
    secret_ref      text,
    scopes          text[]      not null default '{}',
    group_claim     text,
    default_role_id uuid        references roles (id) on delete set null,
    jit_enabled     boolean     not null default true,
    enabled         boolean     not null default true,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint auth_providers_kind_check check (kind in ('oidc', 'oauth2', 'saml')),
    constraint auth_providers_slug_format check (slug ~ '^[a-z]([a-z0-9-]*[a-z0-9])?$')
);

create unique index auth_providers_organization_slug_key on auth_providers (organization_id, slug);

-- ---------------------------------------------------------------------------------------------
-- Permission requests, provisioning tokens and their log (docs/07-IAM.md §16, §19)
-- ---------------------------------------------------------------------------------------------

create table permission_requests (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    requester_id    uuid        not null references users (id) on delete cascade,
    permission_key  text        not null,
    resource_type   text,
    resource_id     text,
    justification   text        not null default '',
    status          text        not null default 'pending',
    decided_by      uuid        references users (id) on delete set null,
    decided_at      timestamptz,
    decision_note   text,
    grant_minutes   integer,
    binding_id      uuid        references role_bindings (id) on delete set null,
    created_at      timestamptz not null default now(),
    constraint permission_requests_status_check
        check (status in ('pending', 'approved', 'rejected', 'expired')),
    constraint permission_requests_grant_minutes_range
        check (grant_minutes is null or grant_minutes between 5 and 43200)
);

create index permission_requests_org_status_idx on permission_requests (organization_id, status, created_at desc);
create index permission_requests_requester_idx on permission_requests (requester_id, created_at desc);

-- SCIM provisioning: one token per organization (hashed, rotatable) and the sync log the
-- provisioning screen reads. The log keeps ids and outcomes, never personal payloads.
create table provisioning_tokens (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null default '',
    prefix          text        not null,
    token_hash      text        not null,
    created_by      uuid        references users (id) on delete set null,
    last_used_at    timestamptz,
    revoked_at      timestamptz,
    created_at      timestamptz not null default now(),
    unique (prefix)
);

create index provisioning_tokens_org_idx on provisioning_tokens (organization_id) where revoked_at is null;

create table provisioning_log (
    id              bigserial   primary key,
    organization_id uuid        not null references organizations (id) on delete cascade,
    direction       text        not null,
    resource        text        not null,
    external_id     text,
    entity_id       uuid,
    action          text        not null,
    outcome         text        not null,
    detail          text,
    created_at      timestamptz not null default now(),
    constraint provisioning_log_direction_check check (direction in ('inbound', 'outbound')),
    constraint provisioning_log_resource_check check (resource in ('user', 'group')),
    constraint provisioning_log_outcome_check check (outcome in ('created', 'updated', 'deactivated', 'failed', 'skipped'))
);

create index provisioning_log_org_idx on provisioning_log (organization_id, created_at desc);

-- ---------------------------------------------------------------------------------------------
-- Indexes the new subject/expiry columns need, and the two subjects that must be findable
-- ---------------------------------------------------------------------------------------------

create index role_bindings_subject_idx on role_bindings (subject_type, subject_id)
    where revoked_at is null;
create index role_bindings_expires_idx on role_bindings (expires_at) where revoked_at is null;

-- ---------------------------------------------------------------------------------------------
-- Seeds
-- ---------------------------------------------------------------------------------------------

-- Every existing organization gets the conservative policy row; new organizations are seeded by
-- the service on creation (the same defaults, one place in code).
insert into security_policies (organization_id)
select id from organizations
on conflict (organization_id) do nothing;
