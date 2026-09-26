# REQ-076 — Theme Builder UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + themes
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

No-code arrangement of a site's chrome.

- Arrange header, homepage and footer layouts by dragging slots and sections.
- Slot library: logo, navigation, search, CTA, social, language switcher.
- Section library: hero, features, testimonials, pricing, CTA, footer columns.
- Live preview with device toggles; save as draft, publish, revert.
- Per-site variants and inheritance from the theme's defaults.

## Implementation spec

### Scope (in / out)

**In**
- The builder screen itself: a three-pane editor (slot rail · canvas · inspector) that arranges a site's chrome — header, footer, home, page, blog list and the theme's other declared layout slots — from library sections and slot elements, with the slot rail showing each slot's state (`Theme default`, `Customised`, `Empty`, `Variant active`).
- Slot element library (REQ-083 owns the definitions, this screen consumes them): logo, navigation, search, CTA button, social row, language switcher, account menu — each a block from the REQ-063 registry with a props schema, inserted into a slot's header/footer row and reorderable inside it.
- Section library (REQ-083): hero, features, testimonials, pricing, CTA band, footer columns, social, copyright, plus the theme's own shipped sections. Cards show a thumbnail, name, category, block count and required content fields; inserting a section places it in the current slot at the drop position.
- Drag arrangement: sections and slot elements reorder with a drop indicator; containers (columns, rows, grids) accept nesting to the depth the theme declares (three levels); slot-level drop zones carry a "Drop a section here" placeholder.
- Live preview: an iframe of the renderer's preview mode (site + theme + slot + draft version), never a hand-built mock, with device toggles (desktop 1440 / tablet 1024 / phone 390 plus a custom width), light/dark/system mode, and a slot switcher so previewing the footer does not require scrolling the whole page.
- Draft → publish → revert: saves write a draft version, `Publish` promotes it, `Revert to theme default` clears the site's override for that slot, and the slot's version history lists every revision with a diff (REQ-111) and a `Restore` action that appends a new version instead of deleting history.
- Variants and inheritance: a slot may carry several site variants (`Default`, `Centered`, `Minimal`, or whatever the theme's manifest declares); exactly one variant is active per slot; the inheritance chain `site published → site draft → theme default variant → theme default` is shown explicitly in the inspector, and every inherited value can be overridden individually with a per-field `Reset to theme default`.
- Reuse of the REQ-063 editor internals (canvas, drag, inspector, validation, undo/redo) and of REQ-062's layout endpoints, extended here with versions, publish and variant state. Route aliases: `/themes/<key>/builder?slot=<slot>` links to `/appearance/builder/<slot>`; there is one canvas implementation, not two.
- Keyboard parity with the page editor plus builder-specific shortcuts, and an accessibility pass: every section renders semantic HTML, images require alt text, and a contrast-relevant warning appears when a section's colour props fight the active tokens.

**Out**
- Authoring *new* section types or slot elements — REQ-083's library; the builder inserts what the registry offers and shows an "unknown section" placeholder with a warning for anything missing.
- Editing a section's inner content outside the props schema (free-form CSS, custom scripts, raw HTML in slots) — raw HTML stays a page-builder block and is refused in header/footer slots.
- Theme settings (tokens, typography, branding) — REQ-062's Customize screen; the builder reads the published tokens and renders with them, it never edits them.
- Publishing content pages, blog posts or products — those are content revisions (REQ-063/REQ-064); the builder only arranges chrome and the layouts the theme declares.
- Package export/import and installation — REQ-062 and REQ-044.

### Screens (UI)

| Route | Screen |
|---|---|
| `/appearance/builder` | Slot rail — every slot with state, variant count, last published |
| `/appearance/builder/<slot>` | Builder for one slot: section library, canvas, inspector |
| `/appearance/builder/<slot>/preview` | Full-width live preview with device, mode and variant toggles |
| `/appearance/builder/<slot>/history` | Layout versions with diffs and restore |
| `/appearance/builder/<slot>/variants` | Variant list for the slot: create, duplicate, rename, activate, delete |
| `/themes/<key>/builder?slot=<slot>` | Alias that opens the routes above with the theme preselected |

- **Layout.** Left rail: slot list with a state badge and a per-slot variant chip, plus a section outline of the current slot (click to select, drag to reorder). Centre: the canvas with a breadcrumb for nested selection, a viewport switcher, and a zoom control (50–150 %) that never changes the underlying breakpoints. Right: inspector with sections — `Content` (`propsSchema`-generated fields), `Layout` (column count, gap step, alignment), `Visibility` (hide on mobile/desktop, order within the slot), `Inheritance` (the chain, with `Overridden here` / `From theme` badges and `Reset to theme default`), `Advanced` (id, class, ARIA label). Bottom bar: section count, validation summary ("1 section needs attention"), last saved time, `Save draft`, `Preview`, `Publish`, `Revert to theme default`.
- **Section library.** A right-side drawer, searchable, grouped by category (Hero, Content, Social proof, Pricing, Conversion, Footer), each card with a thumbnail, block count and a `Content required` chip when the section needs props. `+ Add section` opens it at the drop position; double-click inserts at the end of the slot.
- **Slot elements.** The header/footer rows expose an element strip (`+ Logo`, `+ Navigation`, `+ Search`, `+ CTA`, `+ Social`, `+ Language switcher`) constrained by what the slot's layout allows; a second language switcher in the same row is rejected with `slot_element_unique`. `Navigation` requires a source (a REQ-064 menu or the site's page tree) and links to the menus screen when no menu exists.
- **Variants.** The variant list shows name, source (`Theme` / `Site`), active state and the slot-level diff summary versus the theme default; `Duplicate from default` seeds a new site variant. Activating a variant re-renders the preview immediately and marks the slot `Variant active` in the rail. Deleting a variant in use requires picking a replacement first.
- **History.** Version rows: revision number, state (`Draft` / `Published` / `Archived`), author, time, size (block count), summary, and `Compare` · `Restore`. Restore is confirmed with a note ("Restored header from 24 Sep") and appends a new draft; publishing it makes it live.
- **States.** Empty slot: the canvas shows the library prompt plus `Start from the theme default`. Empty site: preview offers sample content. Loading: skeleton canvas and skeleton rail. Error: inline banner with the failing call and `Retry`, keeping local edits. Validation: required content fields mark the section and block `Publish` (a draft still saves). Permission-limited: without `themes.customize` the builder opens read-only with a `Preview only` chip.
- **Keyboard.** `⌘S` save draft, `⌘⇧P` publish, `⌘Z` / `⌘⇧Z` undo/redo (≥50 steps), `⌘D` duplicate section, `⌘⌥↑/↓` move section, `Backspace` deletes the selected section after a confirm when it has children, `V` cycles variants, `⌘/` shortcut sheet, `Esc` clears selection or closes the drawer.
- **Mobile.** Below 900 px the builder is a slot rail plus a read-only canvas with device toggles, a note that arranging needs a wider screen, and a working preview; save/publish are hidden rather than presented as broken controls. The full-width preview route stays fully usable on a phone.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/sites/{site_id}/theme-layouts` | Every slot with state, active variant, last published version | `themes.read` |
| GET · PUT | `/api/v1/sites/{site_id}/theme-layouts/{slot}` | Read · save the slot's draft blocks (REQ-062 route, now version-aware) | `themes.read` · `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/publish` | Publish the draft version | `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/validate` | Dry-validate a block tree (unknown types, missing content, duplicate slot elements) | `themes.read` |
| GET | `/api/v1/sites/{site_id}/theme-layouts/{slot}/versions` | Version list for the slot and variant | `themes.read` |
| GET | `/api/v1/sites/{site_id}/theme-layouts/{slot}/versions/{no}` | One version, with the REQ-111 diff against its predecessor on request | `themes.read` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/versions/{no}/restore` | Restore a version (appends a new draft, never rewrites history) | `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/reset` | Clear the site override, back to the theme default | `themes.customize` |
| GET · POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/variants` | List · create or duplicate a slot variant | `themes.read` · `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/variants/{variant}/activate` | Make a variant active for the slot | `themes.customize` |
| PATCH · DELETE | `/api/v1/sites/{site_id}/theme-layouts/{slot}/variants/{variant}` | Rename · delete (409 while active) | `themes.customize` |
| GET | `/api/v1/sites/{site_id}/layout-preview` | Signed, short-lived preview payload for the frame (slot, variant, draft version) | `themes.read` |
| GET | `/api/v1/theme-sections` | Section catalogue with props schemas and thumbnails — REQ-083 route | `themes.read` |
| GET | `/api/v1/theme-slot-elements` | Slot element catalogue with allowed slots and uniqueness rules | `themes.read` |
| GET | `/api/v1/content/menus` | Menu list for the navigation element's source picker — REQ-064 route | `content.menus.read` |

Validation error codes: `slot_unknown`, `section_unknown`, `section_prop_required`, `slot_element_not_allowed`, `slot_element_unique`, `raw_html_denied_in_chrome`, `nav_source_missing`, `nesting_too_deep`, `variant_name_taken`, `variant_active_delete`.

### Data model

Migration: `0117_theme_layout_variants.sql` (reserved band 0116–0127; append-only ledger — take the next free number if taken).

```sql
-- theme_layouts (REQ-062) gains a variant dimension and a draft pointer
alter table theme_layouts add column variant_key text not null default 'default';
alter table theme_layouts add column draft_version_id uuid null;   -- fk theme_layout_versions (id)
alter table theme_layouts drop constraint theme_layouts_site_theme_slot_key;
alter table theme_layouts add constraint theme_layouts_site_theme_slot_variant_key
  unique (site_id, theme_key, slot, variant_key);

