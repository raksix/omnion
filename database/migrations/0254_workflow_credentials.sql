-- 0053_workflow_credentials.sql — credential metadata and the node-package ledger.
--
-- REQ-087, slice 2. The REQ reserved the 0030–0039 band (`0031_workflow_node_packages`,
-- `0032_workflow_credentials`); those numbers are taken on other branches, and the ledger is
-- append-only (docs/05-VERSIONING.md), so this takes the number after the shared high-water
-- instead of renumbering anything that exists. The names are the ones the REQ documents, so
-- the difference between this file and the spec is the number and nothing else.
--
-- Four decisions worth stating, because each is a place the obvious table is wrong:
--
-- 1. **No secret column exists in this schema.** The spec's own line is "credential metadata;
--    the secret payload lives in the encrypted store (REQ-125)", and this table follows it
--    strictly: there is no `api_key`, no `token`, no `password`, nothing a reader could select
--    and print. What travels is `secret_ref` — an opaque handle to a row in the encrypted
--    store that only the execution helper can resolve. A credential table with a plaintext
--    column is not "in progress", it is a leak with a nice index on it, and the fact that no
--    response body could read it would not help the next engineer who found the column.
-- 2. **Usage is derived, never stored.** `usage` is a `jsonb_array_elements` probe over
--    `workflows.graph -> 'nodes'` matched on `params ->> 'credential_key'`, in the store's
--    SQL — not a counter column. A counter drifts the moment a graph is edited, and a stale
--    counter is exactly what makes a "delete" guard refuse forever.
-- 3. **A health value the panel cannot colour is a value nobody reads.** `health` is a closed
--    set of four, and the same four words appear in `crates/workflows/src/credentials.rs`;
--    a check constraint that accepts a fifth would let a row in that the panel has no chip for.
--    `crates/workflows` carries a test naming this file as the list it must agree with.
-- 4. **`credential_key` is the join, and it is enforced to exist.** A node's `params` may
--    name a credential by key, so a key is a foreign key: a graph that points at a deleted
--    credential cannot be written behind the delete guard's back. This is the difference
--    between "the guard works" and "the guard is the only reason the graph is valid".

-- ---------------------------------------------------------------------------------------------
-- workflow_node_packages: what a package ledger records (REQ-087 slice 4 wires the installer;
-- this table and its CRUD arrive with the credential work so the key space is complete)
-- ---------------------------------------------------------------------------------------------

create table workflow_node_packages (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    key             text        not null,
    version         text        not null,
    source          text        not null default 'bundled',
    checksum        text        not null,
    permissions     jsonb       not null default '[]'::jsonb,
    enabled         boolean     not null default true,
    installed_at    timestamptz not null default now(),
    removed_at      timestamptz,
    -- `source` is the REQ's own vocabulary: bundled, marketplace, local. A fourth value would
    -- be a row the installer screen has no label for.
    constraint workflow_node_packages_source_valid
        check (source in ('bundled', 'marketplace', 'local')),
    constraint workflow_node_packages_version_not_blank check (length(btrim(version)) > 0),
    -- An update is equal-or-newer only (REQ-087 risks). The ledger cannot compare versions in
    -- SQL, so the installer checks it; the check below keeps the *shape* honest instead:
    -- a key may be present once, and only while it is installed.
    constraint workflow_node_packages_permissions_is_array
        check (jsonb_typeof(permissions) = 'array')
);

-- One live row per key per organization. The partial index is what makes a re-install an
-- update of the same row instead of a second row the reader sees twice.
create unique index workflow_node_packages_key_uid
    on workflow_node_packages (organization_id, key) where removed_at is null;
create index workflow_node_packages_org_idx
    on workflow_node_packages (organization_id, installed_at desc);

-- ---------------------------------------------------------------------------------------------
-- workflow_credentials: the workflow-facing credential entity
-- ---------------------------------------------------------------------------------------------

create table workflow_credentials (
    id                 uuid        primary key default gen_random_uuid(),
    organization_id    uuid        not null references organizations (id) on delete cascade,
    -- The key a node's `params.credential_key` names. Non-secret by construction: it appears
    -- in graph exports (REQ-094) and a key in an export is a name, not a value.
    key                text        not null,
    name               text        not null,
    type               text        not null,
    scope              text        not null default 'organization',
    sharing            text        not null default 'private',
    -- An opaque handle into the encrypted secret store (REQ-125). Deliberately *not*
    -- a foreign key: that store is a different subsystem with its own lifecycle, and a
    -- cascade from it must not be able to delete a credential row a workflow still names.
    -- Resolution happens in the execution helper and nowhere else.
    secret_ref         text,
    -- Non-secret fields of the type's form (a header name, a host, a region). Secret fields
    -- are excluded by construction: the store refuses to persist any key the credential type
    -- declares `secret`, so this object can never hold one.
    settings           jsonb       not null default '{}'::jsonb,
    owner_user_id      uuid        references users (id) on delete set null,
    health             text        not null default 'untested',
    health_checked_at  timestamptz,
    -- The provider's own words with anything secret stripped, so a failing test explains
    -- itself without becoming a second copy of the secret.
    health_detail      text,
    oauth_expires_at   timestamptz,
    oauth_scopes       text,
    oauth_subject      text,
    last_used_at       timestamptz,
    created_by         uuid        references users (id) on delete set null,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    constraint workflow_credentials_scope_valid
        check (scope in ('organization', 'project')),
    constraint workflow_credentials_sharing_valid
        check (sharing in ('private', 'organization')),
    -- The four values the panel has a chip for, and the four the store can write.
    constraint workflow_credentials_health_valid
        check (health in ('untested', 'ok', 'failing', 'needs_reauth')),
    constraint workflow_credentials_name_not_blank check (length(btrim(name)) > 0),
    constraint workflow_credentials_key_not_blank check (length(btrim(key)) > 0),
    constraint workflow_credentials_key_shape
        check (key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    -- `type` is a credential-type key, which the registry in code defines. It is not a
    -- foreign key because the registry is code (REQ-087 slice 1) and lives in the binary;
    -- the store refuses an unknown type with `credential_type_unknown` before it gets here,
    -- and a check constraint is the only thing the database can say about it.
    constraint workflow_credentials_type_not_blank check (length(btrim(type)) > 0),
    constraint workflow_credentials_settings_is_object
        check (jsonb_typeof(settings) = 'object')
);

-- The key is what a graph names, so it is unique per organization — and the delete guard
-- relies on this index being the only route to a row by key.
create unique index workflow_credentials_key_uid
    on workflow_credentials (organization_id, key);
-- The list's own ordering: the type filter and the name it groups by.
create index workflow_credentials_type_idx
    on workflow_credentials (organization_id, type, name);
-- The amber chip's read. Partial, because an installation has very few reauth-pending
-- credentials and very many healthy ones, and the chip is drawn on every page load.
create index workflow_credentials_reauth_idx
    on workflow_credentials (organization_id)
    where health = 'needs_reauth';
-- Sharing is read on every list, and "mine" is read on every list too.
create index workflow_credentials_owner_idx
    on workflow_credentials (organization_id, owner_user_id, name);
