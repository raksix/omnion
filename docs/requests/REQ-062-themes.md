# REQ-062 — Themes & Theme Builder

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform (`themes/*` + admin)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

The theme system made real (docs/03-FRONTEND.md).

- **Ten default themes** shipped in-repo (minimal, editorial, corporate, portfolio, magazine, restaurant, saas, nonprofit, agency, docs) — each with tokens, layouts, and sample content.
- **Theme settings UI**: colors, typography scale, radius, spacing, logo/favicon, header/footer variants, dark mode support — persisted per site.
- **Theme Builder**: visual editing of layouts (header, footer, blog list, single page, product) using the block system; live preview; export/import theme package.
- **Theme SDK**: documented contract for theme packages (manifest, tokens, blocks, slots) + scaffolding CLI.
- **Per-site activation** with preview before publish and one-click rollback.
- **Marketplace-ready**: theme package format for REQ-048.

## Implementation spec

### Scope (in / out)

**In**
- Ten shipped themes in `themes/<key>/`, built on the existing contract (`packages/theme-sdk`: `defineTheme`, `ThemeManifest`, `SiteTheme`, `PageLayout`) and the renderer registry in `apps/web/lib/theme.ts`. Canonical keys are the ten of docs/03-FRONTEND.md §"Ten default themes": `corporate`, `tech`, `agency`, `portfolio`, `magazine`, `commerce`, `documentation`, `startup`, `minimal`, `government`. The request's example names are covered by the closest canonical themes (`editorial`→`magazine`, `restaurant`→`commerce`, `saas`→`tech`, `nonprofit`→`government`, `docs`→`documentation`); each manifest may declare `aliases` so an installation pinned to an older or alternate key still resolves.
- Manifest v2: extends the current `omnion.theme.json` with `slots`, `tokens`, `settingsSchema`, `blocks`, `previewImage`, `screenshots`, `aliases`, `compatibility` — still pure data, never code.
- Theme settings per site: colour tokens (with light/dark values and a contrast check), typography scale, radius and spacing scales, container width, logo and favicon, header/footer variants, default colour mode. Every save is a revision; publishing is explicit; one-click rollback to any earlier revision.
- Theme Builder: visual editing of the layout slots (header, footer, home, blog list, single page/post, product, 404, search) by arranging blocks from the REQ-063 registry; live preview in an iframe; export/import of a theme package (JSON manifests + serialized slot layouts + token overrides).
- Activation: preview before publish, activate per site, rollback to the previous theme with its settings intact.
- Marketplace-ready package format: a versioned bundle (`theme.json` manifest + `layouts/*.json` + `tokens.json` + assets) that REQ-048 can install and REQ-044 can validate.
- Scaffolding CLI: `omnion create-theme <key>` generating the documented skeleton (`omnion.theme.json`, `src/theme.ts`, `src/page-layout.tsx`, `styles/<key>.css`).
- Quality bar from docs/03-FRONTEND.md: mobile-first responsive layout, dark mode, accessible contrast and focus states, skeleton/loading states, SEO metadata and Open Graph, tasteful motion, no colour-swapped clones — each theme has its own layout, components, type system, hero, card system and blog design.

**Out**
- Uploaded themes may not ship executable JavaScript in v1: an uploaded package delivers declarative slots, tokens, CSS and asset files only. Code-carrying themes go through the plugin trust path (REQ-044/REQ-048) with an explicit trust decision, never a silent install.
- Per-block styling languages, Sass build pipelines, child themes beyond `parent` + token overrides.
- The marketplace itself (REQ-048) and the white-label settings surface (REQ-043).
- New content types: theme slots render what the content API serves.

### Screens (UI)

| Route | Screen |
|---|---|
| `/themes` | Theme gallery (installed + bundled), site switcher in the header |
| `/themes/<key>/preview` | Full-width preview with device and mode switcher |
| `/themes/<key>/customize` | Theme settings — tokens, branding, header/footer, modes |
| `/themes/<key>/builder` | Theme Builder — slot picker + block canvas |
| `/themes/upload` | Package upload with a validation report |
| `/themes/<key>/history` | Settings revisions with diff preview and rollback |

