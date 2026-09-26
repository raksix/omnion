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

## Implementation spec

### Scope (in / out)

**In**
- One engine module (`crates/content/src/diff/`) with four public computations, each a pure function over already-loaded values: text diff, block diff, asset diff, snapshot diff. Loading, permissions and audit stay with the callers.
- Text diff: line granularity and word granularity, in one pass over the same input pair. Change grouping merges adjacent changed lines into hunks with a context width (default 3 lines) and returns per-hunk stats (added, removed, changed). Word diff tokenises on whitespace plus punctuation for spaced languages and falls back to character runs for scripts without word separators, so mixed-language bodies do not degrade to a whole-line rewrite.
- Whitespace and case options per call: `ignore_whitespace`, `ignore_case`, `ignore_trailing_newline`. Defaults are off so a comparison never lies about what was saved.
- Block diff over an ordered block list (each block: stable `block_id`, kind, payload text or reference). Operations: `added`, `removed`, `moved`, `updated`, `unchanged`. A moved block is reported as a move with an optional inner `updated` payload — never as an add plus a remove, which is the failure mode that makes a block history unreadable. Per-block summary is a one-line description ("heading text changed", "image replaced", "2 rows added to table").
- Asset diff: given two media references, report replacement (different media id), metadata change (alt, legend, caption) and focal-point change (x/y with a tolerance below which the change is labelled negligible, default 0.01), plus a side-by-side descriptor (each side: media id, width, height, mime kind, a derivative URL for a thumbnail) so the UI renders two real images. No pixel comparison and no perceptual hash in this REQ.
- Snapshot diff: two arbitrary JSON object snapshots plus a shape descriptor (from the content type definitions when available, or inferred), producing a change set. Supported value shapes: scalar, text, rich text, number, date, boolean, media reference, relation list, JSON subtree. Relation lists diff by target id with moves detected by id; JSON subtrees diff recursively down to a depth limit (default 6) and then report the whole subtree as changed.
- Change set format `changeset@1`: a versioned, deterministic JSON document — a stable, sorted list of entries `{path, kind, before, after, summary, meta}` with `paths` in document order rather than alphabetical order of keys, so two snapshots of the same page always serialise byte-identically. Callers can request `narrative_only` (summaries without values) for audit and notification surfaces.
- Reuse contract: the revision history screen, the AI action preview and the audit log all consume the same change set shape; none of them implements its own comparison.
- Bounds are explicit and reported, never silent: maximum inline text size per field (default 512 KB) with an over-limit marker and a stats-only result, maximum entries per change set (default 5000) with a truncation flag, maximum total compute time with a partial result and an `incomplete` flag.

**Out**
- Three-way merge, conflict resolution and auto-merging of concurrent edits.
- Binary or pixel-level image comparison, OCR, perceptual hashing, video or audio diff.
- Patch application: the engine reports differences; applying a change is a separate, permissioned write path (revisions, restore, AI actions).
- Rendering: highlighting, side-by-side layout, scrolling and collapse behaviour are the caller's concern; this REQ ships the data and a thin presentation component, not full screens.
- Semantic or embedding-based similarity ("these paragraphs mean the same") — a change is textual, structural or resource-level here.

### Screens (UI)

No standalone screens. The engine ships shared components used inside existing surfaces: nothing gets its own route, which is the point of the REQ.

| Consumer | Surface | What it renders |
|---|---|---|
| Revision history (REQ-077) | `/pages/<id>` → Revisions, revision comparison drawer | Text and block hunks, per-hunk change counts, `Show unchanged` toggle |
| AI action preview (REQ-101) | AI action review drawer | Field-level change set with per-field summaries and accept/reject affordances owned by REQ-101 |
| Audit log (REQ-039) | Audit detail drawer | `narrative_only` change set — summaries without values |
| Editorial workflow (REQ-110) | Comment anchor "view change" | Text hunks for the revision a comment sits on |
| Content types (REQ-109) | Schema change preview | Snapshot diff over field definitions |

