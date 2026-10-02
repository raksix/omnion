-- Omnion · 0019 · Secret store foundation + key ring, typed credentials, slots and leases
-- (REQ-125, slice 1 · docs/requests/REQ-125-secrets-management-depth.md).
--
-- Additive by design (docs/05-VERSIONING.md): nothing here rewrites an existing table.
--
-- Two requests touch this area. REQ-037 is the secrets manager itself (secret CRUD, provider
-- connections, bindings, rotation policy, `secrets.reveal`); REQ-125 is the depth layer over it
-- (key hierarchy with rotation, typed credential profiles, slot assignment, leases, deployment
-- keys, audit depth). REQ-037 is still queued, so this migration creates the small foundation
-- REQ-125's structures reference and nothing more: the `secrets` row and its `secret_versions`
-- envelopes with the `key_id` that names the root key each version was sealed under. REQ-037
-- extends these two tables additively (providers, bindings, rotation policy) and builds its
-- screens on the same rows — it never has to migrate them again.
--
-- The key hierarchy is the interesting part. An installation root key is never stored by the
-- platform: the operator supplies a key-encryption key through the environment or an
-- operator-controlled file, and the ring stores only the WRAPPED root key plus the key id that
-- sealed each envelope. Unsealing therefore reads the `key_id` already recorded per version, so
-- a version sealed under a retired key keeps resolving while the re-wrap job is still walking
-- the ring. Losing the operator key makes every locally stored secret unrecoverable — that is
-- why the seal self-check exists and why the rotation wizard says it plainly.

-- The secret itself. `kind` is a free label here (REQ-037's taxonomy); REQ-125's
-- `secret_credentials` table is what pins a row to one of the five validated kinds.
create table secrets (
    id          uuid        primary key default gen_random_uuid(),
    name        text        not null,
    -- `organization` or `global`; organization secrets are scoped like every other tenant row.
    scope_type  text        not null default 'organization',
    organization_id uuid    references organizations (id) on delete cascade,
    site_id     uuid        references sites (id) on delete cascade,
    -- `local` (sealed with the installation root key) or a read-only `file` / `env` bridge
    -- whose value lives outside the platform (docs/requests/REQ-125 §"Provider depth").
    provider    text        not null default 'local',
    -- For a `file` provider the mounted path, for an `env` provider the variable name. A
    -- read-only provider stores a pointer, never a value.
    provider_locator text,
    description text        not null default '',
    -- A `file`/`env` provider can never be written to through the API; the stored rows here
    -- carry no envelope at all, which is what makes that structurally true.
    read_only   boolean     not null default false,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    archived_at timestamptz,
    constraint secrets_provider_check
        check (provider in ('local', 'file', 'env')),
    constraint secrets_scope_type_check
        check (scope_type in ('organization', 'global')),
    -- A read-only bridge has no value to store, so it must name the thing it points at.
    constraint secrets_bridge_locator_check
        check (provider = 'local' or (provider <> 'local' and provider_locator is not null))
);

-- Only non-archived secrets of a name, unique per organization. `global` rows are the platform's
-- own installation-wide secrets and are exempt from the per-organization scope.
create unique index secrets_name_scope_idx
    on secrets (name, coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid))
    where archived_at is null and scope_type = 'organization';

create unique index secrets_name_global_idx
    on secrets (name) where archived_at is null and scope_type = 'global';

-- One envelope per version. The version row — not the secret row — is what names the root key,
-- so a rotation can re-wrap versions one at a time while consumers keep resolving on the old
-- key for every version the job has not reached yet.
create table secret_versions (
    id          uuid        primary key default gen_random_uuid(),
    secret_id   uuid        not null references secrets (id) on delete cascade,
    version     integer     not null,
    -- `v1.<nonce>.<ciphertext>.<tag>` from the shared envelope helper. Never printed.
    envelope    text        not null,
    -- The root key that sealed THIS version. Unsealing reads it rather than the active key.
    key_id      text        not null,
    -- A short prefix an operator can recognise in a list without the value ever being readable
    -- in a database dump; it is a hash, not a truncation of the value.
    value_hint  text        not null default '',
    created_at  timestamptz not null default now(),
    -- Which versions an operator is allowed to fall back to. The current one is the max.
    revoked_at  timestamptz,
    constraint secret_versions_version_check check (version > 0)
);

-- Version numbers are per secret and monotonic; the resolver reads the highest live one.
create unique index secret_versions_secret_version_idx
    on secret_versions (secret_id, version desc);

-- A rotation reads the ring in key-id order, so the walk has a stable cursor even while new
-- versions are being written underneath it.
create index secret_versions_key_idx
    on secret_versions (key_id);