- **Gallery.** Cards show preview image, name, version, modes, page types, an `Active` badge and the actions `Preview`, `Activate`, `Customize`, `Builder`, `Export`. Filters: all / bundled / installed / active; search by name. Activating shows a confirmation strip: "The current theme's settings are kept and can be restored" with the rollback link. Empty state explains that the ten bundled themes are always available; loading uses skeleton cards.
- **Preview.** The site rendered with the candidate theme in an iframe pointing at the renderer's preview mode (site key + theme key + optional settings revision overlay), device switcher (desktop 1440 / tablet 1024 / phone 390), light/dark/system toggle, page picker populated from the site's published pages plus a "sample content" entry when the site is empty. A sticky bar carries `Activate this theme` and `Back to gallery`.
- **Customize.** Left panel sections: Colours (per token, light and dark values, reset to theme default, live contrast badge that warns below WCAG AA for text pairs), Typography (base size, scale ratio, font stacks, heading weight), Layout (container width, radius, spacing scale), Branding (logo, dark logo, favicon with upload via the media library, min/max dimensions validated), Header/Footer (variant picker with thumbnails), Modes (default mode, allow visitor switch). Right: live preview reflecting edits without saving. Footer bar: `Discard`, `Save draft`, `Publish`, and `Restore default`. Saving writes a revision; the history screen lists revisions with per-token diffs and a `Restore` action.
- **Builder.** Left: slot picker (Header, Footer, Home, Blog list, Single page, Product, 404, Search) with a badge when the slot is theme-provided, customised, or empty. Centre: the block canvas (same component as the page builder, REQ-063) with drag, nesting inside columns/containers, and a slot-level empty state. Right: block inspector. Toolbar: undo/redo, viewport switch, `Preview`, `Save draft`, `Reset slot to theme default`, `Export package`, `Import package`. Import shows a validation report (manifest schema, unknown slots, block types not present in the registry, asset limits) and refuses to apply anything when the report has errors.
- **Upload.** Drag-and-drop or file picker for a `.zip` theme package, then a report list with pass/warn/fail rows, the manifest summary, and `Install`. Installed packages are inactive until activated, and appear in the gallery with an `Uploaded` tag.
- **Rollback.** Activating a different theme keeps the previous theme's settings; the gallery's `Restore previous` restores both theme and settings revision in one action, with a confirmation naming exactly what changes (theme key, settings revision, layout slots customised).
- **States, keys, mobile.** Global keyboard: `g t` gallery, `p` preview, `b` builder, `⌘S` save draft, `⌘⇧P` publish; the builder supports `⌘Z/⌘⇧Z`, and `⌘/` opens the shortcut sheet. Mobile: the gallery is a single column, customize stacks preview above settings with a sticky save bar, and the builder canvas opens read-only with a note that editing needs a wider screen (never a silently broken editor).

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/themes` | Installed + bundled themes with manifest data and active flag | `themes.read` |
| GET | `/api/v1/themes/{key}` | One theme: manifest, slots, token defaults, sample content flag | `themes.read` |
| POST | `/api/v1/sites/{site_id}/theme` | Activate a theme for a site (records the previous choice) | `themes.activate` |
| POST | `/api/v1/sites/{site_id}/theme/rollback` | Restore the previous theme and its published settings | `themes.activate` |
| GET · PUT | `/api/v1/sites/{site_id}/theme-settings` | Read · save draft settings (tokens, branding, header/footer, mode) | `themes.read` · `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-settings/publish` | Publish the draft settings | `themes.customize` |
| GET | `/api/v1/sites/{site_id}/theme-settings/revisions` · `/{no}` | Revision list · one revision with diff | `themes.read` |
| POST | `/api/v1/sites/{site_id}/theme-settings/revisions/{no}/restore` | Restore an earlier settings revision | `themes.customize` |
| GET · PUT | `/api/v1/sites/{site_id}/theme-layouts/{slot}` | Read · save a slot's blocks (draft) | `themes.read` · `themes.customize` |
| POST | `/api/v1/sites/{site_id}/theme-layouts/{slot}/reset` | Reset a slot to the theme default | `themes.customize` |
| GET | `/api/v1/sites/{site_id}/theme-package/export` | Export the site's theme package (manifest + slots + tokens) | `themes.export` |
| POST | `/api/v1/themes/install` | Install an uploaded package (multipart zip) with validation report | `themes.install` |
| POST | `/api/v1/themes/validate` | Dry-run validation of a package without installing | `themes.install` |
| DELETE | `/api/v1/themes/{key}` | Remove an uploaded theme (never a bundled one, never the active one) | `themes.install` |
| GET | `/api/v1/public/themes/{key}/preview` | Renderer preview payload for the site preview frame (unauthenticated, site-scoped) | — |

The renderer reads the site's theme key from the public site payload (already the case) and the published settings revision from the same payload; an unknown or invalid key falls back to `minimal` so a visitor never sees a broken page.

### Data model

Migrations: `0108_themes.sql`, `0109_theme_settings.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
-- 0108_themes.sql
themes (id uuid pk, organization_id uuid null -> organizations,   -- null = bundled, platform-wide
  key text not null, name text not null, version text not null, source text in ('bundled','uploaded'),
  manifest jsonb not null, storage_key text null, checksum text null,
  installed_by uuid null -> users, installed_at timestamptz, removed_at timestamptz)
  unique (key) where removed_at is null; index (source)
