-- Omnion · 0005 · media: the media library of a site
--
-- v0 of the media model (docs/01-VISION.md §5 "Media library", docs/requests/REQ-010). One row
-- describes one stored object: the bytes live in the object store (`omnion-storage` — MinIO in
-- development, any S3-compatible endpoint in production) under `storage_key`, while everything
-- the panel needs to list, filter and serve the file lives here.
--
-- Rows are written after the object is stored, so a row always points at something that exists;
-- the checksum is the SHA-256 of the bytes, which lets a later phase deduplicate uploads and
-- verify a fetch. Folders, tags and per-media visibility arrive with the file manager
-- (REQ-010); v0 keeps the model small but complete. Released migrations are append-only
-- (docs/05-VERSIONING.md).

create table media (
    id           uuid        primary key default gen_random_uuid(),
    site_id      uuid        not null references sites (id) on delete cascade,
    storage_key  text        not null,
    filename     text        not null,
    content_type text        not null,
    size_bytes   bigint      not null,
    checksum     text        not null,
    created_by   uuid        references users (id) on delete set null,
    created_at   timestamptz not null default now(),
    constraint media_size_positive check (size_bytes > 0),
    constraint media_filename_not_blank check (length(btrim(filename)) > 0),
    constraint media_content_type_format check (content_type ~ '^[a-z0-9!#$&^_.+-]+/[a-z0-9!#$&^_.+-]+$'),
    constraint media_checksum_format check (checksum ~ '^[a-f0-9]{64}$'),
    constraint media_storage_key_unique unique (storage_key)
);

create index media_site_id_idx on media (site_id);
create index media_created_at_idx on media (created_at desc);
