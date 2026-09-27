-- Omnion · 0019 · organization memberships and invitations (REQ-005, slice 1)
--
-- `users.organization_id` is the "home" organization a user was created in; until now that
-- single column was the whole tenant model, so an account could belong to exactly one tenant
-- and there was no way to invite anybody into one. This migration makes the tenant layer
-- first class (docs/requests/REQ-005):
--
-- * `organization_members` is the truth for "who belongs where". A user may belong to several
--   organizations, each membership carrying its own status and an optional primary flag.
--   `users.organization_id` stays exactly where it is and is backfilled as the primary
--   membership, so nothing that exists today changes behaviour.
-- * `organization_invitations` is a hashed, single-use token with an expiry. The raw token is
--   returned once, at creation, and only its hash is stored — the same rule sessions follow.
--
-- Additive by design (docs/05-VERSIONING.md): no column is dropped, no constraint on an
-- existing table is tightened, and the backfill inserts with `on conflict do nothing`, so a
-- live installation survives it.

create table organization_members (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    user_id         uuid        not null references users (id) on delete cascade,
    status          text        not null default 'active',
    is_primary      boolean     not null default false,
    joined_at       timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint organization_members_status_check
        check (status in ('active', 'invited', 'suspended'))
);

-- One membership per (organization, user): a second invite for the same pair is the same row.
create unique index organization_members_org_user_key
    on organization_members (organization_id, user_id);

-- A user has at most one primary organization — the one `users.organization_id` points at.
create unique index organization_members_primary_key
    on organization_members (user_id) where is_primary;

create index organization_members_user_idx on organization_members (user_id);
create index organization_members_org_status_idx on organization_members (organization_id, status);

-- Invitations. `token_hash` is what the public route looks a token up by; the raw token exists
-- only in the creator's response and the invitation e-mail.
create table organization_invitations (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    email           text        not null,
    role_id         uuid        references roles (id) on delete set null,
    token_hash      text        not null,
    invited_by      uuid        references users (id) on delete set null,
    status          text        not null default 'pending',
    message         text        not null default '',
    expires_at      timestamptz not null,
    accepted_by     uuid        references users (id) on delete set null,
    accepted_at     timestamptz,
    created_at      timestamptz not null default now(),
    constraint organization_invitations_status_check
        check (status in ('pending', 'accepted', 'revoked', 'expired')),
    constraint organization_invitations_message_length check (length(message) <= 400)
);

create unique index organization_invitations_token_hash_key
    on organization_invitations (token_hash);

-- An address cannot hold two live invitations in the same organization; the second invite
-- returns the pending one instead of creating a duplicate.
create unique index organization_invitations_pending_key
    on organization_invitations (organization_id, lower(email)) where status = 'pending';

create index organization_invitations_org_idx on organization_invitations (organization_id, status);
create index organization_invitations_expires_idx on organization_invitations (expires_at)
    where status = 'pending';

-- Backfill: every account that already carried a home organization gets exactly one primary
-- membership, and every account without one gets none (a platform account belongs to no
-- tenant until it is invited to one). `joined_at` is the account's own creation moment, so the
-- Members tab reads the same history it would have read from `users.created_at`.
insert into organization_members (organization_id, user_id, status, is_primary, joined_at)
select u.organization_id, u.id, 'active', true, u.created_at
  from users u
 where u.organization_id is not null
on conflict (organization_id, user_id) do nothing;