-- The key ring. Exactly one row is `active`; the rest are `retiring` (while a re-wrap job is
-- still walking them) or `retired`, kept for as long as some version names them.
create table secret_root_keys (
    id              uuid        primary key default gen_random_uuid(),
    -- A short, human-quotable id recorded on every sealed version (`secret_versions.key_id`).
    -- Opaque on purpose: it must not tell an attacker anything about the key material.
    key_id          text        not null unique,
    -- The root key material, wrapped by the operator's key-encryption key. Never stored plain.
    wrapped_key     text        not null,
    -- A checksum over the unwrapped key, so the seal self-check can prove the operator key
    -- still opens this root key without holding the key or printing it.
    seal_checksum   text        not null,
    -- `active`, `retiring` or `retired`.
    status          text        not null default 'active',
    -- The fingerprint a reader compares against the operator's own note; it never reveals the key.
    fingerprint     text        not null,
    created_at      timestamptz not null default now(),
    retired_at      timestamptz,
    -- Why the key was retired ("rotated", "revoked") — an audit sentence, not a secret.
    retired_reason  text,
    constraint secret_root_keys_status_check
        check (status in ('active', 'retiring', 'retired'))
);

-- Exactly one active root key, installation-wide. A partial unique index is how a rotation that
-- crashes between "retire" and "activate" is caught at the database level rather than at read.
create unique index secret_root_keys_single_active_idx
    on secret_root_keys ((status)) where status = 'active';

-- The re-wrap job: one row per rotation ceremony, resumable from `cursor` and pausable.
create table secret_rewrap_jobs (
    id                uuid        primary key default gen_random_uuid(),
    -- `pending`, `running`, `paused`, `completed` or `failed`.
    status            text        not null default 'pending',
    -- The key being retired and the key that replaces it. Both stay in the ring for the whole
    -- job, which is what lets a consumer resolve an unsealed version throughout the rotation.
    from_key_id       text        not null references secret_root_keys (key_id),
    to_key_id         text        not null references secret_root_keys (key_id),
    -- Versions re-wrapped so far, and how many exist at the time the job started.
    rewrapped_count   integer     not null default 0,
    total_count       integer     not null default 0,
    -- The key-id cursor a worker last finished; a restart resumes here instead of at zero.
    cursor            text        not null default '',
    -- Set when the job pauses (operator action or a failed batch) and read by the resume note.
    pause_reason      text,
    -- The last error, kept so the screen can show why a job stalled without a log tail.
    last_error        text,
    started_at        timestamptz not null default now(),
    completed_at      timestamptz,
    constraint secret_rewrap_jobs_status_check
        check (status in ('pending', 'running', 'paused', 'completed', 'failed'))
);

-- One live re-wrap job at a time: a second rotation while one is walking is refused rather than
-- interleaved, because both writers would re-wrap the same versions under different keys.
create unique index secret_rewrap_jobs_live_idx
    on secret_rewrap_jobs ((status)) where status in ('pending', 'running', 'paused');

-- A typed credential profile. The VALUE never lives here — `secret_versions.envelope` holds it;
-- this row holds only the non-secret fields a validator and a consumer need.
create table secret_credentials (
    secret_id            uuid        primary key references secrets (id) on delete cascade,
    -- `api_key` `oauth_token` `smtp_account` `payment_key` `ssh_key`.
    kind                 text        not null,
    -- Structured, non-secret fields (endpoint, username, port, token expiry, key fingerprint,
    -- fingerprint algorithm). Kept as jsonb so a new kind needs no migration.
    fields               jsonb       not null default '{}'::jsonb,
    -- `unknown`, `valid`, `invalid` or `stale` (valid before, not re-checked since the schedule).
    validation_state     text        not null default 'unknown',
    -- The validator's own sentence, e.g. the SMTP greeting or a payment provider's refusal.
    validation_message   text,
    validation_checked_at timestamptz,
    -- How often the validator runs on a schedule; 0 means "on demand only".
    validation_interval_days integer not null default 0,
    next_validation_at   timestamptz,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    constraint secret_credentials_kind_check
        check (kind in ('api_key', 'oauth_token', 'smtp_account', 'payment_key', 'ssh_key')),
    constraint secret_credentials_validation_state_check
        check (validation_state in ('unknown', 'valid', 'invalid', 'stale'))
);

-- The credential slots consumers resolve through, never through a hard-coded secret id: swapping
-- a credential is a slot update (docs/requests/REQ-125 §"Instance-level assignment").
create table credential_slots (
    id              uuid        primary key default gen_random_uuid(),
    -- Who the assignment is for: `environment`, `site`, `module` or `organization`.
    scope_type      text        not null,
    -- The concrete scope, e.g. the environment name or the site id.
    scope_id        text        not null,
    -- One of the documented slot names (`ai.provider`, `smtp`, `payments.stripe`, `storage.s3`,
    -- `ssh.release`, `identity.ldap`).
    slot            text        not null,
    primary_secret_id   uuid    references secrets (id) on delete set null,
    fallback_secret_id  uuid    references secrets (id) on delete set null,
    -- The consumer last seen resolving this slot, so removing it can name who is affected.
    last_resolved_by text,
    last_resolved_at timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint credential_slots_scope_type_check
        check (scope_type in ('environment', 'site', 'module', 'organization')),
    -- Primary and fallback must differ: a "fallback" that is the primary is a no-op that reads
    -- like a safety net.
    constraint credential_slots_distinct_check
        check (primary_secret_id is null
               or fallback_secret_id is null
               or primary_secret_id <> fallback_secret_id)
);

