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

## Implementation spec

### Scope (in / out)

**In**
- One revision browser component, used by four entry points: a page's own history (`/pages/<id>/revisions`), a cross-resource browser (`/revisions`), a per-resource deep link (`/revisions/<resource_type>/<id>`), and the configuration versions screen (REQ-112). One list, one compare view, one restore dialog — parameterised by a resource adapter.
- Resource adapters (code, not rows): `page` over the existing `page_revisions` table, `theme_settings` over REQ-062's settings revisions, `theme_layout` over REQ-076's layout versions, `config_version` over REQ-112's version store, and later `document` (REQ-058) and `automation_rule`. Each adapter declares its read key, its restore key, its renderer for the row preview and its diff strategy, so adding a resource never touches the UI.
- Revision list: revision number, state (`Draft` / `Published` / `Archived`), author, timestamp (installation timezone, exact on hover), size (`1.4 KB · 12 blocks`), the summary line, and a `Published` marker on the revision that fed the live site. Draft revisions are labelled as such and never confused with published ones.
- Compare view with three synchronised panes for a text page (metadata · revision A · revision B) and a mode switch: inline (unified) and side-by-side, word- and line-granularity, change grouping, and a "N changes" header with next/previous navigation between hunks. Modelled on the REQ-111 diff engine — the UI never computes diffs itself.
- Block-level comparison for revisions that carry blocks (REQ-063): added / removed / changed / moved entries with per-prop detail; reorders are shown as moves rather than delete-plus-insert, using block ids.
- Asset diff for image and gallery changes: before/after thumbnails side by side, the media library link, the focal-point or crop change when one exists, and a `missing` state when the asset was deleted since.
- Field-level comparison for configuration versions: a two-column table of changed keys only, with the type, the old and new value, and a redacted marker for values the caller may not read (secrets stay masked for everyone and the row states which permission would reveal more — it never does).
- Restore: a dialog naming exactly what changes (title, body, blocks, N fields), a required note (3–120 characters) that becomes the new revision's summary, an optional `Publish immediately` checkbox that is only enabled with the resource's publish permission, and the new revision number shown in the success toast. Restoring appends a new revision, sets `restored_from_id`, and never deletes or rewrites history.
- Filters and navigation between resources: author, date range (presets plus custom), state, "changed by me", and free-text over the summary. A `Jump to revision` control accepts a revision number or a deep link. Rows carry copyable links, so a page's revision 7 has a stable URL.
- Entry from elsewhere: the audit row's revision pointer (REQ-039) and the activity detail (REQ-080) link straight to the compare or the revision; content screens offer `History` per resource; the ⌘K palette lists `Compare revisions…` and `Revision history of…` for the current page.
- Pinned comparison anchors: a user can pin up to two revisions per resource (`A` and `B`) so the compare view survives navigation; pins are per user, removable, and shown as small chips at the top of the browser.

**Out**
- A second history store: pages keep using `page_revisions`, configuration keeps using REQ-112's versions, layouts keep using REQ-076's versions. This REQ adds presentation, links and pins — never a parallel copy of the data.
- The diff algorithms themselves — REQ-111 owns text, block and asset diffing, including granularity rules and change grouping.
- Restoring a *whole site* or a whole installation (that is REQ-013's restore and REQ-112's bundle import), and reverting a deployment (REQ-024).
- Scheduling or approving publication — REQ-110 (editorial workflow) and REQ-059 (approvals) own those gates; this screen shows state and links.
- Editing content inside the compare view; a `Restore and edit` action opens the page editor instead.

### Screens (UI)

| Route | Screen |
|---|---|
| `/pages/<id>/revisions` | Revision browser for one page (extended with compare, filters, restore) |
| `/pages/<id>/revisions/compare?a=&b=&mode=` | Compare view, linkable and reloadable |
| `/revisions` | Cross-resource browser with type, author, date and text filters |
| `/revisions/<resource_type>/<id>` | Canonical deep link for any resource's history |
| `/revisions/<resource_type>/<id>/compare?a=&b=` | Compare for any resource |
| `/settings/config/revisions` | Configuration versions (REQ-112 store, same UI) |