- **Shared components** (one implementation each, no forks): `TextDiffView` (hunks, word-level sub-highlight, context expander, per-hunk counts), `BlockDiffView` (stacked list of added/removed/moved/updated blocks with the block's own rendered preview or a fallback chip, move arrows with from/to positions), `AssetDiffView` (side-by-side thumbnails, focal-point crosshair per side, metadata rows showing before → after, a `changed` badge per row), and `ChangeSetTable` (path, kind badge, summary, before → after with a value renderer per shape, `narrative_only` mode hiding values).
- **States.** Identical inputs show an explicit "No changes" state, not an empty box. Over-limit and truncated results show a banner naming the limit and a `Download full change set` action where the caller permits it. `incomplete` renders a warning strip with `Recompute` offered to the caller.
- **Accessibility.** Word highlights are `<ins>`/`<del>` with `aria-label`s rather than colour alone; move arrows carry text ("moved from position 2 to 5"); the change set table is keyboard navigable and announces counts.
- **Mobile.** Text hunks wrap with a line-number gutter that collapses; block and asset diffs switch to stacked before/after cards; the change set table becomes a card list.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/diff/text` | Compare two text bodies (granularity, context, ignore options) | `content.diff.read` |
| POST | `/api/v1/diff/blocks` | Compare two block lists into a block change set | `content.diff.read` |
| POST | `/api/v1/diff/assets` | Compare two media references (replacement, metadata, focal point) | `content.diff.read` |
| POST | `/api/v1/diff/snapshots` | Compare two JSON snapshots against an optional shape descriptor | `content.diff.read` |
| GET | `/api/v1/diff/schema` | The `changeset@1` document schema and the active engine limits | `content.diff.read` |
| GET | `/api/v1/content/revisions/{id}/diff/{other_id}` | Server-side revision comparison (loads both revisions, returns a change set) | item read |
| GET | `/api/v1/content/revisions/{id}/diff/published` | Compare a revision with the published revision of the same item | item read |

Ad-hoc routes accept payloads up to a documented size cap and are rate-limited per caller; they never accept ids of objects the caller cannot read, and the revision routes derive permission from the item rather than trusting the request. Responses carry `changeset_version`, `complete`, `truncated` and `limits_applied` so callers never infer correctness from an empty diff.

### Data model

No new tables and no migration in this REQ: every computation is derived from values the caller already holds, and the change set is a response body rather than stored state. Consumers keep storing their own artefacts (revisions in `page_revisions`, AI actions in their own table, audit rows in the audit store); the engine does not become a second history.

The one deliberate omission is a cache. If profiling later justifies memoising revision pairs, it takes the next free migration number (append to the ledger in that change) and stores `(left_hash, right_hash, changeset_version, change_set jsonb, computed_at)` with a bounded retention — and nothing else. Two rules for any such addition: the cache must be keyed on content hashes so a stale row can never be served, and a cache miss must fall back to compute rather than error.

Compatibility rules for the shape descriptor: unknown field kinds diff as opaque JSON (report changed, do not recurse), an absent descriptor falls back to inference, and `changeset@1` gains fields — never loses or renames them — because three screens and the audit trail read it. Requests and responses for the ad-hoc routes are also part of that contract: a caller may send shapes the engine does not know, and it must receive a valid change set rather than an error.

### Events

None. The engine is read-only and side-effect-free, so it emits no domain events and writes no audit rows itself; a diff computed for a pair the caller is not allowed to read fails before any work happens.

Consumers keep their own event contracts: the revision comparison counts as part of the read of an item (no event), while REQ-101 emits its own preview events and REQ-039 records access according to its own policy. If an audit-visibility need later appears for "who compared which revisions", it belongs to REQ-039's event, keyed on ids the caller already has — it is explicitly not added here.

### Acceptance criteria

- [ ] A line diff of two texts returns hunks with correct added/removed counts, and a single-line change produces one hunk with three context lines on each side.
- [ ] Word granularity marks only the changed words inside a changed line, including punctuation changes, and a mixed-language body (spaced script plus a non-spaced script) still produces sub-line highlights rather than whole-line replacements.
- [ ] `ignore_whitespace` treats a re-indented body as unchanged while the default comparison reports it as changed.
- [ ] A block moved from position 2 to position 5 is reported as one `moved` entry with both positions, and not as an add plus a remove; a move that also edits text reports the move with an inner `updated` payload.
- [ ] A block list with an added heading, a removed image and an updated table renders one entry per change with a per-block summary line, and `unchanged` blocks are omitted from the change set by default.
- [ ] Replacing an image reports the replacement, the media ids, the thumbnail descriptors for both sides, and the before/after dimensions; changing only `alt` reports metadata with no replacement.
- [ ] A focal-point shift from 0.40/0.50 to 0.44/0.50 is inside tolerance and reported as negligible; 0.40/0.50 to 0.52/0.50 is reported as a change with both coordinates.
- [ ] A snapshot comparison over a page's fields lists exactly the changed fields with `before`/`after` and handles a relation list where one target was removed and one added, reporting both with target ids.
- [ ] Identical inputs return an empty, complete change set and the UI's "No changes" state; the response is byte-identical across two runs over the same input (deterministic ordering).
- [ ] A 1 MB text body computes within the documented budget (p95 under 250 ms for line granularity on the reference machine) and an over-limit body returns the stats-only result with the over-limit marker.
- [ ] A result exceeding 5000 entries sets `truncated` and the UI banner names the limit; `narrative_only` returns summaries with no before/after values.
- [ ] A caller without read access to the item gets `403` from the revision diff route, and the ad-hoc routes return `403` without `content.diff.read`.
- [ ] Non-ASCII content (combining characters, emoji sequences, Turkish casing pairs) round-trips without splitting a grapheme cluster into two change spans.
- [ ] Text values containing markup render escaped in `TextDiffView`; no payload renders as live HTML.
- [ ] The same change set shape drives the revision drawer and the audit detail drawer — two screens, one renderer, verified by comparing the same pair in both places.
- [ ] The walkthrough reports zero high findings, and the diff surfaces render at 390 px without horizontal scroll.

### QA plan

Seed one page with a rich body and a block layout, publish it, then edit in five ways: change one word, reorder two blocks, replace an image and change its focal point, clear a field, and add a relation. Open `/pages/<id>` → Revisions and compare each pair, checking counts, move arrows, side-by-side assets and the change set table. Then run the same comparison through `/api/v1/content/revisions/{id}/diff/{other_id}` with a panel token and a second token lacking item read, and confirm the `403`. Ops checks: time a 1 MB diff request, post an over-limit body and confirm the stats-only marker, request `narrative_only` and confirm values are absent, and run a non-ASCII pair (Turkish casing, an emoji sequence) to confirm graphemes stay intact. Visual check: hunks show inline highlights, move entries show arrows with position text, asset pairs show both versions with crosshairs, and the truncated banner names the limit.

### Slices

1. **Text and snapshots.** The engine module with text diff (both granularities, ignore options, hunks and counts) and snapshot diff with the `changeset@1` document, plus unit tests for determinism, unicode and bounds; wire `TextDiffView` and `ChangeSetTable` into the revision drawer and the audit detail drawer. *Done when:* acceptance 1–3, 8–13 and 15 pass and the two drawers render real comparisons in the walkthrough.
2. **Blocks and assets.** Block diff with move detection and per-block summaries, asset diff with replacement, metadata and focal-point tolerance, `BlockDiffView` and `AssetDiffView` in the revision drawer; revision routes for server-side comparison and the published-revision comparison. *Done when:* acceptance 4–7 and 16 pass and the revision drawer shows block and asset changes for the seeded page.
3. **Consumer rollout.** AI action preview (REQ-101) binds the change set to field-level accept/reject; workflow comment anchors open text hunks; the schema preview in REQ-109 uses snapshot diff; limits, rate limits and the `403` path are exercised in the QA environment. *Done when:* acceptance 14 passes, all listed consumers use the shared shapes with no local reimplementation, and the QA report lists zero high findings for the wave.

### Risks / notes

- Do not let consumers fork the format. The moment the AI preview builds its own change object the two drift and the audit trail stops matching what the user saw; the `changeset@1` schema is served by the API so compliance is checkable.
- Move detection is heuristic. Match by `block_id` first and fall back to content similarity with a conservative threshold; a false move is worse than an honest add plus remove, so the fallback must prefer honest output.
- Bounds must be visible. Silent truncation reads as "nothing else changed" and is a data-integrity lie; every limit sets a flag and the UI must surface it.
- Determinism is a contract, not a nicety: sorted, stable output lets caches key on hashes and lets tests compare documents; any iteration over a map without an explicit sort is a bug in review.
- The engine is pure and synchronous; callers that need it on large payloads do it off the request path. No hidden threads, no global state, so the compute stays testable and cancels cleanly.
- Unicode correctness is the subtle one: segment on grapheme clusters, normalise consistently before comparing, and never slice by byte index. Non-ASCII test fixtures live in the unit tests, not in seeded demo content.
- Text values are untrusted input: escape on render, cap the size before tokenising, and never feed raw markup into a highlight path that could reassemble a tag boundary.
- No cache until measured. A revision-pair cache keyed on anything other than content hashes is how a comparison view starts showing yesterday's diff.
