-- Keep the theme's own slot blocks in a column a custom save never writes (REQ-062 slice 3).
--
-- WHY THIS MIGRATION EXISTS
--
-- `theme_layouts` has one row per (site, theme, slot) — that is the identity, and it is
-- right: a site has ONE header for a given theme, not two rows to choose between. The bug
-- was that the theme's shipped blocks and the site's blocks shared the single `blocks`
-- column, and `save_slot` UPSERTed over it while setting `is_default = false`.
--
-- The consequence was that the first custom edit deleted the only copy of the theme's
-- blocks. `reset_slot` read its default out of the row `where is_default`, so immediately
-- afterwards that row no longer matched and "Reset to theme default" answered 409
-- `theme_slot_no_default` on a slot that had had a default one request earlier. Reset was a
-- one-way door, and the confirmation dialog it is drawn behind is a lie in exactly the case
-- that matters.
--
-- The obvious alternatives, and why each is worse:
--
-- * A second row per slot, one `is_default` and one custom. Then "the slot" is no longer a
--   row, every read has to pick between two, and the renderer's query — which asks for the
--   site's look — becomes ambiguous the moment both exist. The identity stays; the
--   *provenance* moves.
-- * Re-read the theme's files on reset. That is what the module docs originally described,
--   and it works for a bundled theme and silently fails for an uploaded package whose files
--   live in the library rather than on disk. A reset that 409s for half the gallery is worse
--   than no reset.
--
-- So the theme's blocks move into `default_blocks`, written by `seed_default_layouts` when a
-- theme is activated and by nothing else. `save_slot` never touches it; `reset_slot` restores
-- from it. The column is nullable on purpose: a slot nobody ever seeded has no default, and
-- the honest answer for it is still the 409 — but now that 409 means "this theme ships no
-- layout for that slot", which is true, rather than "somebody edited the header".

alter table theme_layouts add column default_blocks jsonb;

-- Backfill from what each row can still be trusted to know. Only rows that are *still* the
-- theme's own (`is_default`) carry an intact copy, so they alone can seed the column. A row
-- that is already custom has lost its original and must stay NULL — backfilling it from its
-- current blocks would invent a "theme default" that the site wrote itself, which is the one
-- answer a reset must never give.
update theme_layouts set default_blocks = blocks where is_default;

comment on column theme_layouts.default_blocks is
    'What the theme ships for this slot. Written only by seed_default_layouts and read only '
    'by reset_slot, so a custom save can never take the theme default away.';
