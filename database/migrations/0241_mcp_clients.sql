-- REQ-108 slice 1 — MCP clients, their tool grants and the invocation log.
--
-- WHAT THIS IS
-- The platform as a tool server for *other* agents. An MCP client is a machine user with a
-- token: it calls JSON-RPC at `/api/v1/mcp` and every call resolves to the same permission
-- guard a panel action does. These three tables are the whole slice: the client, what it may
-- reach, and what it actually did.
--
-- WHY `token_hash` AND NOT THE TOKEN
-- The request calls this "the highest-value target in the platform for lateral movement". A
-- stolen row must not be a stolen credential, so the token is hashed exactly the way the
-- developer keys and the media shares hash theirs and the cleartext is returned once, at
-- creation, and never again. `token_prefix` is kept in the clear for *recognition* only —
-- the last four characters, the way a bank shows them — and the unique index on `token_hash`
-- is what makes authentication a single-row lookup instead of a table scan.
--
-- WHY `arguments_preview` IS A COLUMN AND NOT A FREE-FORM BLOB
-- Every invocation row is a copy of somebody else's arguments, written by a caller we do not
-- control, on a table that gets exported. REQ-105's guard masks it *before* it is written,
-- not on read, because a log that can leak once should never have held the value.
create table if not exists mcp_clients (
    id uuid primary key,
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    description text not null default '',
    -- The last four characters of the token, in the clear, for recognition in the panel.
    token_prefix text not null,
    token_hash text not null,
    scopes jsonb not null default '[]'::jsonb,
    -- Friction on purpose: a client proves its calls before it may write, and the switch to
    -- live is a conscious, audited action (the request's own words).
    sandbox bool not null default true,
    rate_limit_per_min int not null default 60,
    enabled bool not null default true,
    last_used_at timestamptz,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    revoked_at timestamptz,
    constraint mcp_clients_name_len_ck check (char_length(name) between 1 and 60),
    constraint mcp_clients_desc_len_ck check (char_length(description) <= 200),
    constraint mcp_clients_prefix_len_ck check (char_length(token_prefix) = 8),
    constraint mcp_clients_rate_ck check (rate_limit_per_min between 1 and 600),
    -- Revocation and deletion are different acts: revoking keeps the audit trail, so a
    -- revoked row must not be silently re-enabled by an edit that leaves `revoked_at` alone.
    constraint mcp_clients_revoked_ck check (not (revoked_at is not null and enabled))
);

create unique index if not exists mcp_clients_org_name_ux
    on mcp_clients (organization_id, name);
create unique index if not exists mcp_clients_token_hash_ux
    on mcp_clients (token_hash);
create index if not exists mcp_clients_org_enabled_ix
    on mcp_clients (organization_id, enabled);
create index if not exists mcp_clients_token_prefix_ix
    on mcp_clients (token_prefix);

-- What a client may reach. `permission` is denormalised from the tool registry on purpose:
-- the panel's grant picker must be able to show "this tool needs this power" without loading
-- the registry, and a drift between the two is a bug the registry test can catch.
create table if not exists mcp_client_tools (
    client_id uuid not null references mcp_clients (id) on delete cascade,
    tool text not null,
    permission text,
    approval_required bool not null default false,
    enabled bool not null default true,
    added_at timestamptz not null default now(),
    primary key (client_id, tool)
);

create index if not exists mcp_client_tools_tool_ix
    on mcp_client_tools (tool);

-- Every call, successful or not. This is the table that has to answer "what did that token do
-- on Tuesday" after the token is gone, so rows outlive revocation.
create table if not exists mcp_invocations (
    id bigserial primary key,
    organization_id uuid not null references organizations (id) on delete cascade,
    client_id uuid references mcp_clients (id) on delete cascade,
    jsonrpc_id text,
    tool text not null,
    permission text,
    -- The SHA-256 of the *serialized* arguments, so two identical calls group together
    -- without the log holding a second copy of anything sensitive.
    arguments_sha256 text not null default '',
    -- Masked by REQ-105's guard before it is written, never on read.
    arguments_preview jsonb not null default '{}'::jsonb,
    status text not null default 'ok',
    error_code text,
    duration_ms int not null default 0,
    approval_id uuid,
    run_id uuid,
    created_at timestamptz not null default now(),
    constraint mcp_invocations_status_ck
        check (status in ('ok', 'error', 'denied', 'sandbox', 'blocked_airgap'))
);

create index if not exists mcp_invocations_org_created_ix
    on mcp_invocations (organization_id, created_at desc);
create index if not exists mcp_invocations_client_created_ix
    on mcp_invocations (client_id, created_at desc);
create index if not exists mcp_invocations_tool_status_created_ix
    on mcp_invocations (tool, status, created_at desc);
-- For the retention sweep. Every other index leads with organization_id.
create index if not exists mcp_invocations_created_ix
    on mcp_invocations (created_at desc);
