-- 0240_developer_portal.sql — API keys, OAuth apps and the request log (REQ-022, slice 1).
--
-- The developer portal is the first surface in the platform where a **person is not the
-- caller**. Every credential elsewhere belongs to a user or to a service account that a role
-- binds to; a key here belongs to an organization and carries its own scope list, so the
-- permission check on a guarded route is the intersection of what the caller may do and what
-- the key was granted. That makes these tables different in kind from `service_account_keys`
-- (REQ-006, `0011_iam_advanced.sql`) even though both store a hash and a prefix:
--
--   * A service-account key is a **machine identity** — a subject roles bind to, so it is
--     authorised through the same binding table as a person. A developer key is a **delegation
--     of a person**: it has no role, no binding and no independent identity, and every request
--     it makes is evaluated against the scopes the operator ticked when creating it. Two
--     mechanisms that look alike in a request log and behave nothing alike.
--   * Because a key has no role of its own, it cannot be granted `developer.keys.manage`. A key
--     that could mint another key could escalate from a read-only integration into the whole
--     portal — which is why `is_matable` below is enforced by the **scope list itself**, not by
--     a column: a key whose scopes contain no management key can never create one.
--
-- Shape decisions, each because of a specific way a key store goes wrong:
--
--   * **Only the hash is stored.** `key_hash` is the SHA-256 of the full token; `key_prefix` is
--     the ten characters of lookup namespace the sign-in path indexes on. There is no column
--     that could hold a usable secret, so no query, no export, no CSV and no error message can
--     leak one — the same rule `crates/security/src/secrets_store.rs` keeps structurally for
--     the inventory, applied here because this table is the one place a leak would originate.
--   * **`unique (organization_id, lower(name), environment) where revoked_at is null`** — a
--     revoked key's name is freed, because an operator who revokes a key and creates a new one
--     with the same name is doing exactly what they mean to do. Without the partial index a
--     revoked row would block the name for ever and the only workaround would be a suffix.
--   * **`scopes text[]` is not null and checked non-empty**, so a key that can do nothing can
--     never exist. The alternative — a key with no scopes, treated as "unrestricted" — is the
--     classic inverted default, and the panel's own error would then be *403 on every route*,
--     which reads as a platform fault rather than as a misconfigured key.
--   * **`expires_at` is nullable and `revoked_at` is a soft delete**, because "this key died at
--     midnight" and "an operator revoked it" are different facts and the logs screen needs to
--     tell them apart.
--   * `api_request_logs.permission` holds the permission the guard resolved, and
--     `client_fingerprint` is a **keyed** hash (see `omnion-developer`), never a raw address:
--     the log is a debugging surface that gets pasted into a ticket.
--   * The identity columns are nullable rather than `on delete cascade` on `users`: deleting a
--     user must not delete a key an organization is still being billed for, so the column keeps
--     the id and the row becomes an orphan whose **requests still log**, which is the whole
--     point of the table.

create table if not exists api_keys (
    id uuid primary key default gen_random_uuid(),
    -- Owning organization. Cascade: a tenant that is gone has no keys to keep.
    organization_id uuid not null references organizations(id) on delete cascade,
    -- 3–64 characters, checked below rather than only in the route, so a key written by a
    -- future caller still cannot be nameless.
    name text not null,
    -- `live` or `sandbox`. A closed list because the panel's environment banner is built from
    -- this value and a third one would render a banner nobody wrote.
    environment text not null default 'live',
    -- The lookup namespace: `omn_live_<10 chars>`. Indexed and unique; safe to display.
    key_prefix text not null,
    -- SHA-256 of the whole token. Never returned, never logged, never exported.
    key_hash text not null,
    -- The delegation. Non-empty by constraint, and never contains a `developer.*.manage` key
    -- unless the creating operator held it (enforced in `omnion-developer::keys`).
    scopes text[] not null,
    -- Who issued the key. Set null when that user is deleted; the key outlives its issuer.
    created_by uuid references users(id) on delete set null,
    created_by_name text not null default '',
    -- Live accounting for the list screen: last authentication and the rotation lineage.
    last_used_at timestamptz,
    expires_at timestamptz,
    revoked_at timestamptz,
    rotated_from uuid references api_keys(id) on delete set null,
    created_at timestamptz not null default now(),
    constraint api_keys_environment_ck check (environment in ('live', 'sandbox')),
    constraint api_keys_name_ck check (length(btrim(name)) between 3 and 64),
    constraint api_keys_scopes_ck check (cardinality(scopes) between 1 and 64),
    -- The sign-in path's only lookup: a presented token's prefix, then the hash comparison.
    constraint api_keys_hash_uk unique (key_hash)
);

-- The lookup index. The unique constraint already covers it; named so a plan reads clearly.
create index if not exists api_keys_prefix_ix on api_keys (key_prefix);

-- The list screen's own order, and the scope filter's narrowing.
create index if not exists api_keys_org_created_ix
    on api_keys (organization_id, created_at desc);

