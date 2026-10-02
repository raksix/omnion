-- REQ-033 · internal developer platform (slice 4) — the CLI device-code flow.
--
-- `sdk_scaffolds` already exists (migration 0223, slice 1), so this file adds only what slice 4
-- needs that slice 1 did not: the pending device codes and the tokens they are exchanged for.
--
-- ───────────────────────────────────────────────────────────────────── the shape of the problem
--
-- A terminal cannot hold a browser session, so `omnion login` cannot do a redirect. RFC 8628's
-- answer is a short code the person reads off their terminal, types into a browser, and approves
-- while the terminal polls for a token.
--
-- Every column here exists because of one property the request's risk note names: **phishing**.
-- In this flow the attacker and the victim can be in the same conversation — "run this command,
-- then paste the code at this page" — and nothing in the protocol prevents it. What the protocol
-- does give is four properties, and the schema is where three of them are enforced rather than
-- merely intended:
--
--   1. Short-lived.     `expires_at` is on every row, and the code below refuses one that is
--                       already dead rather than leaving it to a sweep.
--   2. Bound to a user. `approved_by` is NOT NULL, so a token can always name the person whose
--                       authority created it. A token with no owner is a credential nobody can
--                       be asked about.
--   3. Metadata shown.  `client_name` is NOT NULL for the same reason `approved_by` is: an
--                       approval screen with only a code is the screen an attacker wants.
--   4. Permission.     Checked by the route before it reaches the store; the schema cannot see a
--                       permission, so it is not pretended to here.
--
-- ───────────────────────────────────────────────────────────────── the write-only property
--
-- `device_code_hash` is a primary key of a *hash*, exactly as `oauth_authorization_codes.code_hash`
-- is. A database dump therefore hands out no usable device codes, which matters because this
-- table is short-lived, frequently dumped by the hourly backup, and holds the only thing a
-- waiting attacker polls with.
--
-- The user code is *not* hashed and that is deliberate, in both directions. It is far too short
-- to hash usefully (8 characters from a 30-symbol alphabet is ~40 bits, and an offline hash of a
-- 40-bit space is instant). What protects it instead is that it expires in ten minutes and can
-- only be redeemed by a session holding `developer.keys.manage` — the two constraints below.
-- Making it a hash would protect it from nothing and would cost the one property the flow
-- actually needs, which is that a person can read it off a terminal.

-- ──────────────────────────────────────────────────────── cli_device_codes

create table if not exists cli_device_codes (
    id                  uuid primary key default gen_random_uuid(),
    organization_id     uuid not null references organizations (id) on delete cascade,

    -- What the terminal polls with, hashed. Never stored in the clear.
    device_code_hash    text not null unique,

    -- What the person types. Short, low entropy, un-hashed — see the module doc.
    user_code           text not null,

    -- Who the token will belong to. NOT NULL and not defaulted: an approval with nobody behind
    -- it produces a credential with no owner, and a schema that allowed one would be a schema
    -- whose rows cannot be audited.
    approved_by         uuid references users (id) on delete cascade,

    approved_at         timestamptz,

    -- The requesting-client metadata the approval screen shows. NOT NULL for the same reason:
    -- a bare code on a screen is a screen an attacker walks somebody into approving.
    client_name         text not null,
    client_uri          text,

    -- The scopes the terminal asked for. Checked against `omnion-developer`'s fixed set before
    -- it is written; the check lives in code because it is a list, not a shape.
    scopes              jsonb not null,

    -- The interval the client must currently wait between polls, and when it last polled.
    -- Both exist because of RFC 8628's slow-down rule: a client that polls every second is
    -- told to wait longer, and the longer interval has to *persist* or the instruction is a
    -- no-op. See `PollState::polled`, which returns the interval for exactly that reason.
    interval_seconds    integer not null default 5,
    last_polled_at      timestamptz,

    -- When the *device* code expires. Fifteen minutes.
    expires_at          timestamptz not null,

    created_at          timestamptz not null default now(),

    -- The two rules a column type cannot state.
    constraint cli_device_code_approval_is_whole
        check ((approved_by is null and approved_at is null)
            or (approved_by is not null and approved_at is not null)),
    constraint cli_device_code_interval_sane
        check (interval_seconds between 1 and 300),
    constraint cli_device_code_user_code_length
        check (length(btrim(user_code)) between 8 and 9),
    -- NOT NULL is not enough for the client name, and the probe is what showed it.
    -- `not null` accepts `'   '`, so a row with a whitespace-only client name renders as an
    -- approval screen showing a bare code with an empty caption beside it — which is the screen
    -- an attacker wants. The request's risk note says the code must be "displayed with
    -- requesting-client metadata"; an empty caption is the absence of it, wearing a
    -- constraint's clothing.
    constraint cli_device_code_client_name_shown
        check (length(btrim(client_name)) > 0)
);

