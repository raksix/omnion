-- Omnion · 0047 · media: folder and file grants (REQ-010, slice 4)
--
-- Slice 1 gave the library folders, slice 2 a history, slice 3 transformations, storage
-- settings, share links and duplicate detection, and `0044` gave the scan column a pipeline.
-- What is still missing is the *access* half of a file manager: a folder nobody outside the
-- team may open, a draft hero image the marketing site must not publish, a file one person may
-- read and nobody may share.
--
-- Four decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **A grant is a narrowing, never a widening.** The platform already has an answer to "may
--     this account touch the library" — the permission catalogue, evaluated through roles and
--     bindings (docs/07-IAM.md). A second layer that could *hand out* capabilities would be a
--     second, un-audited source of truth beside the first, and the first thing anybody would
--     build on it is "I gave a contractor read on the whole library and he cannot sign in to
--     the panel". So the chain may only remove: a `deny` refuses before the permission
--     catalogue is consulted at all, and a row that says nothing about a subject leaves the
--     catalogue's answer standing. The table is the ability to say *not this one, not here*.
--   * **A deny beats an inherited allow, at any depth.** This is the rule people expect and
--     the one a naive "nearest node wins" walk gets wrong: a `deny` on the file would be
--     overruled by an `allow` on the root, and a root allow is exactly what an organisation
--     grants everybody without thinking. Deny-wins is also the only ordering where a mistake
--     in *either* direction fails closed. It is pinned per capability by a unit test that walks
--     every depth of both orders.
--   * **The subject is polymorphic and unconstrained on purpose.** `subject_kind` is `user`,
--     `group` or `role` and `subject_id` is a bare uuid, because the three live in three
--     tables that the media crate does not own and a fourth foreign key would make a grant on
--     a deleted group un-deletable. A row naming an id that no longer exists is inert — it can
--     deny a subject nobody is, and it can never grant one, so the failure mode is a stale
--     line in a table rather than a capability with no owner. The subjects that *can* be
--     picked are filtered by the picker against the organization's own users, groups and
--     roles, so this only ever happens through a direct database write.
--   * **A grant names exactly one node.** `(folder_id is null) <> (media_id is null)` rather
--     than two nullable columns with no rule between them: a row with neither is a grant on
--     nothing that would resolve to "the whole site", and a row with both is ambiguous in
--     exactly the way that makes two code paths decide differently. Refusing both at the
--     database is what lets the reader be one statement.
--
-- The capability bits are the *narrower* set than the four permission keys on purpose:
-- read/write/delete/share, matching the spec's grant table. `share` is a bit of its own
-- because it is the one that hands bytes to somebody who never signs in, and a folder an
-- editor may reorganise is not a folder whose contents go outside the organization.

-- ---------------------------------------------------------------------------------------------
-- Grants
-- ---------------------------------------------------------------------------------------------

create table media_grants (
    id              uuid        primary key default gen_random_uuid(),
    -- Exactly one of the two. A grant on nothing would be a grant on the whole site, and a
    -- grant on both would be a row two resolvers could each claim.
    folder_id       uuid        references media_folders (id) on delete cascade,
    media_id        uuid        references media (id) on delete cascade,
    -- `user`, `group` or `role` (docs/07-IAM.md §8, §9, §14). A bare id: the three subject
    -- tables are the IAM's, not the media crate's.
    subject_kind    text        not null,
    subject_id      uuid        not null,
    can_read        boolean     not null default true,
    can_write       boolean     not null default false,
    can_delete      boolean     not null default false,
    can_share       boolean     not null default false,
    -- `allow` records that this subject was named here; `deny` is the only effect that
    -- refuses. Both are stored, because a permissions table that can only say "no" cannot
    -- answer "who was given access to this folder" — the question the screen is on.
    effect          text        not null default 'allow',
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    -- One row per (node, subject). A second `allow` for the same pair is a mistake the
    -- `upsert` path overwrites rather than accumulating two rows that disagree.
    constraint media_grants_node_xor
        check ((folder_id is null) <> (media_id is null)),
    constraint media_grants_subject_kind_known
        check (subject_kind in ('user', 'group', 'role')),
    constraint media_grants_effect_known
        check (effect in ('allow', 'deny')),
    constraint media_grants_effect_means_something
        check (effect = 'allow' or can_read or can_write or can_delete or can_share),
    constraint media_grants_unique_subject
        unique (coalesce(folder_id, '00000000-0000-0000-0000-000000000000'::uuid),
                coalesce(media_id, '00000000-0000-0000-0000-000000000000'::uuid),
                subject_kind, subject_id)
);

comment on table media_grants is
    'Folder and file access grants (REQ-010). A grant can only narrow: a `deny` refuses a '
    'capability before the permission catalogue is consulted, and a subject no row names keeps '
    'whatever the catalogue said. Deny wins over an allow at any depth in the tree.';

create index media_grants_folder_idx on media_grants (folder_id) where folder_id is not null;
create index media_grants_media_idx on media_grants (media_id) where media_id is not null;
-- The subject side of the picker and of a "who can see this" report walks the other way.
create index media_grants_subject_idx on media_grants (subject_kind, subject_id);

-- There is deliberately **no site-level row and no `sites` trigger** here, unlike `0028`,
-- `0029` and `0044`, which all closed the "a site created after the migration has nothing"
-- gap. A grant attaches to a folder or a file, and a new site has neither until somebody
-- opens the library — so there is no node for a seeded row to point at, and a trigger would
-- have nothing to write. The root folder is materialised on first use by `root_folder()`
-- (crates/media/src/folder_store.rs), and a grant on it is a grant somebody made.
