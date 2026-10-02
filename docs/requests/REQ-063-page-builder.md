# REQ-063 — Block System & Page Builder

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform (`apps/admin` + `crates/content`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Blocks and the visual page builder.

- **Block registry**: server-defined block schemas (heading, text, image, gallery, video, CTA, columns, card grid, pricing table, testimonial, FAQ, form, embed, raw HTML, product grid, blog list) with typed props.
- **Block editor**: insert, reorder (drag), duplicate, delete, per-block settings panel, nested containers (columns with child blocks).
- **Patterns & templates**: reusable block groups and full page templates (landing, about, pricing, blog post, contact).
- **Content model**: blocks stored as typed JSON in revisions (same versioning as pages), rendered by `apps/web` through the theme engine.
- **Inline editing** on the front-end preview (click text to edit, save as draft revision).
- **Accessibility & responsiveness**: per-block viewport settings (hide on mobile/desktop), semantic HTML output.
- **Events**: `content.blocks.updated`, `content.page.published`.

## Implementation spec

### Scope (in / out)

**In**
- Registry in `crates/content` (`blocks` module): one `BlockDefinition` per type — key, label, category, icon, `propsSchema` (JSON Schema subset: type, required, enum, min/max, maxLength, default), `allowedChildren`, `slots` (for containers), and render hints (`semantic`, `viewportAware`). The registry is code, versioned with the platform, and exposed read-only to the panel; DB rows store payloads only.
- Sixteen block types: `heading`, `text`, `image`, `gallery`, `video`, `cta`, `columns`, `card_grid`, `pricing_table`, `testimonial`, `faq`, `form`, `embed`, `raw_html`, `product_grid`, `blog_list`. Containers: `columns` (2–4 column slots with child blocks) and `cta`/`card_grid` item slots.
- Editor: insert via `+ Block` panel and a `/` slash menu, drag reorder (with drop indicators), duplicate, delete, block toolbar (move up/down, duplicate, delete, insert below, copy/paste), per-block settings panel generated from `propsSchema`, nested containers with a breadcrumb for deep selection, undo/redo stack (minimum 50 steps) persisted per session.
- Store: blocks as typed JSON on the page's working draft revision — one new `blocks jsonb` column on `page_revisions`, appended to the existing append-only revision model (`page_revisions` keeps its immutability rules; publishing still freezes a revision).
- Patterns (reusable block groups) and page templates (landing, about, pricing, blog post, contact) with an "insert pattern" picker and "New page from template".
- Inline editing on the front-end preview frame: click a text block to edit in place, save as a draft revision (never auto-publish), with a clear "draft" banner on the frame.
- Renderer: `apps/web` renders `blocks` through the theme engine, falling back to the plain `body` text for revisions without blocks (backwards compatible with existing published pages).
- Accessibility and responsiveness: per-block viewport settings (`hide_on`: none/mobile/desktop, plus alignment), semantic HTML output (`h1–h6` levels validated, lists as lists, `figure`/`figcaption` for images, alt text required for `image`/`gallery`, landmark-safe sections), and heading-order linting surfaced as block warnings.
- Events and audit for structural changes.

**Out**
- Free-form CSS editing inside blocks (theme tokens own styling), custom block plugins authored by users (REQ-044/REQ-048 territory), collaborative multi-cursor editing.
- A separate page-builder app: the same editor component is embedded for the Theme Builder slots (REQ-062) and the page canvas.
- Authoring new content types (REQ-026 dynamic data model) — content types are read from the type registry when it exists.

### Screens (UI)

| Route | Screen |
|---|---|
| `/pages` | Page list (existing) with a `Blocks` indicator column |
| `/pages/<id>/edit` | Block editor — page tree, canvas, inspector |
| `/pages/<id>/preview` | Front-end preview frame with inline editing |
| `/pages/<id>/revisions` | Revision history with block-level diff (existing screen, extended) |
| `/blocks` | Block registry reference (browse types and props) |
| `/patterns` | Pattern library |
| `/page-templates` | Page template gallery with `New page from template` |

- **Editor layout.** Three panes: left — page tree (all pages of the site, with the current one highlighted) plus an outline of the current page's blocks (click to scroll, drag to reorder); centre — canvas with a viewport switcher (desktop / tablet / phone) and a page-width toggle; right — inspector for the selected block, sections: Content, Settings (`propsSchema`-generated fields with inline validation), Layout (width, padding step, background token), Visibility (`hide_on`, alignment), Advanced (`id`, `class`, ARIA label). Bottom bar: word count, block count, validation summary ("2 blocks need attention"), last saved at, `Save draft`, `Preview`, `Publish`.
- **Insert and manipulate.** `+ Block` opens a searchable panel grouped by category (Text, Media, Layout, Marketing, Content); `/` inside an empty block opens the same panel inline; dragging a block shows a 2 px drop indicator plus a nested drop zone highlight; containers show a "drop here" placeholder per column; keyboard: `⌘D` duplicate, `⌘⌥↑/↓` move, `Backspace` on an empty selection deletes the block after a confirm for containers with children, `⌘Z`/`⌘⇧Z` undo/redo, `⌘/` shortcut sheet, `Esc` clears selection. Copy/paste of blocks works across pages within the same site via the clipboard payload in `localStorage`.
- **Inspector validation.** Required props (alt text, heading level, link URL, form key) show a red field message and the block shows a warning badge; publishing is blocked while a hard error exists, saving a draft is not. A `raw_html` block carries an "untrusted content" note and is sanitized server-side on save (tag/attribute allow-list) with the sanitiser report shown when anything was stripped.
- **Patterns and templates.** Pattern library: cards grouped by category with a preview thumbnail, block count, `Insert`, `Edit`, `Duplicate`, `Delete`; `New pattern from selection`. Template gallery: cards for landing, about, pricing, blog post, contact with "sample content included" and `Use template` (asks for a slug and title, then creates the page with the blocks as a draft).
- **Inline editing.** `/pages/<id>/preview` loads the renderer in a frame with an overlay toolbar (`Edit inline` toggle, `Save draft`, `Exit`). With inline editing on, text blocks become content-editable in place, each save creates one draft revision and a toast names the revision number; the frame shows a persistent `Draft` badge and never publishes. Leaving with unsaved inline edits prompts before discarding.
- **States, keys, mobile.** Empty page shows the insert panel opened and a short "start from a pattern" list. Loading uses skeleton blocks. A failed save keeps the local state and shows a retry banner (no silent loss). On mobile (≤ 900 px) the editor opens read-only with a "editing needs a wider screen, preview is available" notice, and the preview remains fully usable.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/blocks` | Block registry (definitions, propsSchema, categories) | `content.blocks.read` |
| POST | `/api/v1/blocks/validate` | Validate a block tree against the registry (returns per-block issues) | `content.blocks.read` |
| GET | `/api/v1/pages/{id}` | Page with working draft revision including `blocks` | `content.pages.read` |
| PATCH | `/api/v1/pages/{id}` | Update title/slug and the draft revision's `blocks` (validated) | `content.pages.update` |
| POST | `/api/v1/pages/{id}/publish` | Publish the working draft (existing route, now block-aware) | `content.pages.publish` |
| GET | `/api/v1/pages/{id}/revisions/{no}/diff` | Block-level diff between two revisions | `content.pages.read` |
| GET | `/api/v1/pages/{id}/preview` | Renderer-frame payload for the draft (signed, short-lived) | `content.pages.read` |
| GET · POST | `/api/v1/patterns` | Pattern list (`?category=`) · create from blocks or selection | `content.blocks.read` · `content.patterns.manage` |
| GET · PUT · DELETE | `/api/v1/patterns/{id}` | Read · update · delete | `content.blocks.read` · `content.patterns.manage` |
| GET · POST | `/api/v1/page-templates` | Template list · create/update | `content.blocks.read` · `content.templates.manage` |
| POST | `/api/v1/pages/from-template` | Create a page from a template (slug, title) | `content.pages.create` |
| GET | `/api/v1/public/pages/{slug}` | Public render payload — now includes the published revision's `blocks` | — |

Validation errors are per block: `{block_id, path, code, message}` with codes such as `block_unknown_type`, `block_prop_required`, `block_heading_order`, `block_alt_missing`, `block_child_not_allowed`, `block_html_sanitized`.

### Data model

Migrations: `0110_cms_blocks.sql`, `0111_content_patterns.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
-- 0110_cms_blocks.sql
alter table page_revisions add column blocks jsonb not null default '[]';
alter table page_revisions add constraint page_revisions_blocks_is_array
  check (jsonb_typeof(blocks) = 'array');
-- optional index for content search over block text
create index page_revisions_blocks_gin on page_revisions using gin (blocks jsonb_path_ops);
-- 0111_content_patterns.sql
content_patterns (id uuid pk, organization_id uuid not null -> organizations, key text not null,
  name text not null, category text not null default 'general', description text,
  blocks jsonb not null default '[]' check (jsonb_typeof(blocks) = 'array'),
  created_by uuid null -> users, created_at/updated_at timestamptz not null default now())
  unique (organization_id, key); index (organization_id, category)
content_page_templates (id uuid pk, organization_id uuid not null, key text not null, name text not null,
  page_type text not null default 'page', description text,
  blocks jsonb not null default '[]' check (jsonb_typeof(blocks) = 'array'),
  is_system boolean not null default false, created_by uuid null, created_at/updated_at timestamptz)
  unique (organization_id, key)
```

Block JSON shape (one array entry): `{id: uuid, type: string, props: object, children?: [{...}], slots?: {columns: [[...]]}, meta: {hide_on, align, anchor}}`. Ids are client-generated UUIDs so reordering never rewrites identity and diffs stay readable. Existing rows keep `blocks = '[]'` and render from `body`, so released content never breaks.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `content.blocks.updated` | Draft revision blocks changed | `page_id`, `revision_no`, `block_count`, `actor_user_id` |
| `content.page.published` | Publishing a revision (existing, now carries blocks) | `page_id`, `revision_no`, `slug` |
| `content.pattern.created` · `.updated` · `.deleted` | Pattern library changes | `pattern_id`, `key`, `category` |
| `content.template.created` | Page template saved | `template_id`, `key`, `page_type` |
| `content.blocks.validation_failed` | A save was rejected by validation | `page_id`, `issues[]` |

Consumed: `media.deleted` (mark image/gallery blocks with a broken-media warning instead of rendering a dead URL), `content.pages.deleted` (drop pattern references), `themes.layout.saved` (a theme slot may embed the same block ids — no action, but keeps audit context). Webhook relevance: `content.page.published` already drives cache invalidation and downstream integrations; the block payload only adds a `block_count` hint, never the full body.

### Acceptance criteria

- [ ] `GET /api/v1/blocks` returns all sixteen types with propsSchema, and `/blocks` renders that reference without hard-coded lists in the panel.
- [ ] Inserting one of every type produces a valid draft, and saving it round-trips through the API without losing props.
- [ ] Reordering with drag (and with `⌘⌥↑/↓`) persists the new order and does not change block ids, proven by reloading the editor.
- [ ] Duplicate clones a block with a new id and keeps the original untouched; delete removes only the selected block or subtree after the confirm.
- [ ] A `columns` container accepts 2–4 child columns, each accepting child blocks, and the editor's breadcrumb selects a nested block directly.
- [ ] Required-prop validation blocks publish (`block_alt_missing`, `block_prop_required`) but still allows saving a draft, and the offending block is highlighted.
- [ ] Heading order linting warns when an `h2` block precedes the page's `h1`, and the warning disappears after reordering.
- [ ] `raw_html` is sanitized on save; a script tag is stripped, the sanitiser report lists what changed, and the stored payload no longer contains it.
- [ ] Undo/redo covers at least 50 steps including nesting changes, and `⌘Z` after a save restores the pre-save state in the draft.
- [ ] A pattern inserted into a page reproduces the block tree exactly; creating a pattern from a selection works and the new pattern appears in the library.
- [ ] `New page from template` creates a draft page whose blocks match the template, with the sample content intact.
- [ ] The public page renders block output through the active theme, and a revision without blocks (existing content) renders from `body` unchanged.
- [ ] The revision diff shows added/removed/changed blocks with prop-level detail, not a raw JSON diff.
- [ ] Inline editing saves one draft revision per save, shows the revision number in the toast, and never publishes — verified by checking the published revision number stays the same.
- [ ] Blocks marked `hide_on: mobile` are absent from the mobile render (server-side), not merely CSS-hidden, and the semantic output check passes (headings, lists, figure/figcaption).
- [ ] `content.blocks.updated` and `content.page.published` are delivered to a subscribed endpoint with redelivery working.
- [ ] The editor is usable at 1440 px and 390 px without horizontal scroll (read-only notice on the phone), and the walkthrough reports zero high findings.

### QA plan

The walkthrough must open `/pages/<id>/edit` on the seeded page, insert one block of each category from `+ Block`, use the `/` menu in an empty block, drag one block above another, duplicate it, delete one, edit props in the inspector including a deliberately invalid value (expect the field message and the publish block), nest blocks inside a `columns` container, undo and redo, save the draft, and then publish. It must open `/pages/<id>/revisions`, compare two revisions, restore one; open `/pages/<id>/preview`, toggle inline editing, edit a text block, save, and confirm the draft badge; open `/patterns` and insert a pattern into the page; open `/page-templates` and create a page from the landing template; and open `/blocks` to confirm the registry reference renders. Visual check: the canvas shows a real page with real blocks (no placeholder boxes), the inspector matches the selected block's schema, validation badges are visible and legible, the preview frame shows the theme's real styling with the draft badge, and publishing makes the page appear on the public site.

### Slices

1. **Registry, storage, minimal editor.** Migration `0110_cms_blocks.sql`; block definitions with propsSchemas and validation, `blocks` on `page_revisions`, registry + validate routes, renderer support in `apps/web` with the `body` fallback, editor canvas with insert / reorder / duplicate / delete / inspector / save draft / publish, `/blocks` reference screen. *Done when:* acceptance 1–3, 5, 9, 12, 17 pass and `/pages/<id>/edit` is in the walkthrough inventory.
2. **Containers, validation, revision diff.** Nested `columns`, breadcrumb selection, accessibility and viewport rules (`hide_on` server-side), heading-order linting, `raw_html` sanitisation, block-level diff on the revisions screen, inline-editing frame at `/pages/<id>/preview`. *Done when:* acceptance 4, 6–8, 13–15 pass.
3. **Patterns and templates.** Migration `0111_content_patterns.sql`; pattern library with insert/create-from-selection/edit/duplicate, page templates with sample content, `/pages/from-template`, and the initial template set (landing, about, pricing, blog post, contact). *Done when:* acceptance 10–11 pass and the vision review confirms the templates render as real pages.
4. **Polish and events.** Undo/redo persistence, mobile read-only behaviour, empty/loading/error states, the five events with a verified delivery, and the media-deleted degradation path. *Done when:* acceptance 16 passes, the walkthrough covers all new screens, and the QA report shows zero high findings.

### Risks / notes

- The revision model must not be weakened: blocks live on the draft revision and publishing still freezes a revision. Any "save in place" shortcut would break compare/restore and is explicitly forbidden.
- Nested editing is where builders get confusing; the outline pane and breadcrumb are the mitigation, and depth is capped at three levels with a clear message rather than an unbounded tree.
- `raw_html` and `embed` are the security surface: strict tag/attribute allow-lists, no `script`, no `iframe` outside an allow-list of hosts, sanitisation server-side (client-side checks are UX only).
- Renderer parity: the panel preview and the public render must use the same block renderer component, or the "preview lies" class of bugs returns.
- `blocks` is JSON, so schema drift is a real risk: every block payload carries a `type` plus a registry version check on read, and unknown types render as a placeholder with a warning rather than crashing a page.
- Text search over block content needs the JSONB index from the migration; without it, content search degrades as pages grow.
- Turkish example copy belongs to sample template and pattern content (for example a demo pricing line or a contact form heading), never to block labels or registry strings.
