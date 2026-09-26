# REQ-111 — Diff Engine (text, asset, block)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/content`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The machinery behind every comparison view.

- Text diff with word/line granularity and change grouping.
- Block diff: added/removed/reordered blocks with per-block change summary.
- Asset diff: image replacement with side-by-side and focal-point change.
- Field-level snapshot comparison producing a machine-readable change set.
- Reused by: revision history UI (REQ-077), AI action preview (REQ-101), audit log (REQ-039).
