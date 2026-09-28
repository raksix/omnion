-- Omnion · 0038 · media: references and duplicate merging (REQ-010, slice 3)
--
-- The library grew a shape (0025), a history (0026), derivatives (0027), a place to keep its
-- bytes (0029) and a way to hand a file to somebody outside (0036). What it could not answer was
-- the question an operator asks first once a library is any size: **"how much of this is the same
-- file twice?"** This migration gives it that answer, and — more importantly — gives it the one
-- rule that makes the answer actionable.
--
-- Six decisions carry it, and each is a place the obvious shortcut is wrong:
--
--   * **A group is two *live* files inside one site.** A trashed copy is not a duplicate: it is
--     already on its way out, and counting it would make the report claim space that the trash
--     screen is about to return anyway. Grouping across sites is a *different question* — the
--     platform owner's "what does this installation store twice?" — so it is a separate mode
--     behind a permission and a label, never the default.
--   * **A group of one is not a duplicate.** A report whose rows are "every file, alone" is a
--     second file browser with a storage warning painted on it, and its reclaimable column reads
--     like an emergency on an empty library.
--   * **The report never picks the keeper.** An automatic keeper has to break the tie by upload
--     time or by id, which means the merge quietly chooses *some* file and the operator finds
--     out from a broken page afterwards. The caller names the row it keeps, and the API refuses a
--     keeper that is not in the group.
--   * **A merge moves references and trashes copies — it never deletes.** Every other destructive
--     action in the library has a recovery path, and this one sitting next to `Empty trash` as the
--     only irreversible button would be the wrong place to break that. The copies land in the
--     trash the ordinary way, with the retention countdown already running.
--   * **The bytes are still there after a merge, so the report must not claim otherwise.**
--     "Reclaimable" is the size the *purge of those trashed copies* will return, not the size the
--     merge has returned. A report that shows freed bytes immediately teaches the operator to
--     believe a number that is a week old.
--   * **The merge is one transaction.** Repointing references and then failing to trash leaves a
--     library that claims two rows are one file while the pages pointing at the second still
--     point at bytes that are about to be reclaimed.

-- ---------------------------------------------------------------------------------------------
-- Where a file is used
-- ---------------------------------------------------------------------------------------------

-- The reference rows the merge repoints, and the "used in" tab slice 4 will read.
--
-- A reference names *a record and a field*, never a copy of the value: `('page', <uuid>,
-- 'hero_image_id')`. That is what makes the repoint a two-column update rather than a search of
-- every column of every table, and it is why the table has no foreign key on the record — the
-- referent is polymorphic by design and the row is a statement about it, not about a row of it.
--
-- `on delete cascade` from `media` is deliberate and is the *only* place a hard delete of a file
-- is allowed to take a reference with it: a purged file cannot be referenced by anything, and a
-- surviving reference would be a lie about a file that does not exist.
create table media_references (
    id            uuid        primary key default gen_random_uuid(),
    media_id      uuid        not null references media (id) on delete cascade,
    -- What kind of thing refers to the file (`page`, `theme`, `form`, …). An open string: a
    -- module that arrives later must be able to register a kind without a migration here.
    resource_kind text        not null,
    -- The referent's own id, as text — it is a uuid today and may be a slug tomorrow.
    resource_id   text        not null,
    -- Which field of that record points here. Empty means "the record itself is the file".
    field         text        not null default '',
    created_at    timestamptz not null default now(),
    constraint media_references_unique
        unique (media_id, resource_kind, resource_id, field),
    constraint media_references_kind_present check (resource_kind <> ''),
    constraint media_references_resource_present check (resource_id <> '')
);

-- The lookup the report runs: every reference of one file, and the repoint the merge runs.
create index media_references_media_idx on media_references (media_id);
-- The reverse direction, for the "used in" tab: what a page or a theme points at.
create index media_references_resource_idx
    on media_references (resource_kind, resource_id);

comment on table media_references is
    'Where a media file is used: the rows a duplicate merge repoints and the "used in" list the '
    'file detail screen shows (REQ-010).';
comment on column media_references.resource_kind is
    'Kind of referring record (`page`, `theme`, `form`, …). An open string so a later module '
    'registers its own kind without a migration.';
comment on column media_references.field is
    'Which field of the referring record points at the file, e.g. `hero_image_id`. Empty when '
    'the record *is* the file.';

-- ---------------------------------------------------------------------------------------------
-- Duplicate groups
-- ---------------------------------------------------------------------------------------------

-- A group is not a table. It is a projection of `media` over its own checksum, and storing it
-- would mean a second thing to keep in step with the first: a replace changes the checksum, a
-- delete removes a row, a restore brings one back, and every one of those would have to fire a
-- trigger or be re-aggregated. The checksum is already unique-indexed per site (`0025`), so the
-- grouping is a scan of an index rather than a join against a denormalised copy.
--
-- `sum(size_bytes)::bigint` is cast on purpose: PostgreSQL returns `sum(bigint)` as `numeric`,
-- and `numeric` is not the type sqlx decodes into an `i64`. The cast keeps the report's byte sum
-- on the same integer the row itself carries, so a size shown in the report and a size shown on
-- the file are the same number rather than two renderings of one.
create or replace view media_duplicate_groups as
select m.site_id,
       m.checksum,
       count(*)::integer                        as file_count,
       sum(m.size_bytes)::bigint                as total_bytes,
       sum(m.size_bytes)::bigint - min(m.size_bytes)::bigint as reclaimable_bytes,
       min(m.created_at)                        as first_seen,
       max(m.created_at)                        as last_seen
from media m
where m.deleted_at is null
  and m.checksum <> ''
group by m.site_id, m.checksum
having count(*) > 1;

comment on view media_duplicate_groups is
    'Live files of a site that share a checksum, with the bytes a merge would put in the trash. '
    'Reclaimable excludes the keeper''s own copy, so the number is what a purge returns and not '
    'the size of the whole group (REQ-010).';
