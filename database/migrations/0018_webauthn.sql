-- Omnion · 0018 · WebAuthn ceremony challenges (REQ-006, slice 3b).
--
-- Additive by design (docs/05-VERSIONING.md). A passkey ceremony needs a challenge that this
-- server issued, is single-use, and belongs to one account and one purpose; `mfa_challenges`
-- carries hashed sign-in tokens, which is a different shape, so the ceremony gets its own small
-- table rather than a widened one.
--
-- The factor rows themselves need nothing new: `0011` already gave `mfa_factors` its
-- `credential_id`, `public_key`, `sign_count` and `transports` columns, the confirm-shape
-- constraint that keeps a webauthn row from being stored without a key, and the unique
-- `mfa_factors_credential_key` index (one live enrolment per credential id, installation-wide).

create table webauthn_challenges (
    id          uuid        primary key default gen_random_uuid(),
    user_id     uuid        not null references users (id) on delete cascade,
    purpose     text        not null,
    -- The base64url challenge exactly as the browser saw it. It is not a secret: it is a nonce
    -- this server must recognise again, and it is compared as a whole string.
    challenge   text        not null,
    rp_id       text        not null,
    expires_at  timestamptz not null,
    consumed_at timestamptz,
    created_at  timestamptz not null default now(),
    constraint webauthn_challenges_purpose_check
        check (purpose in ('registration', 'authentication'))
);

-- A challenge is looked up by the account and purpose that issued it, newest first.
create index webauthn_challenges_user_idx
    on webauthn_challenges (user_id, purpose, created_at desc);

-- One live challenge per (account, purpose): starting a ceremony again retires the previous
-- attempt, so an abandoned dialog can never leave a usable challenge behind.
create unique index webauthn_challenges_live_idx
    on webauthn_challenges (user_id, purpose) where consumed_at is null;

-- Passkeys are listed per account and kind the way TOTP factors are.
create index mfa_factors_user_kind_idx
    on mfa_factors (user_id, kind) where revoked_at is null;