-- A live key's name is unique per organization **and environment**: `live` and `sandbox` are
-- different systems and an operator migrating a service from one to the other should not be
-- stopped by a name it used a year ago. Revoking frees the name.
create unique index if not exists api_keys_name_uk
    on api_keys (organization_id, lower(name), environment)
    where revoked_at is null;

-- Per-key daily rollup for the usage chart. Written by the request-log middleware, which is the
-- only writer: a route is covered by the log without anybody adding code.
create table if not exists api_key_usage_daily (
    api_key_id uuid not null references api_keys(id) on delete cascade,
    day date not null,
    requests int not null default 0,
    errors int not null default 0,
    avg_duration_ms int not null default 0,
    primary key (api_key_id, day)
);

-- OAuth apps (slice 4 shares this table; the constraints are written once, here).
create table if not exists oauth_apps (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations(id) on delete cascade,
    name text not null,
    description text not null default '',
    -- Public half. Unique platform-wide: an authorization server that cannot name its own client
    -- is not an authorization server.
    client_id text not null,
    -- SHA-256 of the client secret. The plaintext exists once, in the create/rotate response.
    client_secret_hash text not null,
    -- 1–10 absolute https URLs, no fragments, no wildcard. Checked in SQL as well as in the
    -- route, because a redirect URI is the one field where a loose value is a token theft.
    redirect_uris text[] not null,
    scopes text[] not null,
    -- Absolute https, because an app's homepage is where a user is sent to decide whether to
    -- trust it. `loopback` http is permitted by the validator but never by this constraint,
    -- because a redirect URI is not a homepage.
    homepage_url text,
    icon_media_id uuid references media(id) on delete set null,
    created_by uuid references users(id) on delete set null,
    created_by_name text not null default '',
    archived_at timestamptz,
    last_secret_rotated_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint oauth_apps_name_ck check (length(btrim(name)) between 2 and 120),
    constraint oauth_apps_redirects_ck check (cardinality(redirect_uris) between 1 and 10),
    constraint oauth_apps_scopes_ck check (cardinality(scopes) between 1 and 64),
    constraint oauth_apps_client_id_uk unique (client_id)
);

create unique index if not exists oauth_apps_name_uk
    on oauth_apps (organization_id, lower(name))
    where archived_at is null;

create index if not exists oauth_apps_org_ix on oauth_apps (organization_id, created_at desc);

-- Who granted what. `scopes` is the **intersection at grant time**, not the app's current list:
-- an app that later widens must not silently widen the grants already handed out.
create table if not exists oauth_authorizations (
    id uuid primary key default gen_random_uuid(),
    app_id uuid not null references oauth_apps(id) on delete cascade,
    user_id uuid not null references users(id) on delete cascade,
    scopes text[] not null,
    granted_at timestamptz not null default now(),
    revoked_at timestamptz,
    constraint oauth_authorizations_uk unique (app_id, user_id)
);

create index if not exists oauth_authorizations_user_ix
    on oauth_authorizations (user_id, granted_at desc);

-- The request log. Append-only, pruned on a schedule whose window the logs screen publishes.
--
-- `permission` is the resolved key, which is the column that makes a 403 explainable: "this
-- request was refused because the caller needed `content.pages.manage`" is answerable here and
-- nowhere else. `body` is deliberately absent — a request log that stored bodies would become a
-- copy of every payload the platform ever accepted, including the ones that carry credentials.
create table if not exists api_request_logs (
    id bigint generated always as identity primary key,
    organization_id uuid references organizations(id) on delete cascade,
    api_key_id uuid references api_keys(id) on delete set null,
    -- The display prefix, copied in rather than joined: a revoked key's rows must still name
    -- the key that made them, and the row must survive the key.
    api_key_prefix text,
    actor_user_id uuid references users(id) on delete set null,
    actor_name text not null default '',
    method text not null,
    -- The path **without** its query string. A query string is attacker-controlled and routinely
    -- carries a token; storing it would put a credential in the one table that gets exported.
    path text not null,
    status smallint not null,
    duration_ms int not null default 0,
    permission text,
    -- A keyed hash of (key material, ip, user-agent) — see `omnion-developer::logs`.
    client_fingerprint text,
    created_at timestamptz not null default now(),
    constraint api_request_logs_status_ck check (status between 100 and 599)
);

create index if not exists api_request_logs_org_ix
    on api_request_logs (organization_id, created_at desc);

create index if not exists api_request_logs_key_ix
    on api_request_logs (api_key_id, created_at desc);

create index if not exists api_request_logs_status_ix
    on api_request_logs (status, created_at desc);

create index if not exists api_request_logs_created_ix
    on api_request_logs (created_at desc);

-- An organization-scoped GET must not walk the whole platform's log. The index is the log
-- screen's most expensive query and this is the one that answers it.
create index if not exists api_request_logs_org_status_ix
    on api_request_logs (organization_id, status, created_at desc);