site_themes (site_id uuid pk -> sites on delete cascade, theme_key text not null,
  previous_theme_key text null, activated_by uuid null, activated_at timestamptz not null default now(),
  settings_revision_id uuid null)
-- 0109_theme_settings.sql
theme_settings_revisions (id uuid pk, site_id uuid not null -> sites on delete cascade,
  revision_no integer not null, theme_key text not null,
  tokens jsonb not null default '{}', branding jsonb not null default '{}',
  header_footer jsonb not null default '{}', default_mode text not null default 'system',
  created_by uuid null, created_at timestamptz not null default now(), published_at timestamptz)
  unique (site_id, revision_no); index (site_id, created_at desc)
theme_layouts (id uuid pk, site_id uuid not null on delete cascade, theme_key text not null,
  slot text not null, blocks jsonb not null default '[]', is_default boolean not null default false,
  updated_by uuid null, updated_at timestamptz not null default now())
  unique (site_id, theme_key, slot)
```

Bundled themes are files, not rows: the loader reads `themes/*/omnion.theme.json` at boot and mirrors them into `themes` for the gallery. Token precedence at render time: theme default → site revision → published, and the renderer emits the effective tokens as CSS custom properties on `<html>` alongside `data-theme` / `data-theme-version`.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `themes.theme.activated` | A site activates a theme | `site_id`, `theme_key`, `previous_theme_key` |
| `themes.theme.rolled_back` | Previous theme restored | `site_id`, `theme_key`, `settings_revision_no` |
| `themes.settings.published` | Settings revision published | `site_id`, `revision_no`, `theme_key` |
| `themes.layout.saved` | A slot's blocks saved | `site_id`, `slot`, `theme_key` |
| `themes.package.installed` · `.removed` | Uploaded package lifecycle | `theme_key`, `version`, `source` |
| `themes.validation.failed` | Package validation rejected | `theme_key`, `errors[]` |

Consumed: `content.page.published` (refresh preview surfaces and slot caches), `media.deleted` (mark branding assets missing instead of serving broken URLs), `sites.created` (offer the onboarding theme choice, REQ-050). Webhook relevance: activation and settings events let a CDN or a static-site integration invalidate caches; payloads carry site and theme keys only.

### Acceptance criteria

- [ ] All ten themes resolve in the renderer registry, and a site activated on each renders its pages with that theme's layout, not a colour-swapped copy.
- [ ] Each theme ships a manifest that validates against the v2 schema (slots, tokens, settingsSchema, compatibility) and declares light and dark modes.
- [ ] The gallery lists ten bundled themes with preview images, and the active one carries a badge.
- [ ] Activating a theme from the preview changes what a signed-out visitor sees within one refresh, and keeps the previous theme key for rollback.
- [ ] `Restore previous` brings back the previous theme *and* its published settings revision, confirmed by comparing the rendered page.
- [ ] Customize edits (colour token, base font size, radius, logo) appear in the live preview before saving and in the public site after `Publish`.
- [ ] A contrast warning appears when a text/background token pair drops below WCAG AA, and the publish action requires acknowledgement.
- [ ] Saving settings twice creates revisions 1 and 2; restoring revision 1 reverts the tokens and is itself recorded as a new revision (history is append-only).
- [ ] Logo upload rejects files above the configured size and enforces the declared min/max dimensions with a field-level message.
- [ ] The Builder saves a header slot made of theme blocks, shows it in the preview, and `Reset slot to theme default` restores the shipped layout.
- [ ] Export produces a package that imports on a second site and renders identically for slots and tokens.
- [ ] Import validation refuses a package with an unknown slot or an unknown block type and lists each problem; a valid package installs as inactive.
- [ ] An uploaded theme cannot be activated before installation completes, cannot be deleted while active, and a bundled theme can never be deleted.
- [ ] `omnion create-theme my-theme` produces the documented skeleton, and the generated theme builds and renders a page without edits.
- [ ] An unknown theme key in the site payload falls back to `minimal` with a warning in the log, and the visitor still sees a complete page.
- [ ] Every theme passes the walkthrough at 390 px and 1440 px with zero high findings, and the vision review confirms real typography and layout differences between at least three themes.

### QA plan

The walkthrough must visit `/themes`, click `Preview` on two bundled themes, switch device and colour mode inside the preview, activate one, then hit `Restore previous`; visit `/themes/<key>/customize`, change a colour token and the base size, confirm the live preview updates, save a draft, publish, and open `/themes/<key>/history` to restore a revision; visit `/themes/<key>/builder`, pick the header slot, add a block, save, reset the slot; visit `/themes/upload` and upload the exported package (export first, then re-import) to see the validation report; and finally open the public site to confirm the new theme and settings render. Visual check: gallery cards show ten distinct preview images (not ten identical screenshots), the preview frame shows a real page with the theme's type scale, the customize panel shows working colour inputs and a contrast badge, and the public page shows the published token values. The mobile pass must show the single-column gallery and the builder's read-only notice instead of a broken editor.

### Slices

1. **Theme contract v2 and gallery.** Manifest v2 fields and loader, `themes` + `site_themes` tables (migration `0108_themes.sql`), gallery and preview screens, activation, rollback to the previous theme, renderer reading the site's settings revision with fallback. *Done when:* acceptance 1, 3–5, 15 pass and `/themes` is in the walkthrough inventory.
2. **Theme settings.** `theme_settings_revisions` (migration `0109_theme_settings.sql`), customize screen with tokens/branding/header-footer/modes, live preview, draft → publish, revision history with restore, contrast checks, logo upload validation. *Done when:* acceptance 6–9 pass.
3. **Theme Builder and packages.** `theme_layouts`, builder screen on the REQ-063 editor, slot reset, export/import package with validation, upload screen, install/remove rules. *Done when:* acceptance 10–13 pass (depends on REQ-063 slice 2 for nested containers).
4. **The ten themes, SDK and marketplace format.** Nine themes built to the quality bar with tokens, layouts and sample content; `omnion create-theme` in `tools/cli` with the documented skeleton; the package format documented for REQ-048 and validated by `POST /themes/validate`. *Done when:* acceptance 2, 14, 16 pass and every theme renders in the walkthrough.

### Risks / notes

- Theme/content separation is the invariant this REQ protects: switching themes must never touch `pages`, `page_revisions` or media. Any slot layout lives in `theme_layouts` (presentation), never in a page revision.
- Uploaded packages are untrusted input: zip-slip protection on extraction, asset type allow-list, size caps, no JavaScript execution in v1, and validation before anything is written.
- Token precedence and CSS variable emission must be single-sourced; two code paths that compute "effective tokens" will drift and show up as "the preview lies".
- The Builder depends on the block registry (REQ-063); build slices 1–2 first so the wave does not stall, and reuse the same editor component rather than forking it.
- Ten themes is a maintenance commitment: they share `@omnion/ui` primitives and a token contract so a platform change lands once, and each theme keeps its own layout and type system on top.
- Preview rendering costs a real page render; cache previews per (site, theme, settings revision, page) and never expose draft content through the preview route to a signed-out visitor.
- Turkish example copy belongs to sample content inside themes (for example a demo hero line or a menu item), never to layout code or tokens.
