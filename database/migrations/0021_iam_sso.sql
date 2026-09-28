-- Omnion · 0021 · Enterprise sign-in state (REQ-006, slice 4b-2; docs/07-IAM.md §11).
--
-- `0011_iam_advanced.sql` already carries the `auth_providers` table (OIDC, OAuth2, SAML) and
-- `0002_iam.sql` the sessions the flow ends in, so this migration adds only what a *running*
-- sign-in needs:
--
-- * `sso_challenges` — the one thing a provider round trip must bind: a state/nonce this server
--   issued, for one provider, for one organization, single use and short lived. A callback that
--   presents no live challenge is refused before any signature is looked at, so a callback cannot
--   be replayed, cannot be steered at another provider, and cannot be used to smuggle a session.
--   The PKCE verifier for a `code` flow rides in the same row, and the opaque value we hand the
--   browser is the row's `state` — hashed here, never stored in clear.
-- * `auth_provider_events` — the sign-in log of the providers themselves. `sign_in_attempts`
--   (0011) records *password* attempts by email; an SSO attempt has no password and arrives with
--   a provider and an external subject, so it needs its own narrow log: provider, outcome, the
--   external subject id, and which roles the mapping attached. Payloads, tokens and secrets never
--   reach it — the REQ's "never a secret" rule applies to the audit trail too.
--
-- Additive by design (docs/05-VERSIONING.md). Both tables are dropped with their organization,
-- and no existing column changes type. (The number is 0021 because the sibling waves own 0019
-- and 0020 — migration numbers are claimed per wave, not derived from a global count.)

create table sso_challenges (
    id                uuid        primary key default gen_random_uuid(),
    provider_id       uuid        not null references auth_providers (id) on delete cascade,
    organization_id   uuid        not null references organizations (id) on delete cascade,
    -- `oidc`/`oauth2` (an authorization-code round trip) or `saml` (a posted assertion).
    flow              text        not null,
    -- SHA-256 of the `state` we handed the browser. The value itself lives in the client only.
    state_hash        text        not null,
    -- PKCE verifier for the `code` flows: the secret half stays here, never in the provider.
    code_verifier     text,
    -- The panel path the callback returns to. The handler resolves it against a fixed list, so
    -- the column only ever stores a path this server produced.
    return_to         text        not null default '/',
    -- How many callbacks presented this challenge; the second use is refused outright.
    attempts          integer     not null default 0,
    expires_at        timestamptz not null,
    consumed_at       timestamptz,
    created_at        timestamptz not null default now(),
    constraint sso_challenges_flow_check check (flow in ('oidc', 'oauth2', 'saml'))
);

-- A challenge is looked up by the hash the callback presents, newest first.
create index sso_challenges_state_idx on sso_challenges (state_hash, created_at desc);

create table auth_provider_events (
    id                bigserial   primary key,
    provider_id       uuid        not null references auth_providers (id) on delete cascade,
    organization_id   uuid        not null references organizations (id) on delete cascade,
    user_id           uuid        references users (id) on delete set null,
    -- The subject id the provider asserts (`sub`), so support can correlate with the directory.
    external_subject  text,
    -- `success`, `provisioned`, `updated`, `refused` or `error`.
    outcome           text        not null,
    -- Machine-readable reason (`challenge_expired`, `signature_invalid`, `account_disabled` …).
    reason            text,
    -- Roles the claim → role mapping attached, as a name list. Never a claim payload.
    roles_applied     text[]      not null default '{}',
    ip_address        inet,
    user_agent        text,
    created_at        timestamptz not null default now(),
    constraint auth_provider_events_outcome_check
        check (outcome in ('success', 'provisioned', 'updated', 'refused', 'error'))
);

create index auth_provider_events_provider_idx
    on auth_provider_events (provider_id, created_at desc);

-- The panel's provider list reads the last sign-in per provider, and the per-organization event
-- list reads the same rows newest first.
create index auth_provider_events_org_idx
    on auth_provider_events (organization_id, created_at desc);
