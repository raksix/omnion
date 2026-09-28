-- Omnion · 0025 · media: the enterprise file manager (REQ-010, slice 1)
--
-- v1 of the library turns a flat list into a file system. Two ideas carry the whole migration:
--
--   * a folder is a row, and its `path` is the answer to "where is it" in one read, so a breadcrumb
--     and a "used in" check never have to walk the tree;
--   * a delete is a state, not an act — `deleted_at` + `deleted_by` move a file to the trash and
--     the retention policy of the site decides when its bytes leave, so a mistaken delete is
--     recoverable for as long as the policy keeps it.
--
-- Every added column on `media` is nullable or carries a default that reads as the pre-file-
-- manager value: an existing row keeps `folder_id = null` (the library root), `version_count = 1`,
-- `scan_status = 'pending'` and `deleted_at = null`. Nothing is rewritten, so the migration runs
-- against a live library without a stop-the-world backfill, and it is append-only
-- (docs/05-VERSIONING.md).
--
-- Slice 1 ships folders, the browser columns, the trash and the `media.folders.manage` /
-- `media.trash.manage` permission keys. Slices 2–4 add versions, presets, grants, scanning and
-- retention on top of these tables without altering them again.

-- ---------------------------------------------------------------------------------------------
-- Folders
-- ---------------------------------------------------------------------------------------------

create table media_folders (
    id          uuid        primary key default gen_random_uuid(),
    site_id     uuid        not null references sites (id) on delete cascade,
    parent_id   uuid        references media_folders (id) on delete restrict,
    name        text        not null,
    -- Materialised path from the library root, without a leading or trailing slash: `Campaigns`
    -- at the root is `Campaigns`, a folder `2026` inside it is `Campaigns/2026`. Breadcrumbs,
    -- search and "which files live under this folder" are one indexed read instead of a
    -- recursive walk.
    path        text        not null,
    created_by  uuid        references users (id) on delete set null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    constraint media_folders_name_not_blank check (length(btrim(name)) > 0),
    constraint media_folders_path_not_blank check (length(btrim(path)) > 0),
    -- A folder cannot be its own parent; a cycle deeper than that is refused by the API, which
    -- knows the tree, while this constraint keeps the trivial one out of the table.
    constraint media_folders_not_own_parent check (parent_id is null or parent_id <> id)
);

-- The tree of one site: siblings under one parent, listed in name order.
create index media_folders_tree_idx on media_folders (site_id, parent_id, name);
-- `path` is the lookup key for "every folder under X" and for cycle detection.
create index media_folders_path_idx on media_folders (site_id, path);
create unique index media_folders_sibling_name_idx on media_folders (site_id, parent_id, name)
    where parent_id is not null;
-- A site has exactly one root row, and two children of one parent never share a name. The two
-- partial indexes state that without a nullable-column unique index (`parent_id` is null only for
-- the root), so two concurrent creates cannot both land a `Campaigns` folder.
create unique index media_folders_root_name_idx on media_folders (site_id)
    where parent_id is null;

-- ---------------------------------------------------------------------------------------------
-- The library row, extended
-- ---------------------------------------------------------------------------------------------

alter table media add column folder_id uuid references media_folders (id) on delete set null;
-- Last change that was not a new version (rename, move, metadata edit).
alter table media add column updated_at timestamptz;
-- Soft delete: set on delete, cleared on restore. `purged_at` is written when the bytes and the
-- row are actually removed, so the two moments can be told apart in an activity trail.
alter table media add column deleted_at timestamptz;
alter table media add column deleted_by uuid references users (id) on delete set null;
alter table media add column purged_at timestamptz;
-- Accessibility and editorial fields, all defaulting to empty so an old row reads as blank.
alter table media add column alt_text text not null default '';
alter table media add column caption text not null default '';
alter table media add column description text not null default '';
-- Editor-defined key/value pairs; the GIN index makes "which files carry this field" a read.
alter table media add column metadata jsonb not null default '{}'::jsonb;
alter table media add column tags text[] not null default '{}';
-- Filled from the bytes on upload where the format carries them (slice 2).
alter table media add column width integer;
alter table media add column height integer;
alter table media add column duration_ms integer;
alter table media add column page_count integer;
alter table media add column scan_status text not null default 'pending';
alter table media add column scan_detail text not null default '';
alter table media add column version_count integer not null default 1;
alter table media add column is_public boolean not null default false;

alter table media add constraint media_version_count_positive check (version_count >= 1);
alter table media add constraint media_scan_status_known check (
    scan_status in ('pending', 'clean', 'flagged', 'skipped', 'error')
);

-- The browser's main query: one site, one folder, live rows only, newest first.
create index media_folder_idx on media (site_id, folder_id, created_at desc);
-- A partial index over live rows only: the library root of a busy site stays cheap.
create index media_live_idx on media (site_id, created_at desc) where deleted_at is null;
-- The trash listing and the retention worker's sweep both read the trashed rows.
create index media_trash_idx on media (site_id, deleted_at desc) where deleted_at is not null;
-- Duplicate detection groups by checksum inside a site (slice 3).
create index media_checksum_idx on media (site_id, checksum);
-- The scanner worker's claim query.
create index media_scan_idx on media (created_at) where scan_status = 'pending';
-- Tag and metadata filters.
create index media_tags_idx on media using gin (tags);
create index media_metadata_idx on media using gin (metadata jsonb_path_ops);

update media set updated_at = created_at where updated_at is null;

-- ---------------------------------------------------------------------------------------------
-- The library root, made explicit
-- ---------------------------------------------------------------------------------------------

-- A site with no folder row at all still has a root: the browser needs a stable id to deep-link,
-- to address uploads ("upload into the root") and to hold the root's own grants later. The root
-- is materialised per site by the migration itself, so no API call has to invent one.
insert into media_folders (site_id, parent_id, name, path)
select id, null, 'Media', 'Media' from sites
on conflict do nothing;

comment on table media_folders is
    'A folder of one media library. `path` is the materialised path from the library root, so a '
    'breadcrumb, a subtree query and a cycle check are one indexed read (REQ-010).';
comment on column media.deleted_at is
    'When the file was moved to the trash; null while it is live. A delete never removes bytes '
    'immediately - the retention policy decides when it does (REQ-010).';
comment on column media.updated_at is
    'Last change that was not a version. Written for every rename, move and metadata edit, so the '
    'browser can sort by "modified" without scanning versions.';
