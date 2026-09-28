-- Omnion · 0027 · media: transformation presets and the derivative cache (REQ-010, slice 3)
--
-- Slice 1 gave the library folders, slice 2 a history. This slice gives it *derivatives*: the
-- same file served at the size a page actually needs, without a build step and without a matrix
-- of pre-generated files.
--
-- Four decisions carry the migration, and each is a place the obvious shortcut is wrong:
--
--   * **On demand, never a generated matrix.** A pre-generation job writes presets x files, and a
--     site with 20 presets and 10 000 files has 200 000 objects it will mostly never serve. The
--     first request for a (file, preset) pair pays for the work; every later one reads a cache.
--   * **The cache key is content-addressed and includes the preset's own definition.** The key is
--     `sha256(checksum | preset body)`, so editing a preset's quality does not serve the previous
--     quality's pixels under the new name, and it is a pure function of inputs — two replicas
--     computing the same key produce the same object, which is what makes a CDN able to hold it.
--   * **A version's bytes are transformed, never a version's *name*.** The row's own checksum is
--     the identity of the pixels, so a restore changes the derivative for free and a file that
--     happens to share bytes with another file shares its cache entry.
--   * **A derivative is a cache, not an asset.** `media_derivatives` rows point at generated
--     objects under a `derivatives/` prefix, are removed with the file that owns them, and can be
--     dropped and rebuilt at any time without a row of `media` noticing. Nothing in the publishing
--     path may store a derivative key in content.
--
-- The table is create-and-seed only — no column of `media` or `media_versions` is altered, so the
-- migration runs against a live library without a lock-heavy rewrite (docs/05-VERSIONING.md).
-- Every existing site receives the same `Standard` preset the spec names, so the preset URL works
-- the moment the migration lands and an existing page that already asks for `?preset=card` is not
-- broken by a name that does not exist.

-- ---------------------------------------------------------------------------------------------
-- Transformation presets
-- ---------------------------------------------------------------------------------------------

create table media_transformation_presets (
    id          uuid        primary key default gen_random_uuid(),
    site_id     uuid        not null references sites (id) on delete cascade,
    -- The name is the URL: `/raw?preset=card`. It has to survive being typed, so the check below
    -- accepts only what a query string carries unescaped — a preset called `card hero` would have
    -- to be percent-encoded in every page that uses it and breaks on the first editor who does
    -- not.
    name        text        not null,
    -- At least one dimension is required: a preset with neither is the original, which the raw
    -- route already serves, and a row that means "the original" is a row that will drift out of
    -- sync with the file it pretends to describe.
    width       integer,
    height      integer,
    fit         text        not null default 'cover',
    format      text        not null default 'webp',
    quality     integer     not null default 80,
    -- A watermark is a reference to another file, not a copy of its bytes: the marked-up file
    -- inherits the new watermark, and the storage key never becomes a chain.
    watermark_media_id uuid references media (id) on delete set null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    constraint media_presets_name_unique unique (site_id, name),
    constraint media_presets_name_usable check (name ~ '^[a-z0-9][a-z0-9._-]{0,39}$'),
    constraint media_presets_dimensions_present check (width is not null or height is not null),
    -- 0 is a legal width for some filters, but not for a resampler: a preset that asks for a
    -- zero-pixel canvas produces an image no layout can draw.
    constraint media_presets_width_positive check (width is null or width between 1 and 8192),
    constraint media_presets_height_positive check (height is null or height between 1 and 8192),
    constraint media_presets_fit_valid check (fit in ('cover', 'contain', 'fill')),
    constraint media_presets_format_valid check (format in ('webp', 'avif', 'jpeg', 'png')),
    constraint media_presets_quality_valid check (quality between 1 and 100)
);

-- "Every preset of this site, in the order the settings screen lists them."
create index media_presets_site_idx on media_transformation_presets (site_id, name);

comment on table media_transformation_presets is
    'Named image transformations a site may ask for by name. The name is part of the public URL '
    'contract (REQ-010).';
comment on column media_transformation_presets.name is
    'URL-safe preset name. It appears in `/raw?preset=`, so the check accepts only characters a '
    'query string carries unescaped.';
comment on column media_transformation_presets.fit is
    'How the source is fitted into the target box: `cover` crops, `contain` letterboxes, `fill` '
    'stretches. The default is `cover` because a card is a fixed box.';

-- ---------------------------------------------------------------------------------------------
-- The derivative cache
-- ---------------------------------------------------------------------------------------------

create table media_derivatives (
    id            uuid        primary key default gen_random_uuid(),
    media_id      uuid        not null references media (id) on delete cascade,
    -- The preset the entry was built for. `on delete cascade` from the preset table means
    -- deleting a preset drops its cache entries; a stale derivative of a deleted preset is
    -- storage nobody can name.
    preset_id     uuid        not null references media_transformation_presets (id) on delete cascade,
    -- sha256 of `checksum | preset body`: the same inputs always produce the same key, on any
    -- replica, which is the property a CDN needs in order to hold the object for a year.
    cache_key     text        not null,
    storage_key   text        not null,
    content_type  text        not null,
    size_bytes    bigint      not null,
    -- Pixel width of the *result*, which is not the preset's width when `contain` letterboxes.
    width         integer     not null,
    height        integer     not null,
    -- The source bytes this was built from, so a replaced file's derivatives are distinguishable
    -- from the current ones without joining the history.
    source_checksum text      not null,
    created_at    timestamptz not null default now(),
    constraint media_derivatives_cache_key_unique unique (cache_key),
    constraint media_derivatives_size_not_negative check (size_bytes >= 0),
    constraint media_derivatives_dimensions_positive check (width > 0 and height > 0)
);

-- "Is this derivative already built?" — the check every read of a preset URL makes first.
create index media_derivatives_media_idx on media_derivatives (media_id, preset_id);
-- The retention sweep needs "every generated object this site owns", and a cache rebuild needs
-- the same list; neither should scan the whole table.
create index media_derivatives_storage_idx on media_derivatives (storage_key);

comment on table media_derivatives is
    'Generated derivatives, addressed by a content hash of their inputs. A cache: dropping every '
    'row costs time, not correctness (REQ-010).';
comment on column media_derivatives.cache_key is
    'sha256 of the source checksum and the preset definition. Two replicas computing the same '
    'inputs produce the same key, so a CDN can hold the object for a year without revalidating.';

-- ---------------------------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------------------------

-- Every site gets the same `Standard` preset. The seed is idempotent, so re-running the
-- migration on a partially migrated database does not fail on the unique constraint, and the
-- `Standard` name is reserved against a site deleting it by accident in a later migration.
insert into media_transformation_presets (site_id, name, width, height, fit, format, quality)
select s.id, 'standard', 1200, 630, 'cover', 'webp', 80
from sites s
where not exists (
    select 1 from media_transformation_presets p
    where p.site_id = s.id and p.name = 'standard'
);
