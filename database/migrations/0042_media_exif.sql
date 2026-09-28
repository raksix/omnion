-- Omnion · 0042 · media: what a camera wrote (REQ-010, slice 3)
--
-- The library can already say how big a file is (0025 filled the width/height/duration columns
-- from the header) and can already say what an *editor* wrote about it (alt text, caption, tags).
-- What it could not say is what the *camera* wrote: which body took the picture, at what
-- shutter speed, with which lens, on which day. For a photograph library that is the other half
-- of the row, and an editor comparing a shoot against a catalogue asks for it first.
--
-- Four decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **The record gets its own column, not a key in `metadata`.** `media.metadata` is the
--     editor's own key/value bag and `PATCH /media/files/{id}` replaces it wholesale — a camera
--     block stored there is deleted by the first person who saves a caption, silently, and a
--     camera that reports itself twice is worse than one that reports itself never. A dedicated
--     column also makes "which files were shot on a body nobody owns" a filter rather than a scan
--     of every row's free-form keys.
--   * **It is a jsonb object with absent keys, not a wide set of nullable columns.** Most files
--     carry two or three of these fields; a screenshot carries none. Eleven mostly-null columns
--     would make every row of the library wider for the majority that use none of them, and
--     `{"iso": null}` in a jsonb column is a value somebody will eventually filter on — a
--     *missing* key is honestly "the camera did not say".
--   * **The geometry it writes is oriented, and the orientation itself is kept.** A JPEG whose
--     pixels are stored sideways (`orientation` 5–8) is a portrait photograph; browsers rotate it
--     when they render it, so a layout that reserved the stored `width × height` reserves the
--     wrong box and every image below it shifts. The columns are therefore written with the
--     quarter turn applied, and the raw value stays inside the record so a downloader that
--     applies it does not rotate twice.
--   * **No coordinates.** A GPS fix is recorded as `gps: true` and nothing else. A media library
--     that quietly files an operator's home address into a row that search, an API key and a
--     share link can all read is a leak wearing a feature's clothes; the panel says the picture
--     carries a location, and the coordinates stay in the bytes the uploader chose to send.
--
-- Create-and-backfill: no existing column is altered and no row is rewritten, so the migration
-- runs against a live library without a lock-heavy pass, and an old file simply has no record
-- (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- The camera record
-- ---------------------------------------------------------------------------------------------

alter table media add column exif jsonb;

comment on column media.exif is
    'What the file''s own EXIF block said: make, model, lens, iso, exposure_ms, aperture_x100, '
    'focal_length_mm, orientation, captured_at, software and gps. A gps fix is recorded as a '
    'boolean and no coordinate is ever stored. Written from the bytes on upload, replaced '
    'wholesale by a replacement, and null for a file whose format carries no EXIF.';

-- "Which files came off that body" is a real question in a shoot, and the column is jsonb so the
-- answer is a lookup rather than a scan of a wide table.
create index media_exif_idx on media using gin (exif jsonb_path_ops)
    where exif is not null;

-- A rotation is the only field anybody sorts a library by that the geometry probe cannot answer,
-- and the reporting index covers "the files shot sideways" without reading the whole object.
create index media_orientation_idx on media (((exif ->> 'orientation')::integer))
    where exif is not null and exif ? 'orientation';
