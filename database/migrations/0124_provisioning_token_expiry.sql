-- Rotation and expiry for SCIM provisioning tokens (REQ-065, slice 4 part 4).
--
-- `0011_iam_advanced.sql` gave `provisioning_tokens` a secret hash, a public prefix and a
-- `revoked_at`, and the module doc has claimed since that "tokens are rotatable" while the only
-- thing a caller could do was revoke — which is not rotation. Rotation is *replace*, and the
-- difference is visible in the audit trail: a revoke answers "this token is dead", a rotation
-- answers "this token is dead and this is what replaced it", and only the second one can be
-- acted on without opening the database.
--
-- Three columns, and the third is the one that matters most:
--
--   `rotated_to`   the token that replaced this one. A rotation leaves a chain, so a reader who
--                  found a leaked secret can walk *forward* to the live token and *back* to the
--                  leak. Without it, every rotation is an anonymous deletion.
--   `rotated_at`   when the replacement happened.
--   `expires_at`   the spec asks for "expiry required" and the table had no way to express it:
--                  a token minted in 2024 was still accepted in 2026. A token that never expires
--                  is a token nobody has to review, and the one credential whose compromise is
--                  silent (there is no user, no login, no password) is the one that must be
--                  time-boxed.
--
-- `expires_at` is nullable *only* for rows that already exist: a backfill is impossible without
-- inventing a date per token, and a wrong expiry is worse than none. New tokens get one, the
-- default in the route layer, so the column is populated going forward and the existing rows are
-- the visible, enumerable set "needs a decision" rather than a silent permanent exemption.
--
-- The check constraint is what makes "expiry required" true rather than advisory, and it is
-- deliberately loose about the value: a token expiring before it was *created* is refused, but
-- the constraint does not police how far ahead the expiry sits, because a connector that
-- re-provisions hourly and a connector that pushes quarterly have no shared sensible answer.
create table if not exists provisioning_token_rotations (
    -- the retired token
    token_id     uuid        primary key references provisioning_tokens (id) on delete cascade,
    -- what replaced it
    rotated_to   uuid        references provisioning_tokens (id) on delete set null,
    rotated_at   timestamptz not null default now(),
    rotated_by   uuid        references users (id) on delete set null
);

comment on table provisioning_token_rotations is
    'A SCIM token this one replaced. Kept so a rotation is a chain, not an anonymous revoke.';

create index if not exists provisioning_token_rotations_rotated_to_idx
    on provisioning_token_rotations (rotated_to)
    where rotated_to is not null;

alter table provisioning_tokens
    add column if not exists expires_at timestamptz,
    add column if not exists rotated_at  timestamptz;

comment on column provisioning_tokens.expires_at is
    'When this token stops being accepted. NULL only for tokens minted before expiry existed.';
comment on column provisioning_tokens.rotated_at is
    'Set when this token was replaced by a newer one; the successor is in provisioning_token_rotations.';

-- A token cannot expire before it existed. This is the only rule the database enforces, because
-- it is the only one that cannot be wrong about somebody's provisioning schedule.
alter table provisioning_tokens
    drop constraint if exists provisioning_tokens_expiry_after_creation;

alter table provisioning_tokens
    add constraint provisioning_tokens_expiry_after_creation
    check (expires_at is null or expires_at > created_at);