-- One assignment per (scope, slot): the resolver's lookup is a single indexed read.
create unique index credential_slots_scope_slot_idx
    on credential_slots (scope_type, scope_id, slot);

-- The documented slot names. A table rather than a constant so the panel can offer a picker and
-- the API can refuse a typo with the list of valid names.
create table credential_slot_catalog (
    slot        text        primary key,
    -- One sentence on what the slot is for, shown next to the picker.
    description text        not null,
    -- The consumers that resolve this slot, so a removal can name who is affected.
    consumers   text        not null default '',
    sort_order  integer     not null default 0
);

insert into credential_slot_catalog (slot, description, consumers, sort_order) values
    ('ai.provider',     'Model provider API key the AI Hub talks to',                 'ai-hub, automations', 10),
    ('smtp',            'Outgoing mail server the notification centre sends through', 'notifications, onboarding', 20),
    ('payments.stripe', 'Payment provider key the checkout flow charges with',        'storefront, orders', 30),
    ('storage.s3',      'Object storage bucket credentials for media and exports',    'media, import-export', 40),
    ('ssh.release',     'Deploy key the release pipeline authenticates with',         'deployment centre', 50),
    ('identity.ldap',   'Directory credentials the SSO bridge binds with',            'identity providers', 60);

-- Scoped machine credentials for CI and remote environments (docs/requests/REQ-125
-- §"Deployment keys"). A deployment key can lease inside its scope; it can never reveal, list
-- values or read another environment.
create table deployment_keys (
    id              uuid        primary key default gen_random_uuid(),
    name            text        not null,
    -- Where the pipeline runs: `production`, `staging`, `development` or a custom name.
    environment     text        not null default 'default',
    -- The secret scopes this key may lease inside (comma-separated scope names).
    scopes          text        not null default '',
    -- The key material, hashed: a deployment key is presented in a CI header, so only its
    -- hash is kept, the way a session token is. The value is shown once at creation.
    key_hash        text        not null,
    key_prefix      text        not null,
    key_fingerprint text        not null,
    -- An operator-set expiry is required: a machine credential that never expires is a liability.
    expires_at      timestamptz not null,
    -- Optional allow-list of source addresses; empty means "any address, but still audited".
    allowed_ips     text        not null default '',
    created_at      timestamptz not null default now(),
    last_used_at    timestamptz,
    revoked_at      timestamptz,
    revoke_reason   text
);

create index deployment_keys_live_idx
    on deployment_keys (environment) where revoked_at is null;

-- A short-lived, use-capped lease over one secret. The lease TOKEN is not the secret: it is an
-- opaque handle that the loopback helper redeems for the value, at most `max_uses` times.
create table secret_leases (
    id              uuid        primary key default gen_random_uuid(),
    secret_id       uuid        not null references secrets (id) on delete cascade,
    -- The opaque token, stored as a hash: a database dump must never redeem a lease.
    token_hash      text        not null unique,
    -- Who the lease was issued to — a pipeline name, a machine identity, a workload.
    consumer        text        not null,
    -- Which deployment key redeemed it (null for an operator-issued lease).
    issued_to_key_id uuid       references deployment_keys (id) on delete set null,
    -- The environment whose deploy revokes this lease automatically.
    environment     text        not null default 'default',
    issued_at       timestamptz not null default now(),
    expires_at      timestamptz not null,
    revoked_at      timestamptz,
    revoke_reason   text,
    -- Redemption budget and the count so far; a lease is useless once the budget is spent.
    max_uses        integer     not null default 1,
    uses            integer     not null default 0,
    last_redeemed_at timestamptz,
    constraint secret_leases_max_uses_check check (max_uses > 0)
);

-- The lease list reads the live leases of one environment newest first, and the list screen
-- reads the recent ones; a revoked lease stays readable so the screen can show the reason.
create index secret_leases_live_idx
    on secret_leases (environment, issued_at desc) where revoked_at is null;

-- Every use of a deployment key is logged with the pipeline identity, the source address and
-- the lease id, so a leaked key can be traced to the pipeline that spent it.
create table deployment_key_uses (
    id          uuid        primary key default gen_random_uuid(),
    key_id      uuid        not null references deployment_keys (id) on delete cascade,
    -- `lease`, `denied` or `revoke`.
    action      text        not null,
    -- The lease the use touched, when there was one.
    lease_id    uuid        references secret_leases (id) on delete set null,
    -- The pipeline identity from the key's scope list, as presented.
    identity    text        not null default '',
    address     text,
    result      text        not null default 'ok',
    created_at  timestamptz not null default now(),
    constraint deployment_key_uses_action_check
        check (action in ('lease', 'denied', 'revoke'))
);

create index deployment_key_uses_key_idx
    on deployment_key_uses (key_id, created_at desc);
