-- Theme settings and slot layouts (REQ-062, slice 2).
--
-- A theme is a FILE and a site's look is a ROW, and this migration is the row. The decisions
-- below are the ones where the obvious schema produces a product that lies to the person
-- using it, so each is argued here rather than left to the code.
--
-- 1. **Revisions are append-only and a save is NOT a publish.**
--    `theme_settings_revisions` grows by `revision_no`, and `published_at` is the only thing
--    that makes a revision the one visitors see. The alternative — a single mutable
--    `theme_settings` row with an `is_published` flag — makes "Save draft" and "Publish" the
--    same write, so a half-finished colour change becomes the live site the moment somebody
--    types. It also makes the REQ's own acceptance criterion ("saving settings twice creates
--    revisions 1 and 2; restoring revision 1 ... is itself recorded as a new revision")
--    impossible to express, because a row that gets overwritten has no history to restore.
--
-- 2. **A restore is a new revision, not a flag.** `POST .../revisions/{no}/restore` inserts
--    revision N+1 carrying revision N's content and sets `restored_from_id`. Nothing is ever
--    deleted or rewritten, so "the site looks like last Tuesday" is answerable and a later
--    restore cannot silently erase the revision it restored from. The alternative (a
--    `current_revision_id` pointer moving backwards) is a history screen that shows revisions
--    in an order the site never had.
--
-- 3. **Publishing is a pointer, not a copy.** `theme_settings_published` is a one-row-per-site
--    pointer to the revision that is live. It is a table rather than a column on the revision
--    because publishing must be atomic when two administrators publish at once, and
--    `on delete restrict` is what stops a revision from being removed while it is the one
--    every visitor is seeing.
--
-- 4. **Slot layouts are keyed by (site, theme, slot), not (site, slot).** A site that
--    customises `corporate`'s header and then activates `magazine` must still own the
--    corporate layout, because the confirmation says "the current theme's settings are kept
--    and can be restored". Keying by slot alone silently hands magazine the corporate header.
--    `is_default = true` means "this row is what the theme itself ships", which is what lets
--    `Reset slot to theme default` be an UPDATE rather than a delete-and-guess.

create table theme_settings_revisions (
    id               uuid        primary key default gen_random_uuid(),
    site_id          uuid        not null references sites (id) on delete cascade,
    revision_no      integer     not null,
    theme_key        text        not null,
    -- Each of the four is a free-form object because a theme's `settingsSchema` decides the
    -- shape; the columns the platform can reason about (below) are extracted from them.
    tokens           jsonb       not null default '{}'::jsonb,
    typography       jsonb       not null default '{}'::jsonb,
    layout           jsonb       not null default '{}'::jsonb,
    branding         jsonb       not null default '{}'::jsonb,
    header_footer    jsonb       not null default '{}'::jsonb,
    default_mode     text        not null default 'system',
    created_by       uuid        references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    published_at     timestamptz,
    restored_from_id uuid        references theme_settings_revisions (id) on delete set null,
    constraint theme_settings_revision_no_positive check (revision_no > 0),
    constraint theme_settings_theme_key_not_blank check (length(btrim(theme_key)) > 0),
    constraint theme_settings_default_mode_check check (
        default_mode in ('light', 'dark', 'system')
    )
);

-- The number an author sees, so it has to be unique per site. Two concurrent saves colliding
-- on this index is caught here rather than producing two rows both numbered 3.
create unique index theme_settings_revisions_site_no_idx
    on theme_settings_revisions (site_id, revision_no);
create index theme_settings_revisions_site_created_idx
    on theme_settings_revisions (site_id, created_at desc);
-- Listing the history walks this order, and without the index every revision read is a sort.
create index theme_settings_revisions_theme_key_idx
    on theme_settings_revisions (site_id, theme_key);

-- The pointer to the live revision. One row per site, enforced by the primary key rather than
-- by a unique constraint on a nullable column (which would need a second partial index for
-- the "no published row yet" case).
create table theme_settings_published (
    site_id    uuid        primary key references sites (id) on delete cascade,
    revision_id uuid       not null references theme_settings_revisions (id) on delete restrict,
    published_by uuid      references users (id) on delete set null,
    published_at timestamptz not null default now()
);

-- A draft that was never published still needs a name, and "the newest revision" is not it:
-- two drafts and no published revision is a state the screen must be able to say out loud.
create table theme_settings_draft (
    site_id     uuid        primary key references sites (id) on delete cascade,
    revision_id uuid        not null references theme_settings_revisions (id) on delete cascade,
    updated_by  uuid        references users (id) on delete set null,
    updated_at  timestamptz not null default now()
);

create table theme_layouts (
    id         uuid        primary key default gen_random_uuid(),
    site_id    uuid        not null references sites (id) on delete cascade,
    theme_key  text        not null,
    slot       text        not null,
    blocks     jsonb       not null default '[]'::jsonb,
    -- true when the row is what the theme ships rather than what the site customised.
    is_default boolean     not null default false,
    updated_by uuid        references users (id) on delete set null,
    updated_at timestamptz not null default now(),
    constraint theme_layouts_slot_not_blank check (length(btrim(slot)) > 0),
    constraint theme_layouts_theme_key_not_blank check (length(btrim(theme_key)) > 0)
);

-- The (site, theme, slot) triple is the identity. See note 4 for why the theme is in it.
create unique index theme_layouts_site_theme_slot_idx
    on theme_layouts (site_id, theme_key, slot);
-- No event catalogue rows here. That registry is seeded from `crates/events/src/catalogue.rs`
-- and not from SQL, so `themes.settings.published` is declared THERE, and the catalogue's
-- drift test fails if this migration's emitter uses a name it does not carry.
