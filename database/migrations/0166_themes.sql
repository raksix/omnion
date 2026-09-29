-- Themes & site activation (REQ-062 slice 1).
--
-- Two decisions are visible in the schema itself, and both were the obvious alternative
-- before they were not.
--
-- 1. **A bundled theme is a FILE, not a row.** The loader reads `themes/<key>/omnion.theme.json`
--    at boot and mirrors the manifests into `themes` so the gallery has something to list; an
--    installed package is a row that no file backs. `source` therefore decides who may delete
--    a theme, and a CHECK refuses an `uploaded` row with no storage key: a package whose bytes
--    are missing is a theme that cannot be re-installed after a restore, and the table is the
--    last place that can still say so.
--
-- 2. **Activation records the theme it replaced, in the same row.** `previous_theme_key` is
--    what "Restore previous" reads, and it is written by the activation that displaced it — not
--    by the rollback. A rollback that computed the previous key would have to guess, and the
--    guess is wrong exactly twice: after two activations, and after a restore.
--
-- The partial unique index on `key` is the one that matters at runtime: two rows for
-- `corporate` would make the renderer's lookup depend on insertion order, and a gallery that
-- lists a theme twice is a gallery nobody trusts. `removed_at` is a soft delete so a removed
-- upload keeps its row (and its manifest) for an audit trail without holding the key.

create table themes (
    id              uuid        primary key default gen_random_uuid(),
    -- null = bundled: a theme the platform ships belongs to no organization, and giving it
    -- one would make uninstalling it impossible to reason about on a multi-tenant platform.
    organization_id uuid        references organizations (id) on delete cascade,
    key             text        not null,
    name            text        not null,
    version         text        not null,
    source          text        not null,
    manifest        jsonb       not null,
    storage_key     text,
    checksum        text,
    installed_by    uuid        references users (id) on delete set null,
    installed_at    timestamptz,
    removed_at      timestamptz,
    constraint themes_source_check check (source in ('bundled', 'uploaded')),
    -- An uploaded theme without its package cannot be re-installed from this table, so the
    -- table would claim to hold a theme that exists nowhere else.
    constraint themes_upload_storage_check check (
        source = 'bundled' or storage_key is not null
    ),
    constraint themes_key_not_blank check (length(btrim(key)) > 0),
    constraint themes_name_not_blank check (length(btrim(name)) > 0)
);

-- One live row per key. A removed upload frees its key so the same key can be installed again.
create unique index themes_key_live_idx on themes (key) where removed_at is null;
create index themes_organization_id_idx on themes (organization_id);
create index themes_source_idx on themes (source);

-- Which theme a site renders with, and the one it can go back to.
--
-- `site_id` is the primary key rather than a unique constraint on (site_id, theme_key): a site
-- has exactly one active theme, and a second row would make the renderer's read a choice.
create table site_themes (
    site_id             uuid        primary key references sites (id) on delete cascade,
    theme_key           text        not null,
    -- Written by the activation that displaced it. Null until the first activation.
    previous_theme_key  text,
    activated_by        uuid        references users (id) on delete set null,
    activated_at        timestamptz not null default now(),
    constraint site_themes_key_not_blank check (length(btrim(theme_key)) > 0)
);

create index site_themes_theme_key_idx on site_themes (theme_key);

-- No event catalogue rows here: that registry is seeded from `crates/events/src/catalogue.rs`
-- and not from SQL, so an `insert` into a table of that name would only be a new table nobody
-- reads. `themes.theme.activated` and `themes.theme.rolled_back` are declared THERE, and the
-- catalogue's drift test fails if an emitter uses a name it does not carry.
