# REQ-077 — Revision History UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/content`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Seeing and undoing content changes.

- Revision browser per page (author, time, summary, size) with keyboard navigation.
- Compare view: text diff, asset diff (image swap), block-level diff.
- Restore action (creates a new revision rather than deleting history) with confirmation and note.
- Filter by author/date; jump to a revision from the audit log.
- Same UI reused for configuration versions (REQ-112).