- **Browser layout.** Left: the revision list, newest first, virtualised for long histories, each row with a state chip, author avatar with initials fallback (`Former user` when the account is gone), relative time, size, summary and a `Published` marker; a radio pair (`A` / `B`) on each row sets the comparison anchors. Right: a preview pane rendering the selected revision (page render for content, key/value list for configuration, block outline for layouts) with a sticky compare bar (`Compare A ↔ B`, `Restore this revision`, `Copy link`).
- **Compare view.** Header: the two revisions with author, time and summary, plus `Swap`, `Compare with current`, and `Compare with published`. Body: inline or side-by-side; added lines green, removed lines red, changed words highlighted within a line; a change counter with `n`/`p` navigation bound to `n` and `p`; block-level mode shows a tree of sections with per-block badges (`changed`, `moved`, `added`, `removed`) and expanding a block shows its prop diff. Asset mode shows the thumbnails with the focal-point overlay and the media library link.
- **Restore dialog.** Title names the resource and the target revision, the body lists the fields that will change with the count of changed blocks or keys, the note field is required (counter 0/120), `Publish immediately` is visible but disabled with a reason when the caller lacks the publish permission, and a destructive-change acknowledgement appears (typed `restore`) when the target revision has fewer blocks than the current one. Success lands on the resource with a toast naming the new revision number.
- **States.** Single revision: an explainer ("this page has one revision; it changes the first time you edit or restore") instead of an empty table. Loading: skeletons for list and preview. Error: banner with the failing call and `Retry`, keeping the selected anchors. Permission-limited: without the restore key the action is present but disabled with `needs content.pages.restore`; without the read key the screen is not reachable from the UI and the API returns `403`. Large histories: cursor paging plus a `Jump to revision` box. Very large diffs: server-side hunk paging with a clear "showing changes 1–50 of 320" line.
- **Keyboard and mobile.** `j`/`k` or arrow keys move through rows, `Enter` opens the preview, `c` compares the anchors, `x` sets anchor A, `shift+x` sets anchor B, `r` opens restore, `n`/`p` step through changes in the compare view, `/` focuses the summary filter, `g h` jumps to the current resource's history, `Esc` clears the selection or closes the dialog. Below 900 px the browser is a full-screen list with the preview and compare as separate full-screen routes, the compare defaults to inline mode (side-by-side becomes a swipeable pair), and the restore dialog fits without scroll.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/pages/{id}/revisions` | Page revision list with filters (`state`, `author`, `from`, `to`, `q`, cursor) | `content.pages.read` |
| GET | `/api/v1/pages/{id}/revisions/{no}` | One revision with body and blocks | `content.pages.read` |
| GET | `/api/v1/pages/{id}/revisions/compare` | Diff between two revisions (`a`, `b`, `granularity`) via the REQ-111 engine | `content.pages.read` |
| POST | `/api/v1/pages/{id}/revisions/{no}/restore` | Restore (now requires `note`, accepts `publish`) | `content.pages.restore` (+ `content.pages.publish` when `publish: true`) |
| GET | `/api/v1/revisions` | Cross-resource list: `resource_type`, `resource_id`, `author`, `q`, cursor | per-row resource read key (see below) |
| GET | `/api/v1/revisions/{resource_type}/{id}/compare` | Resource-agnostic compare, same parameters as the page route | resource read key |
| GET | `/api/v1/revisions/subjects/{resource_type}/{id}` | Header payload for a resource: title, link, current revision, revision count | resource read key |
| GET · POST | `/api/v1/revisions/pins` | List · create a comparison anchor (`resource_type`, `resource_id`, `revision_no`, `side`) | resource read key |
| DELETE | `/api/v1/revisions/pins/{id}` | Remove a pin | resource read key |
| GET | `/api/v1/config-versions/{id}` · `/{id}/compare` | Configuration version read · diff — REQ-112 routes, rendered by this UI | `config.versions.read` |
| GET | `/api/v1/audit/{id}` | Audit row with its revision pointer, for the deep link | `audit.read` |

The cross-resource endpoints do not take a single guard key: they resolve the resource adapter, evaluate that adapter's read key (`content.pages.read`, `themes.read`, `config.versions.read`, …) against the caller, and return only rows whose resource the caller may read — an installation with mixed scopes shows a partial list rather than a `403`. Restore always demands the adapter's restore key (`content.pages.restore`, `themes.customize`, `config.versions.restore`).

