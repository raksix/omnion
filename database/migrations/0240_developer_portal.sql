-- 0240 — the developer portal, as `main` wrote it (REQ-022 slice 1), reconciled with the schema
-- this branch already built (REQ-033).
--
-- ## Why this file is not what `main` merged
--
-- `main` created `crates/developer` and migration `0240` independently of this branch, which had
-- already created the same crate and the same tables in `0223`, `0231` and `0232`. The tables
-- disagree on names for the *same* facts, and the disagreement is not cosmetic:
--
-- | main `0240` | this branch `0223` |
-- |-------------|--------------------|
-- | `key_prefix text` | `prefix text` |
-- | `key_hash text` | `secret_hash text` (scheme-prefixed) |
-- | `created_by_name text` | `created_by uuid` joined to `users` |
-- | `rotated_from uuid` | `rotated_at timestamptz` |
-- | `api_key_usage_daily.avg_duration_ms` | `.p95_ms` |
-- | `api_request_logs.client_fingerprint`, `.permission`, `.actor_name` | not present |
--
-- This branch's naming is kept, for the reason the reader is: `prefix`/`secret_hash` are what
-- `omnion-developer`'s store and the key sign-in path already query, and `p95_ms` is what the
-- detail screen's usage chart plots. Renaming a column to satisfy a second writer would mean
-- editing a store, a sign-in query, a route body and a panel view to make a file nobody reads
-- agree with itself.
--
-- ## What that makes this migration do
--
-- `main`'s file was, in effect, "create these three tables if they are missing, then index
-- them". On this branch the tables exist and the names differ, so a literal merge of that file
-- was **not** merely redundant — it was broken, and `if not exists` is what hid it. Applied over
-- this branch's `0223`, `main`'s original file got as far as line 80 and then failed:
--
--     NOTICE:  relation "api_keys" already exists, skipping
--     ERROR:  column "key_prefix" does not exist
--
-- The `create table if not exists` succeeded by doing nothing, the failure surfaced only at the
-- first index that names a column, and everything before it in the file had already been
-- committed. That is the reason this file does not create `api_keys`, `api_key_usage_daily` or
-- `api_request_logs` at all: on this branch `0223` owns them, and a migration that re-declares a
-- table another migration owns is a migration whose success depends on which branch ran first.
--
-- So this file keeps only what is genuinely *not* in this branch's chain, and adds the indexes
-- that `main`'s file contributed and `0223` does not have. Every statement is `if not exists`,
-- so it is idempotent, and so it is a no-op if this branch later takes a route that creates the
-- same objects.

-- ---------------------------------------------------------------------------------------------
-- The one table main added that this branch does not have
-- ---------------------------------------------------------------------------------------------

-- A user's standing grant of an OAuth app. `0231` stores the *grant record for one request*
-- (`oauth_authorization_codes`, which expires); this stores the decision itself, so consent is
-- asked once per (app, user) rather than on every authorization.
--
-- Both migrations create tables in the same namespace, so the constraint and index names are
-- main's — renaming them would change main's migration in place, and a migration that has been
-- applied anywhere must keep its checksum.
create table if not exists oauth_authorizations (
    -- The app the grant is for.
    app_id uuid not null references oauth_apps(id) on delete cascade,
    -- Who granted it.
    user_id uuid not null references users(id) on delete cascade,
    -- The scopes granted, as a narrowing of the app's own list.
    scopes text[] not null,
    granted_at timestamptz not null default now(),
    revoked_at timestamptz,
    constraint oauth_authorizations_uk unique (app_id, user_id)
);

create index if not exists oauth_authorizations_user_ix
    on oauth_authorizations (user_id, granted_at desc);

-- ---------------------------------------------------------------------------------------------
-- Indexes `main`'s file added that `0223` does not have
-- ---------------------------------------------------------------------------------------------

-- **On `prefix`, not `key_prefix`.** This is the single line that made `main`'s version of this
-- file fail on this branch, and it is worth stating why the correction is not optional: an index
-- on a column that does not exist is a hard error, and `create table if not exists` had already
-- reported success for the table this index belongs to.
--
-- This is the sign-in path's hot lookup — a presented token's first ten characters resolve to at
-- most one row, and then the hash comparison decides. `0223` already creates a unique index on
-- `(prefix, secret_hash)`, which covers that lookup; the index below is the *prefix-only*
-- variant, which serves a panel filter that narrows a key list by prefix without the hash.
create index if not exists api_keys_prefix_only_ix on api_keys (prefix);

-- `main`'s name for the org-scoped list index. `0223` has `api_keys_org_revoked_idx`, which is
-- `(organization_id, revoked_at)` — a different order, answering "the live keys of this
-- organization". This one answers "the newest keys of this organization", which is the order
-- the list screen renders in and the order an audit reads in. Two different questions, two
-- different orders, so both exist.
create index if not exists api_keys_org_created_ix
    on api_keys (organization_id, created_at desc);

-- An organization-scoped GET must not walk the whole platform's traffic. This is the log
-- screen's most expensive query and this is the index that answers it.
create index if not exists api_request_logs_org_status_ix
    on api_request_logs (organization_id, status, created_at desc);