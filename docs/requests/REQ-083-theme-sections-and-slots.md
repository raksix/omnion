# REQ-083 — Theme Sections & Slots Library

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** themes/* + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The building blocks themes and users share.

- Header slots: logo, navigation, search, CTA, language switcher, account menu.
- Sections: hero, features, testimonials, pricing, CTA bands, footer columns, social row, copyright.
- Every section is configurable (headline, subline, image, links, layout variant) without code.
- Sections compose into pages through the block system (REQ-063) and the theme builder (REQ-076).
- Accessibility and responsive rules baked into each section.

## Implementation spec

### Scope (in / out)

**In**

- `packages/sections` (`@omnion/sections`) — one registry of slots and sections with typed props schemas, variants, defaults, thumbnails, a11y notes, responsive contract and a stress fixture per entry. Themes (REQ-082) compose from it, the page builder (REQ-063) exposes the same entries as blocks, and the builder UI (REQ-076) renders the props panels from the schemas. One registry, three consumers.
- Header slots: `logo`, `primary_nav`, `search`, `cta`, `language_switcher`, `account_menu`, `announcement_bar`. Footer slots: `footer_columns`, `social_row`, `newsletter`, `legal_row`, `copyright`. Each slot declares its allowed positions (start / center / end for header, column index for footer), its maximum count where a count makes sense (one logo, one nav, one copyright), and whether the theme may hide it.
- Page sections: `hero` (split, centred, media-first, text-only), `features` (grid, split, alternating, list), `testimonials` (single, carousel, grid), `pricing` (tiers with monthly/annual toggle), `cta_band`, `logo_cloud`, `stats`, `faq` (accordion), `team`, `gallery`, `blog_list`, `product_grid`, `doc_tree`, `contact`, `steps`, `comparison_table`, `rich_text`. Each ships variants where the request names them; a section without at least one variant and one stress fixture does not land.
- Every section is configurable without code: `headline`, `subline`, `eyebrow`, `media` (media-library reference with required alt text), `links` (typed link list with label, href, style, external flag), `layout_variant`, `tone` (default, muted, contrast, accent), `spacing` (compact, normal, roomy) and section-specific fields. Validation comes from a JSON Schema per section and produces field-level errors the props panel renders inline.
- Composition: sections are stored as section keys plus props, never as markup — in page revisions (REQ-063) and in slot layouts (below). The theme builder arranges slots and sections by drag or keyboard; the page builder inserts the same section objects between blocks.
- Theme override: a theme may replace a section's renderer with its own implementation while keeping the registry key (`defineSection({ key, render, schema })` merged through the `UiProvider` override mechanism of REQ-081). The override must satisfy the same a11y and responsive rules; the harness runs the shared section fixture against overrides too.
- Accessibility baked in: each section declares its landmark role and heading behaviour, renders exactly one heading level derived from a `heading_level` prop with an automatic fallback based on position, uses tokens for every colour, reserves media aspect ratio to avoid layout shift, names its interactive elements, and passes axe with zero serious findings in light and dark at three viewports.
- Responsive contract per section: documented stacking order, breakpoint behaviour (mobile / tablet / desktop), truncation rules for long text, image crop behaviour, and whether the section hides entirely below a breakpoint (allowed only for decorative media, never for content or actions).
- Versioning: each section entry carries `props_version`; layouts store the version they were composed against. Adding an optional prop keeps the version, renaming or removing one bumps it and keeps an alias for one minor release so saved layouts keep rendering.

**Out**

- The block engine, storage of page revisions and the page builder UI (REQ-063 / REQ-077); the builder shell, drag surface and preview frame (REQ-076); the theme engine, per-site settings and activation (REQ-062).
- Theme-specific styling: a theme may restyle a section, but the registry's structure, schema and a11y contract are fixed here.
- Third-party section packs beyond the plugin contract already defined in REQ-081 / REQ-044 (marketplace sections are a follow-up, and they must ship schemas, not free-form HTML).
- Rich text inside section props beyond a reference to a `rich_text` section or block.
- Email templates: transactional mail reuses sections only when the email renderer lands; out of scope here.

### Screens (UI)

- **`/design-system/sections`** (admin, internal) — the section library: one card per section in its variants, live-rendered at the current viewport with a props editor beside it, token usage listed, a11y notes, the stress fixture toggle (one item, long headline, no media, 12 nav items) and a usage list showing which sites and pages use it.
- **`/themes/<key>/builder`** (REQ-076) consumes the registry: slot picker grouped Header / Footer / Page with badges for `theme default`, `customised` and `empty`; section cards with thumbnails and variant count; props panel generated from the schema with field-level validation; drag reordering inside a slot with keyboard alternatives (`⌥↑` / `⌥↓`); toolbar actions `Save draft`, `Publish`, `Reset slot to theme default`.
- **`/themes/<key>/customize`** (REQ-062) shows the tokens a section reads, so a colour change is traceable from section to token to theme setting.
- **States** — loading skeleton shaped like the section (not a generic bar), empty slot state with an "add section" affordance, invalid props state listing messages next to fields, missing media state rendering a placeholder with the correct aspect ratio, long-content stress state, and a "section not available in this theme" state when a theme removes an optional section (content keeps rendering with the theme's fallback rather than vanishing).
- **Keyboard** — in the builder: `Tab` through the slot list, `Enter` to open a section, `⌥↑`/`⌥↓` to reorder, `⌫` to remove with a confirm, `⌘S` save draft, `⌘⇧P` publish; page updates are announced in a polite live region ("Hero updated in Header slot") rather than the whole preview being re-announced.
- **Mobile (<1024px)** — the builder is read-only below the breakpoint with a clear note (REQ-076 owns the frame); the sections themselves define their own mobile behaviour: header condenses to a sheet or bottom bar, footer columns become an accordion, pricing tables stack into cards with the tier name sticky, media sections crop rather than scale, and no section may introduce horizontal scroll at 390px.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/section-library` | Slots and sections with variants, props schema, defaults, thumbnails, versions | `themes.read` |
| GET | `/api/v1/section-library/{key}` | One section: schema, a11y notes, stress fixture, version history, deprecations | `themes.read` |
| GET | `/api/v1/section-library/usage` | Where each section is used per site (slots and pages, published and draft) | `themes.read` |
| GET · PUT | `/api/v1/sites/{site_id}/theme-layouts/{slot}` | Read the slot's published layout and draft · save a draft (REQ-062's path) | `themes.read` · `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/publish` | Publish the draft layout for one slot | `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/reset` | Reset a slot to the theme's default arrangement | `themes.customize` |
| GET | `/api/v1/sites/{site_id}/theme-layouts/{slot}/revisions` · `/revisions/{no}` | Layout revision list · one revision as a diff against the current draft | `themes.read` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/revisions/{no}/restore` | Restore an earlier layout revision as the draft | `themes.customize` |
| POST | `/api/v1/section-library/validate` | Validate a prospective layout (unknown section, bad props, slot constraints, duplicate singletons) and return field errors | `themes.customize` |

The read and save rows are REQ-062's paths, repeated here so the two specs line up; publish, reset, revisions and validation are new. `themes.read` and `themes.customize` come from the theme surface and live in `crates/permissions/src/catalogue.rs`; every handler is guarded with `guards::require(...)`. Layout payloads are validated server-side with the same schema instance the props panel uses, and a validation failure is a 422 with field paths, never a silent drop of unknown props.

### Data model

- Migration `0018_theme_sections.sql` (next free number at merge; numbering is assigned in merge order across the theme requests):
  - `theme_slot_layouts` — `id uuid primary key default gen_random_uuid()`, `site_id uuid not null references sites (id) on delete cascade`, `slot text not null check (slot in ('header','footer','home','page','list','detail','product','doc','search','not_found','announcement'))`, `status text not null check (status in ('draft','published'))`, `blocks jsonb not null default '[]'`, `layout_version integer not null default 1 check (layout_version >= 1)`, `checksum text not null`, `updated_by uuid references users (id) on delete set null`, `updated_at timestamptz not null default now()`, unique `(site_id, slot, status)`.
  - `theme_slot_layout_revisions` — `id bigserial`, `site_id`, `slot`, `revision_no integer not null`, `blocks jsonb not null`, `layout_version integer not null`, `note text`, `created_by uuid references users (id) on delete set null`, `created_at timestamptz not null default now()`, unique `(site_id, slot, revision_no)`, index `(site_id, slot, created_at desc)`.
  - `create index theme_slot_layouts_blocks_idx on theme_slot_layouts using gin (blocks jsonb_path_ops);` and the same for `theme_slot_layout_revisions.blocks`, so the usage query can find sections by key.
  - Once REQ-063 adds the blocks column to `page_revisions`, the same GIN index is added there so page usage is answerable from one query shape.
- Each stored block is `{ "section": "<key>", "section_version": <int>, "variant": "<variant>", "props": { ... }, "id": "<uuid>" }`; the server rejects unknown keys, unknown sections, duplicate singleton slots (a second `copyright`), and props failing the schema. The `checksum` is a hash of the canonicalised blocks array and is used for optimistic concurrency (a save with a stale checksum is refused with a conflict).
- Publishing is transactional: the draft row is validated, copied to the published row, and a revision row is written in the same transaction; reset writes a revision whose blocks are the theme defaults so history stays complete.
- Theme defaults are never copied into the database: the draft starts from the theme's default arrangement, and any node is stored only when it differs. Serialised is therefore "overrides plus explicit placements", and reset simply clears the row.

### Events

- `theme.layout.updated` — a draft layout was saved (payload: site, slot, section count, actor).
- `theme.layout.published` — a draft was published (payload: site, slot, sections with versions, revision number).
- `theme.slot.reset` — a slot was reset to the theme default (payload: site, slot, removed section count).
- `theme.section.deprecated_used` — a layout referencing a deprecated section version was read by the renderer (payload: site, section key, version, deprecation window); it tells maintainers when the compatibility window can close.
- Validation failures are HTTP 422 answers, not events: refused writes are facts about a request, and putting them on the bus would flood subscribers with user typing.

### Acceptance criteria

- [ ] The registry exposes every header slot, footer slot and page section named in the request, each with a props schema, at least one variant, defaults, a thumbnail and a stress fixture.
- [ ] `/design-system/sections` renders every section in every variant, in light and dark, at 390 / 768 / 1440, with no horizontal scroll.
- [ ] Every section passes axe with zero serious or critical findings at all three viewports and both modes; overrides pass the same fixture.
- [ ] Heading behaviour is correct: each section renders one heading at the level from `heading_level` or the automatic fallback, and a page assembled from sections has no skipped levels and exactly one `h1` per page (checked by the harness).
- [ ] Props are editable without code: changing headline, subline, media, links, variant, tone and spacing in the builder updates the preview and persists after publish, across a reload.
- [ ] Validation is honest: an unknown section key, an unknown prop, a missing required prop, a duplicate singleton slot and a stale checksum each return 422 with a field path, and the panel shows the message beside the field.
- [ ] Save draft → publish → reload keeps the layout; a second draft started after a publish begins from the published state.
- [ ] Revision history shows layout revisions with a structural diff (added, removed, reordered, changed props), and restore returns the draft to that revision without publishing it.
- [ ] Reset to theme default clears the row, the slot renders the theme's default arrangement again, and the reset appears in history.
- [ ] Header slot composition works with all seven slot types together: logo, navigation, search, CTA, language switcher and account menu render and are keyboard operable, including with a long navigation list.
- [ ] Footer composition works: columns (2–4), social row and copyright render; mobile collapses columns into an accordion without losing links.
- [ ] The usage endpoint answers which sites and pages use a given section, and the answer matches the rendered pages (verified for at least one section).
- [ ] A theme overriding a section renderer changes the rendered output while the registry key, schema and stored layout stay identical, and the builder keeps editing the same props.
- [ ] Section versioning holds: a layout saved against an older section version keeps rendering after a schema bump, and a deprecated version emits `theme.section.deprecated_used` with the site and version.
- [ ] Stress fixtures render cleanly: 140-character headline, one-item grid, missing media, 12-item navigation, a 300-character subline and an empty link list.
- [ ] Mobile behaviour per section is as specified (header condenses, footer accordions, pricing stacks) and tap targets stay ≥ 44px.
- [ ] `dir="rtl"` smoke check renders header and one composed page with mirrored order and no clipped text.
- [ ] Events fire exactly once per action with the documented payloads, verified by a webhook receiver test.
- [ ] `scripts/qa/run.sh` exercises `/design-system/sections` and the builder slot flow and reports zero high findings.

### QA plan

- **Walkthrough must click:** open `/design-system/sections`, pick a section, edit headline, subline, media, one link and the variant, switch viewport, then reset; go to the builder, add `hero`, `features`, `testimonials`, `pricing` and `cta_band` to the home slot, reorder with the keyboard, save draft, publish, reload and confirm; edit the header slot to add search, CTA and language switcher; edit the footer slot with three columns, social row and copyright; reset the footer and see the theme default; submit an intentionally broken layout through the validation endpoint and read the field errors.
- **Visual check should see:** sections aligned to a shared grid with a consistent vertical rhythm, sensible heading hierarchy (one obvious page title, section headings below it), visible focus rings on all interactive elements, media that never jumps (reserved aspect ratio), graceful rendering with sparse content (one testimonial, no image), proper stacking order on mobile, and the footer accordion collapsed by default with all links reachable.

### Slices

1. **Registry, schemas, gallery** — `packages/sections` with the slot set and the core sections (`hero`, `features`, `cta_band`, `testimonials`, `footer_columns`, `copyright`, `rich_text`), props schemas, defaults, thumbnails, a11y notes and stress fixtures; `/design-system/sections` with the live props editor and the axe fixtures.
   *Done when:* every core section renders in all variants in both modes with zero serious axe findings, and the props editor round-trips edits.
2. **Slots and builder integration** — the remaining header slots (`logo`, `primary_nav`, `search`, `cta`, `language_switcher`, `account_menu`, `announcement_bar`) and footer slots (`social_row`, `newsletter`, `legal_row`); slot constraints; builder slot picker with drag and keyboard reordering; mobile behaviours per slot.
   *Done when:* a header built from all seven slot types and a footer built from four slots render, are keyboard operable, and match the mobile spec.
3. **Storage, publish, revisions, validation** — the two tables with indexes, draft/publish/reset endpoints, revision diff and restore, schema validation with field errors and checksum concurrency, usage query, and the view/override wiring that lets a theme replace a section renderer.
   *Done when:* the full save → publish → revision → restore cycle is proven by tests, and a stale save is refused with a conflict.
4. **Remaining sections and hardening** — `pricing`, `logo_cloud`, `stats`, `faq`, `team`, `gallery`, `blog_list`, `product_grid`, `doc_tree`, `contact`, `steps`, `comparison_table`; versioning and deprecation behaviour; RTL smoke; stress fixtures; walkthrough update.
   *Done when:* the full library is present, deprecation emits its event, and the walkthrough covers the gallery and builder flows with zero high findings.

### Risks / notes

- Section sprawl is the real cost: every section is maintained forever. The rule is a real consumer (a shipped theme of REQ-082, or a documented use case) plus a stress fixture before a section is added; the initial set stays close to the request.
- Schemas are the contract. A rename without a version bump breaks published layouts silently, which is the worst failure mode; the alias-and-deprecation rule exists for that reason, and the renderer must log rather than throw when it meets an unknown version.
- Never store rendered markup. Layouts keep section keys, versions and props, so a theme change or an accessibility fix applies to existing content.
- Heading levels are a content decision the tooling must not fake: the automatic rule plus manual override, with a warning when a page ends up with two `h1`s or a skipped level — and the builder should surface that warning before publish.
- Header and footer edits sit on top of theme defaults; copying a theme's markup into the database would make theme updates impossible.
- Long navigation lists, long headlines and empty lists are the realistic breakages; they are part of the definition of done, not a stretch goal.
- Scope section CSS so an override wins by layer order rather than by specificity escalation, otherwise the first theme that restyles a section forces `!important` into the codebase.
- Validation must run server-side even though the panel validates too: the panel is a convenience, the API is the contract.
