-- REQ-033 · internal developer platform (slice 3) — OAuth applications and the
-- authorization codes the platform issues for them.
--
-- Two tables. The safety property here is the one the request states about secret handling
-- and about overlap windows, and it is expressed in the *columns* rather than in code:
--
--   * `client_secret_hash` is a one-way hash, so an app's secret cannot be read back out of
--     the database by this API or by anyone holding a dump. The plaintext exists exactly
--     once, in the creation or rotation response.
--   * `previous_secret_hash` + `previous_secret_expires_at` are the overlap window. Both are
--     nullable *together* and constrained together below, because half an overlap is the
--     dangerous case: a previous hash with no expiry is a secret that is valid forever, and
--     an expiry with no previous hash is a deadline for a secret that does not exist. The
--     check constraint refuses both rather than letting a partial write decide.
--
-- `redirect_uris` is `jsonb` of absolute URIs, checked here for the one rule a database can
-- enforce cheaply — it must be a list of strings — and for shape: the application layer owns
-- the `https` / `http://localhost` rule because the exception is a *semantic* one and a regex
-- in SQL would be a second implementation of it that nobody can read.
--
-- `oauth_authorization_codes` is a short-lived ledger, not a session store. The code is
-- hashed on the way in (`code_hash` is a primary key of a hash, so a database dump does not
-- hand out usable codes), it is single-use (`used_at`), and it expires in minutes. The
-- sweeper that removes spent rows is the REQ-016 retention worker's job; the index on
-- `(app_id, expires_at)` is what makes that sweep a range scan rather than a table scan.

-- ─────────────────────────────────────────────────────────────────── oauth_apps

create table if not exists oauth_apps (
    id                  uuid primary key default gen_random_uuid(),
    organization_id     uuid not null references organizations (id) on delete cascade,
    name                text not null,
    description         text,
    -- Object key in the bucket, never a URL: the logo is served through the media surface
    -- like every other asset, so its access rules are the ones an organization already has.
    logo_object_key     text,
    -- Public identifier, sent by a client in the authorization request. Unique on its own so
    -- the authorization endpoint resolves an app with one index probe.
    client_id           text not null unique,
    -- One-way. See `omnion_developer::secret`.
    client_secret_hash  text not null,
    -- The overlap window. Both columns or neither: the constraint below is the enforcement,
    -- and the application layer sets them in the same statement for the same reason.
    previous_secret_hash text,
    previous_secret_expires_at timestamptz,
    -- One absolute URI per entry. The `https`-or-localhost rule is applied in the crate.
    redirect_uris      jsonb not null default '[]'::jsonb,
    scopes              jsonb not null default '[]'::jsonb,
    grant_types         jsonb not null default '["authorization_code"]'::jsonb,
    status              text not null default 'active',
    -- *When* the app was withdrawn, as distinct from the fact that it was. `status` answers the
    -- only question the platform acts on; this answers the one an operator asks ("when did
    -- somebody retire this, and was it me?"), and it is what the audit trail and the app's own
    -- detail screen read. Null for a live or merely suspended app.
    --
    -- Constrained to travel with `status` below, because half a withdrawal is the dangerous
    -- direction: a `deleted` app with no timestamp is a row the panel cannot date, and a
    -- timestamp on an app whose status still reads `active` is an app that keeps issuing codes
    -- after somebody believed they had stopped it.
    deleted_at          timestamptz,
    created_by          uuid not null references users (id) on delete cascade,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint oauth_apps_name_known
        check (length(btrim(name)) between 3 and 60),
    -- A list of strings, and a list with something in it. A registered app that can be
    -- redirected nowhere is a client that will fail its first authorization request, which is
    -- exactly the mistake a schema check can prevent and a UI cannot.
    constraint oauth_apps_redirect_uris_known
        check (jsonb_typeof(redirect_uris) = 'array' and jsonb_array_length(redirect_uris) > 0),
    constraint oauth_apps_scopes_known
        check (jsonb_typeof(scopes) = 'array' and jsonb_array_length(scopes) > 0),
    constraint oauth_apps_grant_types_known
        check (jsonb_typeof(grant_types) = 'array' and jsonb_array_length(grant_types) > 0),
    constraint oauth_apps_grant_types_are_known
        check (grant_types <@ '["authorization_code", "client_credentials"]'::jsonb),
    constraint oauth_apps_status_known
        check (status in ('active', 'suspended', 'deleted')),
    -- The withdrawal's two halves, together — the same reasoning as the overlap window below.
    constraint oauth_apps_deletion_is_whole
        check ((status = 'deleted') = (deleted_at is not null)),
    -- Half an overlap window is the failure this rules out. A previous hash with no expiry is
    -- a credential that stays valid after the operator believed it was revoked; an expiry with
    -- no hash is a countdown to nothing, which reads as a broken rotation.
    constraint oauth_apps_overlap_is_whole
        check ((previous_secret_hash is null) = (previous_secret_expires_at is null))
);

create index if not exists oauth_apps_org_status_idx
    on oauth_apps (organization_id, status);

-- The organization owns its app names: two apps with one name make the panel's list
-- ambiguous and a support conversation about "the reporting app" unanswerable.
create unique index if not exists oauth_apps_org_name_key
    on oauth_apps (organization_id, name)
    where status <> 'deleted';

-- ──────────────────────────────────────────────── oauth_authorization_codes

create table if not exists oauth_authorization_codes (
    -- The SHA-256 of the code, never the code. A dump of this table hands out no usable
    -- credential, and the row is looked up by the same hash the client presents.
    code_hash       text primary key,
    app_id          uuid not null references oauth_apps (id) on delete cascade,
    user_id         uuid not null references users (id) on delete cascade,
    redirect_uri    text not null,
    scopes          jsonb not null default '[]'::jsonb,
    -- The PKCE challenge, when the client sent one. Nullable because `client_credentials`
    -- has no user and no challenge; the flow that requires it refuses the ones without.
    code_challenge  text,
    code_challenge_method text,
    expires_at      timestamptz not null,
    used_at         timestamptz,
    created_at      timestamptz not null default now(),
    constraint oauth_codes_challenge_method_known
        check (code_challenge_method is null or code_challenge_method in ('S256', 'plain')),
    -- A challenge and its method travel together for the same reason the overlap window does.
    constraint oauth_codes_challenge_is_whole
        check ((code_challenge is null) = (code_challenge_method is null)),
    -- A code is a code for minutes. A row whose expiry is already past when it is written is
    -- a bug in the caller, and storing it would make the sweeper the only thing standing
    -- between a bug and an unexpired credential.
    constraint oauth_codes_expiry_is_future
        check (expires_at > created_at)
);

-- The sweeper's range scan, and the authorization endpoint's "is this app already holding
-- live codes" question, both want exactly this index.
create index if not exists oauth_codes_app_expiry_idx
    on oauth_authorization_codes (app_id, expires_at);

-- An app whose codes are all spent is the common case, and the one that must never grow
-- without bound: `used_at is null` is the half of the index the sweeper walks.
create index if not exists oauth_codes_unused_idx
    on oauth_authorization_codes (expires_at)
    where used_at is null;
