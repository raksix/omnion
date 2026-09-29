-- Omnion · 0158 · content: a page's featured image, its alt, its legend and its focal point
-- (REQ-064, slice 4d — "media reuse").
--
-- One page has ONE featured image, and everything about that decision is a property of the page
-- rather than of the file, for the reason the REQ states: the same photograph is the hero of
-- three pages with three different crops and three different captions. So the columns live on
-- `pages` — next to `og_image_media_id`, which is the same idea one level out — and the media
-- table keeps its own `alt_text`/`caption` for the library's own screens. Editing the featured
-- alt of one page must not rename the file everywhere it is used, and the store never reads
-- `media.alt_text` to fill this in.
--
-- Four decisions, each of which is a way the obvious version fails:
--
-- 1. **`on delete set null`, and the DEGRADATION IS NOT THE DELETE.**
--    A purged file nulls the column, which is the easy case. The hard case is the *trash*: a
--    trashed file keeps its row (REQ-010 keeps the bytes until the trash is purged), so the
--    column still holds an id whose object store copy is gone. A renderer that follows the id
--    emits a dead `<img>` on every page that used it. So the read path asks whether the row is
--    still live and reports a WARNING, and the page still renders — the criterion is "leaves the
--    page renderable with a warning", not "leaves the page broken" and not "the page silently
--    loses its picture". The warning is generated on read rather than by an event consumer,
--    because a consumer that has not run yet is a page serving a dead URL, and a panel that has
--    to be told the truth after a crash would need a reconciliation job nobody asked for.
--
-- 2. **`focal_x`/`focal_y` are BOTH set or BOTH null, and the schema says so.**
--    Half a focal point is a point on the horizontal axis only, which is what a caller sending
--    `focal_y: null` and letting the other column default means. `object-position: 50%` is a
--    legitimate centre crop, and it is *not* the same thing as an unset focal point — the panel
--    must be able to say "this page has never been cropped" — so the two are distinct states
--    rather than one nullable pair. The CHECK is in the column set rather than in a writer so a
--    second writer cannot spell the rule differently.
--
-- 3. **A 0..1 fraction, not a percentage column.**
--    `focal_x` is the *fraction of the image's width the crop centres on*, which is what
--    `object-position` takes. Storing 62.5 means the renderer multiplies by a width it has to
--    fetch, and the focal point then depends on a rendering context it was not chosen in. The
--    column is `double precision` rather than the REQ's `numeric(4,3)` — see the comment on the
--    ALTER below for why, which is a driver fact and not a design change.
--
-- 4. **The alt is required WHEN THERE IS AN IMAGE, and that is a check, not a convention.**
--    An `<img>` with no `alt` is read by a screen reader as a filename, so "featured image set,
--    alt empty" is worse than no image at all. The CHECK refuses the combination rather than
--    letting the panel ship one, and the panel therefore cannot save a picture without being
--    asked what it is. This is the one constraint in this file that an owner will feel, and it
--    is the one the criterion is about.
--
-- Released migrations are append-only (docs/05-VERSIONING.md), and every field is nullable or
-- has a default, so a page that predates this file renders exactly as it did.

alter table pages
    add column featured_media_id uuid references media (id) on delete set null,
    add column featured_alt text not null default '',
    add column featured_legend text not null default '',
    -- `double precision`, NOT the REQ's `numeric(4,3)`, and the reason is the driver rather than
    -- the design: sqlx decodes a `NUMERIC` column as a decimal crate type, so a `f64` field
    -- answers `mismatched types; Rust type Option<f64> (as SQL type FLOAT8) is not compatible
    -- with SQL type NUMERIC` at the first read. The 0..1 CHECK below is what actually bounds the
    -- value, so the scale-and-precision column type only ever added a second place to state a
    -- rule the CHECK states — and paid for it with a driver mismatch. `double precision` is
    -- exact for every fraction a focal point can be.
    add column focal_x double precision,
    add column focal_y double precision;

-- **Half a focal point cannot be stored.** Both columns or neither: a caller that sends one of
-- them is a caller whose crop silently centres on the horizontal middle, and it looks correct
-- in the editor and wrong in every rendered page that crops vertically.
alter table pages
    add constraint pages_focal_pair_check check (
        (focal_x is null and focal_y is null) or (focal_x is not null and focal_y is not null)
    );

-- A fraction, not a percentage and not a pixel offset. The bound is inclusive: `1.0` is "the
-- crop centres on the right edge of the image", which is a real thing an author wants.
alter table pages
    add constraint pages_focal_range_check check (
        (focal_x is null or (focal_x >= 0 and focal_x <= 1))
        and (focal_y is null or (focal_y >= 0 and focal_y <= 1))
    );

-- A focal point on a page with no picture is a crop of nothing, and it is what a stale editor
-- leaves behind when somebody clears the image field but not the two numeric ones.
alter table pages
    add constraint pages_focal_needs_media_check check (
        focal_x is null or featured_media_id is not null
    );

-- **The alt is required with the image, and the empty string is not a way out.** `''` is the
-- default on the column so that a page with no image has nothing to answer for, so the check
-- reads "no image, or a non-blank alt".
alter table pages
    add constraint pages_featured_alt_required check (
        featured_media_id is null or length(btrim(featured_alt)) > 0
    );

-- The media table's own alt is a library fact; this pair is an editorial one. The two are
-- deliberately NOT the same column and nothing in this migration reads `media.alt_text`.
comment on column pages.featured_media_id is
    'The page''s featured image (REQ-064 slice 4d). on delete set null: a purged file empties the '
    'column, while a trashed file keeps it and is reported as unavailable with a warning, so the '
    'page still renders.';
comment on column pages.featured_alt is
    'Alt text for THIS page''s use of the image, not the file''s own alt_text — the same file is '
    'the hero of several pages with different descriptions.';
comment on column pages.featured_legend is
    'Caption drawn under the image. Optional, and never a substitute for the alt: a legend is '
    'optional text, an alt is the content of the image.';
comment on column pages.focal_x is
    'Horizontal focal point as a 0..1 fraction of the image width; rendered as object-position. '
    'Set with focal_y or not at all.';
comment on column pages.focal_y is
    'Vertical focal point as a 0..1 fraction of the image height; rendered as object-position.';

-- The panel opens the picker by "which images can this page use", and that question has to be
-- answerable without a sequential scan of every file: the featured column is sparse (most pages
-- have none) so a partial index is the whole of it.
create index pages_featured_media_idx
    on pages (featured_media_id)
    where featured_media_id is not null;