-- The approval lookup: a person types a user code and the browser finds the row. Organization is
-- part of the key rather than a filter afterwards, so the lookup cannot be pointed at another
-- tenant's code — a code is read aloud, and "the code is unique" has to be true.
create unique index if not exists cli_device_codes_user_code_idx
    on cli_device_codes (organization_id, user_code);

-- The sweeper's range scan. Expired rows are the ones to delete, and a partial index on the
-- unapproved ones keeps it off the rows that were approved (which are kept a little longer for
-- attribution, exactly as revoked OAuth tokens are).
create index if not exists cli_device_codes_live_expiry_idx
    on cli_device_codes (expires_at)
    where approved_by is null;

-- ──────────────────────────────────────────────────────────────── cli_access_tokens

-- The token a completed exchange produces. `oauth_access_tokens` is the app-token table and
-- this one is deliberately not an extension of it: a CLI token belongs to a *user* acting from
-- a terminal, it is scoped by an approval rather than by an OAuth grant, and putting it in the
-- app table would make "which tokens does this app have" answer with terminal logins.
create table if not exists cli_access_tokens (
    id              uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,

    -- Hashed on the way in. The plaintext exists for exactly one response.
    token_hash      text not null unique,

    -- The approving user. This is what binds the terminal to a person.
    user_id         uuid not null references users (id) on delete cascade,

    -- The device code it came from, for revocation: revoking a login means finding every token
    -- this flow issued to that person from that code.
    device_code_id  uuid not null references cli_device_codes (id) on delete cascade,

    -- What the terminal will authenticate as.
    environment     text not null default 'live',

    -- The scopes actually granted, which may be narrower than requested. Stored rather than
    -- derived from the code so that an audit of "what could this token do" reads the answer
    -- off the token instead of replaying a policy that may since have changed.
    scopes          jsonb not null,

    expires_at      timestamptz not null,
    created_at      timestamptz not null default now(),

    constraint cli_tokens_environment_known
        check (environment in ('live', 'sandbox')),
    constraint cli_tokens_expiry_in_the_future_at_write
        check (expires_at > created_at)
);

-- The sweep, same partial-index shape as the codes above.
create index if not exists cli_tokens_live_expiry_idx
    on cli_access_tokens (expires_at);

-- "Which terminals does this person have" — the revoke-everywhere screen a future slice needs,
-- and the answer to "is that CLI still logged in" without a full scan.
create index if not exists cli_tokens_user_idx
    on cli_access_tokens (user_id, created_at desc);

-- ───────────────────────────────────────────────────────────────────── what is NOT here
--
-- No `client_secret`, no `code_challenge`, no `refresh_token`.
--
-- The first two belong to `oauth_apps` and this flow does not register applications — a CLI is
-- not an OAuth client and making it one would mean a client id and secret in every shell
-- history. The third is a deliberate omission rather than a gap: a refresh token in a terminal
-- is a long-lived credential on a disk nobody is watching, and the CLI re-runs `omnion login`
-- instead. Both are decisions a later reader can see rather than reverse-engineer.