### Data model

Migration: `0118_revision_pins.sql` (reserved band 0116–0127; append-only ledger — take the next free number if taken).

```sql
revision_pins (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid not null references organizations (id) on delete cascade,
  user_id uuid not null references users (id) on delete cascade,
  resource_type text not null, resource_id uuid not null,
  revision_ref text not null,                     -- revision number for pages/layouts, version id for config
  side text not null default 'a',
  label text null, created_at timestamptz not null default now(),
  constraint revision_pins_side_check check (side in ('a','b')),
  constraint revision_pins_type_check check (resource_type in
    ('page','theme_settings','theme_layout','config_version','document','automation_rule')),
  constraint revision_pins_unique unique (user_id, resource_type, resource_id, side)
);
create index revision_pins_user_idx on revision_pins (user_id, created_at desc);

-- author filtering on the existing page revision table (no new storage)
create index page_revisions_author_idx on page_revisions (created_by, created_at desc);
create index page_revisions_state_idx  on page_revisions (page_id, state, created_at desc);
```

Nothing else is stored: the revision rows, their authors, timestamps, bodies, blocks, summaries and `restored_from_id` links already exist (`page_revisions` today, the theme and configuration stores through their own requests). The `restored_from_id` chain is what makes the UI able to say "restored from revision 7" on every row it produced, and the UI must surface it rather than hide the lineage. `revision_ref` is text on purpose: adapters use different identifiers (an integer sequence for pages, a uuid for configuration versions).

### Events

| Event | When | Payload sketch |
|---|---|---|
| `content.revision.restored` | A revision is restored into a new revision | `resource_type`, `resource_id`, `from_revision`, `new_revision`, `published` |
| `content.revision.pinned` · `.unpinned` | A user pins or removes a comparison anchor | `resource_type`, `resource_id`, `user_id`, `side` — audit-level only, never webhook-visible |

Consumed: `content.page.published` (mark the new `Published` revision in an open browser without a manual reload), `themes.layout.published` and `themes.settings.published` (same for layout and settings histories), `config.version.published` (REQ-112), `content.page.deleted` (close the browser with a clear "this page was deleted, history retained" state). Live refresh uses the REQ-041 realtime topic for the resource (`page:<id>:revisions`) rather than polling. Webhook relevance: `content.revision.restored` is worth delivering (it explains why published content changed without an editor action); pins are deliberately not.

### Acceptance criteria

- [ ] `/pages/<id>/revisions` lists every revision of the seeded page with number, state, author, time, size, summary and a `Published` marker on exactly the revision that feeds the live site.
- [ ] The draft revision (when one exists) is labelled `Draft` and is visually distinct from the published revision — a restore never overwrites it.
- [ ] `Compare` with two revisions renders a word- and line-granularity diff with added/removed highlighting, and the hunk counter navigation works with `n`/`p`.
- [ ] Side-by-side and inline modes show the same change set for the same pair of revisions.
- [ ] A block revision paired with a plain-text revision lists the block-level changes (added, removed, changed, moved) instead of a raw body diff, and a reorder shows as a move, not as delete-plus-insert.
- [ ] An image replacement shows both thumbnails with the media library link; deleting the underlying asset switches the panel to a `missing asset` state rather than a broken image.
- [ ] A configuration version comparison lists only changed keys with old and new values, and any secret-valued key stays masked for every role with a note naming the permission that would be required (which does not unlock it).
- [ ] `Restore` requires a note of 3–120 characters; a shorter note blocks submission with a field message.
- [ ] Restoring revision 7 creates revision N+1 with `restored_from_id` pointing at revision 7, the note as its summary, all earlier revisions still listed, and the success toast naming the new revision number.
- [ ] `Publish immediately` is disabled with `needs content.pages.publish` for a caller without that key, and a restore never publishes implicitly.
- [ ] Restoring a revision with fewer blocks than the current one asks for a typed acknowledgement; cancelling changes nothing.
- [ ] Filters (author, date range, state, changed-by-me, free text) compose, and the row count matches the filtered set after paging to the end.
- [ ] A revision link copied from the browser opens the same revision and compare state in a second session.
- [ ] Jumping from an audit row (REQ-039) and from an activity detail (REQ-080) lands on the right revision or compare pair.
- [ ] The same UI renders the theme settings history (REQ-062) and a configuration version history (REQ-112) without resource-specific code paths anywhere in the components.
- [ ] Pins survive navigation and a new session for the same user, and `Compare A ↔ B` is prefilled from them.
- [ ] A caller without the resource's read key gets `403` from the API and never sees the rows in the cross-resource list.
- [ ] The browser and compare view are usable at 390 px (inline mode default) and pass the walkthrough with zero high findings.

