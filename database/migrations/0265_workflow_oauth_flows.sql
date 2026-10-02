-- 0054: the OAuth flow's own persistence (REQ-087 slice 3).
--
-- Two things this migration deliberately does NOT do:
--
-- * It has no `access_token`, `refresh_token` or `code_verifier` column. Those live in the
--   encrypted store (REQ-125) and this table holds only the handle, exactly as
--   `workflow_credentials.secret_ref` does. A table that could hold a token is a table a
--   future `select *` will print.
-- * It has no foreign key to `workflow_credentials`. A cascade from this table must not be
--   able to delete a credential, and the credential's own delete path calls into this table,
--   so the reference is by id and is cleaned up explicitly where it must be.
--
-- `state_hash` is a hash of the signed state rather than the state itself. The state is a
-- bearer value: anything that can read this table could otherwise replay a callback. A
-- callback is single-use and short-lived, so hashing costs nothing and removes the class.

create table if not exists workflow_oauth_flows (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    credential_id uuid not null,
    credential_type text not null,
    state_hash text not null,
    code_challenge text,
    code_verifier_enc text,
    authorize_url text not null,
    redirect_uri text not null,
    scopes text,
    status text not null default 'pending',
    subject text,
    failure_code text,
    failure_detail text,
    started_at timestamptz not null default now(),
    expires_at timestamptz not null,
    completed_at timestamptz,
    constraint workflow_oauth_flows_status_valid
        check (status in ('pending', 'completed', 'failed', 'expired')),
    constraint workflow_oauth_flows_expiry_after_start
        check (expires_at > started_at));

-- The lookup the callback performs, and it is the only one: hash in, row out.
create unique index workflow_oauth_flows_state_hash_uid
    on workflow_oauth_flows (organization_id, state_hash);
create index workflow_oauth_flows_pending_idx
    on workflow_oauth_flows (organization_id, credential_id)
    where status = 'pending';
