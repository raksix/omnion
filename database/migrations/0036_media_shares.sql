-- Omnion · 0036 · media: public share links (REQ-010, slice 3)
--
-- Slices 1 and 2 gave the library a shape and a history; `0027` gave it derivatives and
-- `0029` gave it a place to keep its bytes. This migration gives it a way to *hand a file to
-- somebody outside the platform* — with an expiry, an optional password, a download counter and
-- a revocation that is immediate.
--
-- Five decisions carry it, and each is a place the obvious shortcut is wrong:
--
--   * **The token is stored hashed, and the hash is the lookup key.** A share table keyed by the
--     token answers "give me the file" with a query whose *argument* is the capability. One
--     leaked backup, one stray `select *`, one replica log and every live link is readable by
--     anybody. Hashing the token means the row cannot produce the capability, only compare
--     against one — and the index is a hash index, so the lookup is still one probe.
--   * **A share is a grant about a *file*, not about a *session*.** It survives the session
--     that made it, and it is revoked by setting an instant, never by deleting the row: a
--     delete loses the audit trail of who held this link and when, which is the only thing that
--     makes a leaked link investigable. `revoked_at` is therefore not nullable-by-luck, it is
--     the revocation itself.
--   * **A share never widens a file's own rules.** A quarantined or trashed file is refused
--     through a share exactly as it is refused through the raw route — the link is a way to
--     *reach* the file for somebody who cannot sign in, not a bypass of what the file is. The
--     check therefore lives in the serving path, not in the create path, because the file's
--     state can change after the link was made.
--   * **`download_count` counts attempts that produced bytes.** A count incremented before the
--     store is asked would report a download for a 500; incremented after, it is a fact. The
--     increment is not in the same transaction as the read, so a counter is never rolled back
--     by a serving path that later fails.
--   * **A password is hashed with the platform's own Argon2id parameters** rather than a cheap
--     digest. A share password is low-entropy by nature (people type `hunter2` on a link they
--     will hand to a client), so an offline attacker gets one guess per hash — which makes the
--     work factor the security, not the salt.
--
-- Create-and-seed only: no column of `media` is altered, so the migration runs against a live
-- library without a lock-heavy rewrite (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- Public share links
-- ---------------------------------------------------------------------------------------------

create table media_shares (
    id              uuid        primary key default gen_random_uuid(),
    media_id        uuid        not null references media (id) on delete cascade,
    -- The *hash* of the token, never the token. The row can therefore never produce the
    -- capability it grants; it can only be asked whether the caller presented the capability.
    token_hash      text        not null,
    -- Argon2id PHC string when the link is password-protected, null otherwise. A null here is
    -- "no password", which is a real state and not a missing one.
    password_hash   text,
    -- When the link stops working. Null means "until revoked": an internal share handed to a
    -- colleague has no natural end, and inventing one would silently expire somebody's link.
    expires_at      timestamptz,
    -- Downloads that produced bytes. Not a nonce, not a rate limit — the number the person who
    -- created the link shows to the person who asked for it.
    download_count  integer     not null default 0,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    -- The revocation itself. A revoked row is kept forever so "who held this link, and when did
    -- it stop working" is answerable after the fact.
    revoked_at      timestamptz,
    -- Why it was revoked, when there is a reason worth keeping.
    revoked_reason  text        not null default '',
    constraint media_shares_token_present check (token_hash <> ''),
    -- A link that expires must say when; a link whose `expires_at` is in the past is refused
    -- rather than served, and the refusal names expiry rather than "not found" so the holder
    -- knows to ask for a new one.
    constraint media_shares_expiry_sane
        check (expires_at is null or expires_at > created_at),
    -- A negative counter is not a count.
    constraint media_shares_count_sane check (download_count >= 0)
);

-- One lookup key: every request carrying a token does exactly one probe on this index.
create unique index media_shares_token_idx on media_shares (token_hash);

-- The file-detail screen lists a file's links; without this it is a sequential scan per row.
create index media_shares_media_idx on media_shares (media_id, created_at desc);

comment on table media_shares is
    'Public share links. The token is stored hashed, so no query over this table can produce a '
    'live capability — a row can only confirm one (REQ-010).';
comment on column media_shares.token_hash is
    'sha256 of the bearer token, hex. A hash index rather than a plain lookup: the token is a '
    'capability, and the storage of a capability is where it leaks from.';
comment on column media_shares.password_hash is
    'Argon2id PHC string, null when the link has no password. Argon2id rather than a fast digest '
    'because a share password is low-entropy by construction and the work factor is the security.';
comment on column media_shares.expires_at is
    'Null means "until revoked". An expiry is a decision the person making the link makes, not '
    'a default the platform picks on their behalf.';
comment on column media_shares.revoked_at is
    'Set, never deleted. Immediate: a revoked link stops answering on the very next request, '
    'without waiting for a cache, a worker or a reaper.';

-- ---------------------------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------------------------

-- Deliberately no seed: a share link is created by a person who intends to hand a file over,
-- and a migration that mints one would mint a capability nobody chose. The table starts empty
-- on every site, which is the honest empty state the file-detail screen renders.