### QA plan

Add `{ path: "/revisions", name: "revisions" }` to the `routes` array in `scripts/qa/walkthrough.cjs`, and reach per-page history from the page list as well. The walkthrough must: seed two edits plus one restore on a page so at least four revisions exist; open `/pages/<id>/revisions`, keyboard through the list with `j`/`k`, set anchors `A` and `B`, compare, switch between inline and side-by-side and between word and line granularity, step through hunks; restore the second revision with a note, then open the browser again to confirm the new revision, the `restored_from_id` note and the unchanged published revision; copy a revision link and open it in a new tab; open `/revisions` and filter by author and date, then search a summary fragment; open the configuration history screen and compare two versions; and jump from an activity row into the revision it names. Visual check: the list shows real authors, times and sizes; the diff colours are distinguishable without relying on colour alone (added/removed markers or glyphs); the compare header names both revisions; the restore dialog states precisely what changes; the preview pane renders the revision as the public site would, not as a JSON blob; and the mobile pass shows inline mode with a legible diff and a usable full-screen restore dialog.

### Slices

1. **Shared browser and page history.** Adapter interface plus the `page` adapter, the extended revision list (filters, state chips, size, summary), the preview pane, the compare route on the REQ-111 engine with inline/side-by-side and granularity, keyboard navigation, and the walkthrough routes. *Done when:* acceptance 1–4, 12–13, 18 pass.
2. **Restore with notes and safety.** Required note and validation, `restored_from_id` lineage surfaced in the list, `Publish immediately` gating, typed acknowledgement for shrinking restores, the success toast with the new revision number, and the audit row written by the restore. *Done when:* acceptance 8–11 pass.
3. **Resource-agnostic depth.** The `theme_settings`, `theme_layout` and `config_version` adapters, block-level and asset diff surfaces for the resources that have them, masked secret values in configuration diffs, cross-resource list with per-row permission evaluation, pins, and the canonical deep links. *Done when:* acceptance 5–7, 15–17 pass.
4. **Integrations and polish.** Jump-to-revision from audit (REQ-039) and activity (REQ-080), live refresh on the resource topic, copy-link affordances, empty/loading/error states, mobile compare behaviour, and the events verified against a subscribed endpoint. *Done when:* acceptance 14, 18 pass and the walkthrough is green.

### Risks / notes

- History is append-only and this UI must never offer a way around it: no "edit a revision", no "delete a revision", no "make this the current revision without a new row". The only mutating action is restore, and it appends. A single shortcut here destroys the audit story the product sells.
- The restore dialog is the last line of defence against a silent content regression: it must name the fields and the block counts that change, not just say "are you sure", and the required note is what makes later archaeology possible.
- Cross-resource listing is a permission minefield: resource types are filtered per row by the adapter's read key, and a mixed-permission caller sees a partial list with an explanation. Widening a single query to "show everything to anyone who can read one thing" is the failure to avoid.
- Diff performance on large bodies must be handled by the engine (hunk paging, caps) rather than the browser; a client-side diff of two 200 KB bodies freezes the panel.
- The `Draft` versus `Published` distinction is a correctness matter, not a style one: the labels, the chips and the markers must all come from the same server-computed state, never from the UI guessing which row is live.
- Configuration versions frequently carry secrets-adjacent values; masking happens server-side before the response, and the note that names the required permission must not imply the value is retrievable in the UI.
- Adapters are the extension point, and the temptation is to leak resource-specific branches into components. `document` and `automation_rule` land later without a UI change — that is the test of this design.
- Timestamps render in the installation timezone with the viewer's locale, and authors that no longer exist render as `Former user` with the original display name preserved in the payload.
