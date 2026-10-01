-- Remember the settings a rollback displaced, so the rollback can bring them back.
--
-- `site_themes.previous_theme_key` answers "which key goes back", which is the half
-- `themes.restore_previous` could already do. It could not answer "and the settings the
-- site was published with at that moment" — and REQ-062's criterion is "brings back the
-- previous theme AND its published settings revision, confirmed by comparing the rendered
-- page". A rollback that restores only the key leaves the displaced theme's live tokens in
-- place, so the restored theme renders with another theme's colours: a complete, valid,
-- wrong page — the same class of defect as a page drawn under the wrong sheet.
--
-- Two columns, both null on every existing row, and both written by the activation:
--
--   * `previous_settings_revision_id` — the PUBLISHED settings revision that was live when
--     this activation displaced it. Null when the site had published nothing, which is a real
--     state and not a defect: there is nothing to bring back.
--   * `previous_theme_key` keeps its meaning and its nullability, unchanged.
--
-- Why a pointer and not a copy: the revision row is append-only and immutable, so the id is
-- the whole content. A copy would be a second representation that could drift from the row it
-- was copied from, which is the exact failure `theme_settings_revisions` exists to prevent.
--
-- Why the restore path reuses `restore_revision` rather than moving the published pointer
-- backwards: a restore is itself something the site did, so it gets a number, an author and a
-- place in the history. `theme_settings.restore_revision` already implements that and is
-- covered by its own walks.
--
-- The foreign key is `on delete set null`: a site that published nothing has no row to point
-- at, and a revision that is removed leaves the rollback target absent rather than refusing
-- the deletion of a revision the history screen offered.

alter table site_themes
  add column previous_settings_revision_id uuid
    references theme_settings_revisions (id) on delete set null;

comment on column site_themes.previous_settings_revision_id is
  'The published settings revision live when this activation displaced the previous theme; a rollback republishes it through theme_settings.restore_revision, which writes a new revision rather than moving the pointer backwards.';

-- No index: the column is read only as part of the single `site_themes` row the rollback
-- already selects by primary key, and the only predicate over it is `is not null` on rows a
-- writer has just touched. An index here would be written on every activation and read by
-- nothing.