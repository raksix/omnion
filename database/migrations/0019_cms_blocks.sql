-- Omnion · 0019 · content: the block system of the page builder
--
-- Blocks are typed JSON stored on the revision that owns them (REQ-063). The registry that
-- describes which block types exist is *code* (`crates/content/src/blocks.rs`), versioned with
-- the platform and exposed read-only over the API; this migration only adds the column the
-- payload lands in. That split is deliberate: a block type must be reviewable, typed and
-- testable in the same commit as the renderer that understands it, so it never becomes a row
-- an operator can edit into a shape nothing renders.
--
-- The default `'[]'::jsonb` is what makes this migration non-breaking for released content:
-- every revision written before the block system renders from its `body` until an editor
-- saves blocks (docs/05-VERSIONING.md §4 — history is never rewritten, so an old revision
-- keeps the shape it was written with).
--
-- The GIN index is what keeps text search over block content from degrading as a site grows:
-- without it, `jsonb_path_ops` lookups inside a growing `page_revisions` table are a scan.

alter table page_revisions
    add column blocks jsonb not null default '[]'::jsonb;

alter table page_revisions
    add constraint page_revisions_blocks_is_array
    check (jsonb_typeof(blocks) = 'array');

create index page_revisions_blocks_gin on page_revisions using gin (blocks jsonb_path_ops);
