-- REQ-033 · internal developer platform (slice 3c) — the access tokens an OAuth flow issues.
--
-- One table, and the safety property here is the same one `oauth_apps` and
-- `oauth_authorization_codes` already express: **a credential is stored only as a hash, and a
-- row that a later release writes must not be able to break the ones this release reads.**
--
--   * `token_hash` is the primary key and the only form of the token that is ever written. A
--     dump of this table hands out no usable credential, and a lookup is an index probe rather
--     than a scan — the same two-halves argument the API key table makes, without needing two
--     halves because a token has no public prefix to look up by.
--   * `app_id` and `user_id` travel **together**, and the constraint below refuses half a pair.
--     A token from an authorization code names a person; a token from `client_credentials` names
--     the application alone. A row with neither is a machine token, a row with both is a user
--     token, and a row with a user but no app is a credential the platform could not revoke
--     when the app is withdrawn — which is exactly the row a hand-written insert would create.
--
-- **Why `user_id` is nullable rather than required.** Requiring it would have made
-- `client_credentials` unimplementable, and modelling the machine token as a synthetic user row
-- is how a platform ends up with audit entries naming a person who never did the thing — the
-- same reason `KeyPrincipal::actor_user_id` returns `None`.
--
-- `revoked_at` rather than a delete: a withdrawn app's tokens have to keep being attributable
-- for the retention window, and "who used this credential on the 3rd" is a question an incident
-- review asks long after the row would have been interesting to keep.

create table if not exists oauth_access_tokens (
    -- The SHA-256 of the token, never the token. Primary key because a lookup is by the hash and
    -- because two tokens that hash the same *are* the same token, so a duplicate is a
    -- constraint violation rather than a second row authenticating the same bearer value.
    token_hash      text primary key,
    app_id          uuid not null references oauth_apps (id) on delete cascade,
    -- Null exactly when this is a `client_credentials` token. The constraint below is what
    -- keeps that an either/or rather than a "usually".
    user_id         uuid references users (id) on delete cascade,
    -- What this token may do, as a jsonb array of permission keys. Copied from the grant at
    -- issue time rather than read from the app row on every call, and that is a deliberate
    -- denormalisation: narrowing an app's scopes must not silently widen a token already in
    -- circulation, and reading the app's current scopes would do exactly that.
    scopes          jsonb not null default '[]'::jsonb,
    -- `'authorization_code'` or `'client_credentials'`, recorded so a review can tell a
    -- person's grant from an application's without joining the code ledger (which is swept).
    grant_type      text not null,
    issued_at       timestamptz not null default now(),
    expires_at      timestamptz not null,
    revoked_at      timestamptz,
    constraint oauth_tokens_grant_known
        check (grant_type in ('authorization_code', 'client_credentials')),
    -- A scope list is a list of strings, and a token with no scopes can do nothing. Not an
    -- emptiness refusal: an app registered with no scopes cannot be registered at all (the
    -- API layer says so), so an empty array here means a direct insert, and a token that
    -- cannot do anything is harmless in a way a malformed column is not.
    constraint oauth_tokens_scopes_is_array
        check (jsonb_typeof(scopes) = 'array'),
    -- A token that is already expired when it is written is a bug in the caller. The sweeper
    -- would clean it up, but a token row whose expiry is in the past at insert time is the
    -- same class of defect `oauth_codes_expiry_is_future` refuses.
    constraint oauth_tokens_expiry_is_future
        check (expires_at > issued_at),
    -- **The two shapes of token, in the database.** A `client_credentials` token stands for the
    -- application and must name no user; an `authorization_code` token stands for a person who
    -- consented and must name one. Stated here rather than only in the application layer for
    -- the same reason `oauth_apps`' overlap is: half a pair is the dangerous direction. A
    -- machine token carrying a user id is a row that attributes the application's requests to a
    -- person, and an audit trail that does that is worse than no audit trail — it is one
    -- somebody will believe.
    --
    -- A future third grant has to make a decision here rather than fall through: it will be
    -- refused until somebody states which shape it is, which is the intended friction.
    constraint oauth_tokens_provenance_is_whole
        check ((user_id is null) = (grant_type = 'client_credentials'))
);

-- The authentication path: a bearer token arrives, is hashed, and is looked up. Primary key
-- covers it, so no secondary index is needed for that — but the *cleanup* path and the
-- app-detail screen both walk by app, and the app-cascade delete walks it too.
create index if not exists oauth_tokens_app_idx
    on oauth_access_tokens (app_id);

-- The sweeper's range scan. `revoked_at is null and expires_at < now()` is the whole query, and
-- a partial index on the live rows only means the sweep does not walk the tokens that have
-- already been cleaned or deliberately kept for attribution.
create index if not exists oauth_tokens_live_expiry_idx
    on oauth_access_tokens (expires_at)
    where revoked_at is null;

-- A user's own token list, for a future "revoke my sessions" screen and for the token
-- introspection endpoint. `user_id` is null for machine tokens, and this index simply does not
-- hold them — which is correct: they have no user to list them under.
create index if not exists oauth_tokens_user_idx
    on oauth_access_tokens (user_id)
    where user_id is not null;

-- ── the two shapes of token ──────────────────────────────────────────────────────────────
--
-- Not a check constraint that could live in the table definition, because it is stated here
-- as a *test* in `scripts/qa/probe-oauth-tokens-migration.sql` alongside the rest: the property
-- is that a `client_credentials` token has no user and an `authorization_code` token has one,
-- and a future third grant has to make a decision rather than fall through.
--
--   -- a machine token with a user is refused
--   insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, expires_at)
--   values ('h1', <app>, <user>, 'client_credentials', now() + interval '1 hour');
--   -- ERROR: violates oauth_tokens_provenance_is_whole
