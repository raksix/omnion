-- Omnion · 0001 · initial schema
--
-- The foundation of the platform database (docs/07-IAM.md): organizations, users, sessions
-- and the audit log. Later phases extend this schema in new migrations — released
-- migrations are append-only (docs/05-VERSIONING.md).

create extension if not exists "pgcrypto";

-- Organizations are the top-level tenant boundary: sites, roles and content hang off one
-- organization (docs/07-IAM.md §7).
create table organizations (
    id          uuid        primary key default gen_random_uuid(),
    name        text        not null,
    slug        text        not null,
    status      text        not null default 'active',
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    constraint organizations_slug_key unique (slug),
    constraint organizations_slug_format check (slug ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    constraint organizations_status_check check (status in ('active', 'suspended', 'archived'))
);

-- Global identity table. A user can later belong to several organizations through
-- memberships; `organization_id` is the user's primary organization (null = platform level).
create table users (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        references organizations (id) on delete set null,
    email           text        not null,
    password_hash   text,
    display_name    text        not null default '',
    status          text        not null default 'active',
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint users_status_check check (status in ('active', 'invited', 'disabled'))
);

-- Case-insensitive uniqueness: addresses differing only in case are one account.
create unique index users_email_lower_key on users (lower(email));
create index users_organization_id_idx on users (organization_id);

-- Server-side sessions. Only the hash of the session token is stored, never the token.
create table sessions (
    id           uuid        primary key default gen_random_uuid(),
    user_id      uuid        not null references users (id) on delete cascade,
    token_hash   text        not null,
    user_agent   text,
    ip_address   inet,
    created_at   timestamptz not null default now(),
    last_seen_at timestamptz,
    expires_at   timestamptz not null,
    revoked_at   timestamptz
);

create unique index sessions_token_hash_key on sessions (token_hash);
create index sessions_user_id_idx on sessions (user_id);
create index sessions_expires_at_idx on sessions (expires_at);

-- Append-only audit trail (docs/07-IAM.md). The AI Hub reuses the same chain for agent
-- actions, hence `actor_type` (docs/06-AI-HUB.md).
create table audit_log (
    id              bigint      generated always as identity primary key,
    organization_id uuid        references organizations (id) on delete set null,
    actor_user_id   uuid        references users (id) on delete set null,
    actor_type      text        not null default 'user',
    action          text        not null,
    target_type     text,
    target_id       text,
    metadata        jsonb       not null default '{}'::jsonb,
    ip_address      inet,
    created_at      timestamptz not null default now(),
    constraint audit_log_actor_type_check check (actor_type in ('user', 'agent', 'service', 'system'))
);

create index audit_log_org_created_at_idx on audit_log (organization_id, created_at desc);
create index audit_log_actor_user_id_idx on audit_log (actor_user_id);
create index audit_log_action_idx on audit_log (action);
