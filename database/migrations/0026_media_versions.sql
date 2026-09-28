-- Omnion · 0026 · media: version history (REQ-010, slice 2)
--
-- Slice 1 gave the library a file system. This slice gives it a *history*: replacing a file
-- writes a new version beside the old one instead of overwriting it, so a wrong upload is a
-- version away from gone rather than gone.
--
-- Three decisions carry the table:
--
--   * **A version is append-only.** A restore does not rewrite an old row — it copies the old
--     bytes to a NEW key and appends that as the newest version. History is therefore a straight
--     line of appends, and "what did this file look like on day 3" never depends on whether
--     anyone took a shortcut in between.
--   * **Version 1 is a backfill, not a rewrite.** Every live `media` row gets one version row
--     pointing at the storage key the row already carries, so `version_count` and the table
--     agree from the first read and an old library has a downloadable version 1.
--   * **The bytes are addressed by the version, not by the file.** `media.storage_key` keeps
--     pointing at the *current* version's key, so the panel read path never changes; each
--     version row carries the key of its own bytes.
--
-- The table is create-and-backfill only — no column of `media` is altered here, so the migration
-- runs against a live library without a lock-heavy rewrite, and it is append-only
-- (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- Versions
-- ---------------------------------------------------------------------------------------------

create table media_versions (
    id            uuid        primary key default gen_random_uuid(),
    media_id      uuid        not null references media (id) on delete cascade,
    -- 1, 2, 3… The gap is never reused: a purged version leaves a hole rather than letting a new
    -- one take a number a reader has already seen in an audit trail.
    version       integer     not null,
    storage_key   text        not null,
    size_bytes    bigint      not null,
    checksum      text        not null,
    content_type  text        not null,
    width         integer,
    height        integer,
    -- What the uploader said about this version ("fixed the crop").
    note          text        not null default '',
    -- Who created it. Null on a backfilled version 1: the row predates the history.
    created_by    uuid        references users (id) on delete set null,
    created_at    timestamptz not null default now(),
    constraint media_versions_one_per_number unique (media_id, version),
    constraint media_versions_number_positive check (version >= 1),
    constraint media_versions_size_not_negative check (size_bytes >= 0)
);

-- The history read: one file, newest version first. The unique index above already serves
-- "version N of this file", and this one serves the listing without a sort.
create index media_versions_media_idx on media_versions (media_id, version desc);
-- "Which bytes is this row's current version?" — a duplicate report groups by checksum across a
-- site, and a purge sweep wants every key a file owns.
create index media_versions_checksum_idx on media_versions (checksum);

comment on table media_versions is
    'Every version of one file, append-only. A restore appends a copy of an old version as the '
    'newest one instead of rewriting history (REQ-010).';
comment on column media_versions.storage_key is
    'Object key of THIS version''s bytes. `media.storage_key` tracks the current version only, so '
    'the panel read path stays stable while a restore repoints it.';

-- ---------------------------------------------------------------------------------------------
-- Backfill: version 1 for every file that exists
-- ---------------------------------------------------------------------------------------------

-- The existing row is version 1, whether it was uploaded before this migration or a minute
-- earlier. `created_by` and `created_at` are copied rather than defaulted, so the backfilled row
-- carries when the file actually arrived — a history that starts at "now" is a lie.
insert into media_versions (
    media_id, version, storage_key, size_bytes, checksum, content_type,
    width, height, created_by, created_at, note
)
select
    id, 1, storage_key, size_bytes, checksum, content_type,
    width, height, created_by, created_at, 'Imported with the file manager'
from media
on conflict (media_id, version) do nothing;

-- `version_count` was written by slice 1 as 1 for existing rows; a file that was replaced while
-- the table did not exist yet would disagree, and the detail screen trusts the table over the
-- counter only when the counter is lower.
update media set version_count = 1 where version_count < 1;