theme_layout_versions (
  id uuid primary key default gen_random_uuid(),
  site_id uuid not null references sites (id) on delete cascade,
  theme_key text not null, slot text not null, variant_key text not null default 'default',
  revision_no integer not null, state text not null default 'draft',
  blocks jsonb not null default '[]' check (jsonb_typeof(blocks) = 'array'),
  summary text null, restored_from_id uuid null references theme_layout_versions (id) on delete set null,
  created_by uuid null references users (id) on delete set null,
  created_at timestamptz not null default now(), published_at timestamptz null,
  constraint theme_layout_versions_state_check check (state in ('draft','published','archived')),
  constraint theme_layout_versions_number_positive check (revision_no >= 1),
  constraint theme_layout_versions_key unique (site_id, theme_key, slot, variant_key, revision_no)
);
create index theme_layout_versions_slot_idx on theme_layout_versions (site_id, slot, created_at desc);

theme_slot_state (
  site_id uuid not null references sites (id) on delete cascade,
  theme_key text not null, slot text not null,
  active_variant_key text not null default 'default',
  updated_by uuid null references users (id) on delete set null,
  updated_at timestamptz not null default now(),
  primary key (site_id, theme_key, slot)
);
```

The renderer keeps reading `theme_layouts` only (the published fast path, with `draft_version_id` for the editor); `theme_layout_versions` is the append-only history and the source the diff and restore paths read. `theme_layouts.variant_key` rows are created lazily per variant, so a site with no customisation keeps zero rows and inherits the theme default by absence. Section and slot-element definitions are code (REQ-083 registry plus the REQ-063 block registry, mirrored into `content_blocks`-style references on read), never rows in these tables. New permission key: `themes.customize` already covers every write here; `themes.publish` is intentionally *not* introduced — publishing a layout is the same capability as publishing its content in this wave, and splitting it would create a permission nobody can reason about.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `themes.layout.saved` | Draft version saved for a slot and variant | `site_id`, `slot`, `variant_key`, `revision_no`, `block_count` |
| `themes.layout.published` | A draft version is published (renderer input changed) | `site_id`, `slot`, `variant_key`, `revision_no` |
| `themes.layout.reverted` | Site override cleared (back to theme default) | `site_id`, `slot`, `variant_key` |
| `themes.layout.variant_changed` | The active variant for a slot changes | `site_id`, `slot`, `from_variant`, `to_variant` |
| `themes.layout.validation_failed` | Save or publish rejected | `site_id`, `slot`, `issues[]` |

Consumed: `themes.theme.activated` (slot set and theme defaults change — reload the rail, mark slots whose theme default moved, warn before overwriting an overridden slot), `content.menu.updated` and `content.page.published` (navigation and page-list elements re-resolve in the preview), `media.deleted` (mark a section's image missing instead of rendering a broken URL), `themes.settings.published` (re-render preview with the new tokens). Webhook relevance: `themes.layout.published` is the one an edge cache or static-site integration needs; payloads carry keys and the revision number, never blocks.

### Acceptance criteria

- [ ] The slot rail lists every slot the active theme declares with a state (`Theme default` / `Customised` / `Empty` / `Variant active`), and a slot with no override shows the theme's content in the canvas without creating a row.
- [ ] Dragging a section from the library into the header, home and footer slots places it at the drop position, and the order survives a reload and publish.
- [ ] Slot elements (logo, navigation, search, CTA, social, language switcher, account menu) insert, reorder and delete; a second language switcher in one row is rejected with `slot_element_unique`.
- [ ] `Navigation` without a source is refused at publish time with `nav_source_missing`, and picking a menu clears the error.
- [ ] The preview frame renders the real site through the renderer with the published tokens; switching device and mode changes the rendered output, and the frame never shows the panel's styles.
- [ ] `Save draft` writes a version, `Publish` makes it live for a signed-out visitor, and `Revert to theme default` removes the override and restores the shipped layout.
- [ ] Version history lists every save with author, time and block count; `Restore` on an older version appends a new draft (the older versions stay listed) and the confirmation carries the note the operator typed.
- [ ] A slot supports at least three variants, exactly one active; activating another variant changes the public render within one refresh.
- [ ] Deleting an active variant returns `409`; deleting an unused one works and leaves the theme default intact.
- [ ] The inspector shows the inheritance chain with `Overridden here` and `From theme` badges, and per-field `Reset to theme default` restores a single inherited value without touching siblings.
- [ ] `Enter` in an empty required content field marks the section and blocks publish; the draft still saves and the validation summary names the field.
- [ ] Raw HTML in a header or footer slot is refused with `raw_html_denied_in_chrome`, while the same block stays available in the page builder.
- [ ] Undo/redo covers at least 50 changes including nesting, and `⌘Z` immediately after a save restores the pre-save draft state.
- [ ] A caller without `themes.customize` sees the builder read-only with the preview working and no save controls.
- [ ] The layout of a page that embeds the same section type twice round-trips through the API without prop loss, and the stored payload keeps stable section ids across reorders.
- [ ] `themes.layout.published` and `themes.layout.variant_changed` reach a subscribed endpoint with the documented payloads.
- [ ] The builder is usable at 1440 px and degrades to the read-only notice at 390 px, and the walkthrough reports zero high findings on the new routes.

### QA plan

Add `/appearance/builder` and `/appearance/builder/<slot>` to the `routes` array in `scripts/qa/walkthrough.cjs` (with a seeded slot name), and reach the builder from the widgets screen's `Open in builder`. The walkthrough must: open the header slot, insert a logo, a navigation element (with a seeded menu) and a CTA; insert a hero and a features section into the home slot; drag a section above another and duplicate one; edit a required content field to empty (expect the field message and the publish block), then fill it; save the draft; switch the preview to phone and dark mode; publish; open the history, compare two versions, restore an older one and re-publish; create a `Centered` variant, activate it, then delete it after activating the default again; and finally revert the footer slot to the theme default. Visual check: the canvas shows real sections with the theme's typography and spacing, the library drawer shows distinct thumbnails rather than identical placeholders, the inspector matches the selected section, the inheritance badges are readable, the preview frame matches the public site after publish, and no raw JSON or debug panel appears anywhere. The public site is opened after publishing to confirm the header, home and footer changes, then again after the revert to confirm the theme default returns.

### Slices

1. **Versioned layouts and the canvas.** Migration `0117_theme_layout_variants.sql`, `theme_layout_versions` + `theme_slot_state`, draft/save/publish/reset endpoints, the builder shell (rail, canvas, inspector) on the REQ-063 editor internals, section library insert/drag/reorder/delete, required-content validation, and the walkthrough routes. *Done when:* acceptance 1–2, 6, 11, 13, 15, 17 pass and `/appearance/builder` is in the walkthrough inventory.
2. **Preview, variants and inheritance.** Signed preview payload and the iframe frame with device/mode toggles, variant CRUD and activation, the inheritance chain with per-field reset, revert-to-default, and the empty/loading/error states. *Done when:* acceptance 3–5, 8–10 pass.
3. **History, slot elements and rules.** Slot element library with allowed-slot and uniqueness rules, navigation source resolution, version history with REQ-111 diffs and restore-with-note, raw-HTML denial in chrome slots, permission-limited read-only mode. *Done when:* acceptance 7, 12, 14, 16 pass.
4. **Polish and mobile.** Keyboard shortcuts and the shortcut sheet, undo/redo depth, 390 px read-only behaviour, event payload verification, and the layout cache invalidation path on publish. *Done when:* acceptance 17 passes with a green walkthrough and no high findings.

### Risks / notes

- The single-canvas rule is the invariant: the page builder (REQ-063), the theme builder (REQ-062's entry) and this screen must render the same editor component. A fork here reproduces the "preview lies" class of bug, where the panel shows one thing and the renderer another.
- Inheritance is shown, never guessed: every inherited value is labelled with its source and every override is reversible field by field. Silently copying theme defaults into site rows is forbidden — it would freeze a site against future theme updates.
- Theme updates are the dangerous interaction: a new theme version can move a slot's default layout. Publishing a layout always states which theme version produced the default it replaced, and `Restore` keeps the previous version even after a theme update.
- Draft blocks and published blocks must never be confused in the renderer: only published versions are read by `apps/web`, drafts are preview-only and behind a signed payload so a signed-out visitor cannot read unpublished chrome.
- Validation cost grows with nesting; the API validates server-side against the registries and the panel mirrors the same rules. Where the two disagree, the server wins and the panel must surface the server's message verbatim.
- Unknown section or block types (a theme referencing a removed section) render as a labelled placeholder with a warning, never a blank slot or a crash.
- Nesting depth is capped at the depth the theme declares (three levels in the shipped themes) with a clear message; an unbounded tree turns the rail and the diff view into unusable output.
- Turkish sample copy belongs to section sample content, never to library labels, prop names or validation messages.
