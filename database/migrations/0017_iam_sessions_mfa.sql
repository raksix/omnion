-- Omnion · 0017 · Sessions, devices and MFA (REQ-006, slice 3).
--
-- Additive by design (docs/05-VERSIONING.md): one column on `sessions` and one new table for the
-- pending second-factor challenges. The tables this slice writes (`user_devices`, `mfa_factors`,
-- `mfa_recovery_codes`, `security_policies`, `sign_in_attempts`) already exist in `0011` — this
-- migration only completes what they cannot express.

-- ---------------------------------------------------------------------------------------------
-- Sessions
-- ---------------------------------------------------------------------------------------------

-- When the caller last proved they are still the person at the keyboard. Dangerous operations
-- (resetting another account's factors, issuing a machine key) demand a fresh mark; ordinary
-- work refreshes it only through an explicit step-up (docs/07-IAM.md §15).
alter table sessions add column step_up_at timestamptz;

-- The panel lists live sessions per account, newest first; the v0 user index does not cover the
-- "live only" predicate the list filters on.
create index sessions_live_idx on sessions (user_id, created_at desc) where revoked_at is null;

-- ---------------------------------------------------------------------------------------------
-- Second-factor challenges
-- ---------------------------------------------------------------------------------------------

-- A sign-in whose account holds a confirmed factor is not finished by the password alone: it
-- produces a challenge token (hashed, short-lived, single-use) and only the code that consumes
-- it starts a session. The same table carries the step-up challenge, because both are "prove a
-- factor, once, soon".
create table mfa_challenges (
    id          uuid        primary key default gen_random_uuid(),
    user_id     uuid        not null references users (id) on delete cascade,
    purpose     text        not null,
    token_hash  text        not null,
    ip_address  inet,
    user_agent  text,
    expires_at  timestamptz not null,
    consumed_at timestamptz,
    created_at  timestamptz not null default now(),
    constraint mfa_challenges_purpose_check check (purpose in ('login', 'step_up'))
);

-- A token hash is unique, so a consumed challenge can never be replayed.
create unique index mfa_challenges_token_key on mfa_challenges (token_hash);
create index mfa_challenges_user_idx on mfa_challenges (user_id, created_at desc);

-- Anything left behind by an earlier boot is unusable the moment its window passed; the rows are
-- kept for the audit trail the security centre reads, so nothing is deleted here.
create index mfa_challenges_expires_idx on mfa_challenges (expires_at)
    where consumed_at is null;
