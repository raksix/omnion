# REQ-062 — Themes & Theme Builder

> **Status:** in-progress (tick 61 — **the contrast badge was measuring the last SAVE and printing its all-clear beside a preview of a different palette.** `view.contrast` while the form held the draft: edit the accent into a failing pair and the panel said “every pair meets WCAG AA” while the preview beside it showed otherwise, and the publish refusal — whose whole message is *read the contrast panel, then publish again* — pointed at that all-clear. The acknowledgement rode the same stale read, so the browser was deciding whether the server's guard ran. Fixed with a server dry run (`contrast-check`) the panel calls debounced on what it holds; `null` (unmeasured) is distinct from `[]` (measured, clean) and only the second may print an all-clear. Proof: 2 API walks **verified to fail first** (14/2), `cms_theme_settings` **16/16** on real PostgreSQL, `omnion-content --lib` **346/0**, `tsc --noEmit` exit 0, `probe-contrast-live.cjs` **11/11** against **2/11** on the pre-change tree. Criterion 11 stays unticked — no browser pass has watched the badge. PREVIOUS: tick 60 built the branding editor.)
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
      - *The colour-swap half WAS the defect: FIXED and PROVEN (`8403f2e8`). The browser half has not run, so this stays unticked.* All ten sheets ship in one bundle and every element rule in them is scoped to `html[data-theme="<key>"]`, so that one attribute decided whether a theme painted anything — and `app/layout.tsx` wrote it from the **installation default** while the page resolved the **addressed site's** theme. Any site not on `minimal` therefore rendered a complete, valid, entirely wrong page: Magazine's markup under a Minimal attribute, every theme token unset, the body inheriting Minimal's palette and sans stack. Measured against the ten real sheets, `--ma-magazine-canvas` goes `#fffdf9` → unset and `body` `rgb(255,253,249)`/serif → `rgb(250,249,245)`/`ui-sans-serif`. The layout now resolves the request's site (through `proxy.ts` — a layout cannot read `searchParams`) and the page reads the same header, so the two halves of one page agree. `scripts/qa/probe-site-theme.cjs` drives the real modules against three sites on three themes and asserts document and page agree — **verified to fail first** against the old tree (`layout themes: {"main":"minimal","shop":"minimal"}`) and to pass after (8/8); it runs from `run.sh`. *Not ticked:* "renders its pages" is a browser claim, and no pass has run.
- [x] Each theme ships a manifest that validates against the v2 schema (slots, tokens, settingsSchema, compatibility) and declares light and dark modes.
      - *The half that was missing was VALIDITY, and it was hiding a defect in all ten themes.* `manifest_shape` counted `slots`/`tokens` and noted which v2 fields were **present** — it could not object to a wrong one. Every bundled theme declared the layout slot `single`, while the slot the builder's picker holds and the renderer asks for is **`single-page`**; the presence check passed those files because the field was there, and it was wrong. `validate_v2_fields` (`3a915842`) now checks that what a theme declares can be rendered: slot names against the same `is_slot` the package validator uses, token shapes, `settingsSchema` `type`/`default`/`min`/`max` coherence (a default outside its own range, a one-sided bound, a bound on a text setting, a `select` default outside its own options), the compatibility engine, alias key shapes, and a `previewImage` that walks out of the theme's own directory. Nothing short-circuits: seven faults give seven rows. **11 unit tests, 7 verified to fail against the old presence-only behaviour.** The ten manifests are corrected in `a58cf648`; the light/dark half was already proven and stays proven.
- [ ] The gallery lists ten bundled themes with preview images, and the active one carries a badge.
      - *The image half was a DEAD PROMISE until this tick; the badge half was already real.* `previewImage` was on the manifest, in the gallery payload and in the client's `ThemeCard` type since slice 1, and **no component rendered it** — the card had no image element at all, so a field carried through three layers and read by none was the whole of this criterion. `GET /api/v1/themes/{key}/assets/{file}` (`9c4e3dee`) is now the reader: an allow-list of the two files a bundled theme serves, with the KEY validated first (`validate_key` lowercases what it accepts, so the check is "accepted AND unchanged"), bytes cached once, the SVG served with a sandbox CSP and `nosniff`. The card draws `previewUrl` when the server offers one and a deterministic per-key swatch when it does not (`f5cd6de9`) — an uploaded package's bytes live in object storage, so no image is the honest state for it. The URL comes from `preview_url` in the content crate, shared with the route. **5 unit tests, one of which is the traversal case the first version of `served_names` failed**, plus one that reads all ten real themes off disk. The `Active` badge was already the server's `activeKey` and stays proven. *Not ticked:* "the gallery lists ten" is a browser count and no pass has run.
- [ ] Activating a theme from the preview changes what a signed-out visitor sees within one refresh, and keeps the previous theme key for rollback.
- [x] `Restore previous` brings back the previous theme *and* its published settings revision, confirmed by comparing the rendered page.
- [ ] Customize edits (colour token, base font size, radius, logo) appear in the live preview before saving and in the public site after `Publish`.
- [ ] A contrast warning appears when a text/background token pair drops below WCAG AA, and the publish action requires acknowledgement.
      - *The guard and the badge were both real and the badge was measuring the WRONG PALETTE.* The screen keeps `form` (what the operator is editing) and `view` (the last server response) side by side, and it rendered `view.contrast` — a measurement of the last SAVE — beside a live preview of the draft being typed. Rule 2 of the view's own header claimed "measured by the SERVER, on every edit" while reading the last save: the rule was right and the code was wrong, which is the worse of the two, because a documented invariant nobody checks is a lie with a comment on it.
      - *What the operator actually saw.* Edit the accent into a failing pair and the panel printed **"Every text/background pair in this draft meets WCAG AA"** while the preview beside it showed the failing colours. Press *Publish* and the server refuses `theme_settings_contrast_required`, whose entire message is "read the contrast panel, then publish again" — sending the operator to the all-clear that had just told them everything was fine. A screen that contradicts itself and then cites the contradiction as the instruction.
      - *The acknowledgement was riding the same stale read.* `acknowledge` was `view.contrast.length === 0 || contrastSeen`, so **the browser decided whether the server's guard ran**. `5c6c3d6d`: the panel asks the server about the palette it is holding (`POST …/theme-settings/contrast-check`, debounced 300 ms, aborted so two in-flight measurements cannot land out of order), and `acknowledge` is `(liveContrast?.length ?? 1) === 0 || contrastSeen` — an **unmeasured** palette sends `false`, so the server's own refusal is the answer rather than this client asserting a verdict it never computed. `null` is deliberately distinct from `[]`: an empty list is "measured, nothing fails" and may print the all-clear; `null` is "not measured" and may not. A failed check prints "Contrast could not be measured… the panel will not report a palette as passing a check that did not run", because in a measurement panel silence reads as a pass.
      - *Proof.* `POST /api/v1/sites/{id}/theme-settings/contrast-check` merges the theme defaults under the submitted overrides (a token the draft does not set is still painted from the theme), writes nothing, and is guarded on **`themes.read` rather than `themes.customize`** — refusing a measurement to an account that can read the draft it measures answers "your palette is broken" with a 403. **2 new API walks, verified to fail first at 14 passed / 2 failed** against a dry run that ignored its body; `cms_theme_settings` **16/16** in 104 s on real PostgreSQL; `omnion-content --lib` **346/0**; `tsc --noEmit` exit 0 on `apps/admin`; `probe-contrast-live.cjs` **11/11**, measured **2/11 on the pre-change tree**. *Not ticked:* a pass has still not watched the badge appear on screen. The value the operator reads is the server's, and the claim that it now matches the palette beside it is one this tick's gates prove statically.
      - *A lesson from two of my own red checks.* Both were in the probe, not the product, and both were confirmed against the source before the regex changed: the guard is a `Layer` in `routes/mod.rs` and not in the handler file (a check looking in the handler would have failed forever against a correctly wired route), and the acknowledgement is optional-chained because an unmeasured palette must not read as nothing to acknowledge. Loosening a gate until it is green is how a gate stops measuring anything.
- [ ] Saving settings twice creates revisions 1 and 2; restoring revision 1 reverts the tokens and is itself recorded as a new revision (history is append-only).
- [x] Logo upload rejects files above the configured size and enforces the declared min/max dimensions with a field-level message.
      - *Both halves were absent: the section was stored, unlooked at.* `branding` was the ONE part of a settings payload the platform did not walk -- tokens, typography and layout all go through `check_token_values`, because the renderer writes every string in them into a CSS custom property, and branding went in as bare jsonb. `crates/content/src/branding.rs`: `BrandingLimits::for_theme` reads the theme manifest's `settingsSchema` and keeps the TIGHTER of the declaration and the platform default, so an installed theme cannot raise the ceiling for a shared installation; `validate_branding` returns EVERY finding with the field it belongs to, because an operator who fixes the logo and resubmits should not then discover the favicon; SVG is refused by TYPE rather than sanitised, since a header element must not carry script.
      - *The wiring (`4c232637`) is where the interesting half is: where the SIZE and the DIMENSIONS are read from.* Size and type are on `media`; width and height are **not on `media` at all** -- the upload path probes the header and writes the geometry onto `media_versions` version 1. A resolver reading only the media table would find no dimensions and quietly skip the entire second half of the criterion, which is precisely how a check that looks installed goes unmeasured. Scope is enforced by the SQL (`where m.site_id = $1`), so another site's file answers `UnknownAsset` -- the same answer as a deleted file -- rather than passing a second comparison afterwards.
      - *Panel half, tick 60.* `apps/admin/features/themes/theme-branding-editor.tsx`: the section is no longer the shared free-text editor, it offers the media library (images only), uploads through it, renders one `<ul role="alert">` per slot from `details.findings[].field`, shows the current value as a thumbnail, clears a key to `null`, and retires a field's message when that field is edited. The limits are displayed **from the server** (`brandingLimits` on the view) rather than restated in TypeScript, because the ceiling comes from the theme manifest and a client copy would be wrong for exactly the themes that narrow it. `BrandingLimits` gained `Serialize`; `settings_view` takes the limits as an argument rather than re-reading a manifest the store cannot see.
            - *Proof.* **18 unit tests, 6 verified to fail** against a validator with the size and pixel checks removed (12 passed / 6 failed). **3 API walks, all verified to fail first** against a route with `check_branding` deleted (0/3 -> 3/3): a logo 3 MB AND 6000 px refused for two independent reasons with no revision written, another site's file refused while the same file on its own site saves clean, and an SVG refused by type while a measured PNG is stored and read back through the API. `cms_theme_settings` **14/14**; `omnion-content --lib` **346/0**. *Panel half:* `tsc --noEmit` exit 0 and `node scripts/qa/probe-branding-editor-wiring.cjs` **31/31**, measured **9/31 against the pre-change tree** so the gate is known to fail without the editor. **The box above stays as tick 59 ticked it and is NOT re-ticked here:** the field-level message is now *returned and rendered*, but no browser pass has yet *observed* it on screen, and ticking a visibility claim from a static gate is the one thing this REQ's notes keep refusing to do. A `--only=theme-customize` pass is owed before criterion 9 can be called whole.
- [ ] The Builder saves a header slot made of theme blocks, shows it in the preview, and `Reset slot to theme default` restores the shipped layout.
      - *API half proven (17/17 walks):* `PUT/POST/GET /sites/{id}/theme-layouts{,/{slot},/{slot}/reset}` store a theme-block tree, mark it the site's own, touch no page and no page revision, and a reset restores the theme's own tree — the last part only after `0173`, because before it the first custom save destroyed the default.
      - *SCREEN BUILT this tick (`f23c0667`), pass not yet run:* `/themes/<key>/builder` reuses the page editor wholesale — `BlockCanvas`, `BlockInspector`, `InsertPanel`, the whole of `block-tree.ts` and the undo stack — because a header, a footer and a page body are all block trees drawn by one renderer, and a second editor for slots would be a second implementation of insert, reorder, duplicate, delete, nesting, the inspector and undo. What it adds is the slot picker (all eight, always, including the empty ones: a picker that hides an empty slot cannot answer "what if I clear the header"), a reset drawn only for a slot the theme actually ships something for, and a save whose notice says *saved for this site* and never *rendering*. `runThemeBuilderDepth` drives it and reads the TABLE after every action — `is_default = false` plus the edited block in `blocks`, the theme's tree still in `default_blocks`, then a reset returning `is_default` to true. *Not ticked:* the browser pass has not run.
- [ ] Export produces a package that imports on a second site and renders identically for slots and tokens.
      - *Round-trip proven:* `an_export_round_trips_through_the_validator` — the export validates clean and carries the site's own slots plus the theme defaults. *Not ticked:* the criterion says "renders identically", which is a rendering claim; it needs the two sites in a browser.
- [ ] Import validation refuses a package with an unknown slot or an unknown block type and lists each problem; a valid package installs as inactive.
      - *API half proven:* `a_bad_package_reports_every_problem_not_only_the_first` (four findings, not the first), `a_package_with_errors_installs_nothing` (the refusal carries the whole report in `error.message`), `a_valid_package_installs_inactive_and_changes_no_site` (no `site_themes` row).
      - *SCREEN BUILT this tick (`f23c0667`), pass not yet run:* `/themes/upload` reads the file, validates BEFORE it installs, and lists every finding with the path into the file. Picking another file clears the previous report, so validating file A and installing file B is impossible. The install lands inactive and its button is only offered on a report the screen has read; the depth pass proves "inactive" by the ABSENCE of a `site_themes` row and "lists each problem" by counting the findings on screen against the three faults the fixture package carries on purpose. *Not ticked:* the browser pass has not run.
- [ ] An uploaded theme cannot be activated before installation completes, cannot be deleted while active, and a bundled theme can never be deleted.
      - *Two of three proven at the API layer:* `a_valid_package_installs_inactive_and_changes_no_site` (an install never activates) and `a_bundled_or_in_use_theme_cannot_be_removed` (bundled answers `theme_bundled_cannot_be_removed`, in-use answers `theme_in_use`).
      - *The gallery's remove control was a DEAD BUTTON until this tick.* `canDelete` has been on the card since slice 3 shipped `DELETE /themes/{key}`, and the button had no `onClick` at all — which is why this clause stayed unticked while the route underneath it was fully tested by two walks. It now opens a confirmation naming the theme, calls `removeTheme`, and re-reads the gallery; the pass asserts the button is *behaviourally* wired (the dialog names the theme and states the bundled refusal) rather than that a `data-` attribute exists. *Not ticked:* the browser pass has not run.
- [ ] `omnion create-theme my-theme` produces the documented skeleton, and the generated theme builds and renders a page without edits.
      - *BUILD half proven (`2a34fdd4`):* the six generated files were linked into the real workspace (`pnpm --filter @omnion/web add`), registered in the renderer's registry and compiled by `tsc --noEmit` over `apps/web` — exit 0, with the theme's own files reached by the compiler. The first attempt at that proof passed `tsc` while the theme was **not linked at all** (nothing imported it, so nothing compiled it); the second failed with four TS2307s that exposed the missing `apps/web` dependency, and the fix is now the command's own `Next` output. *Not ticked:* the render half — the compiler accepting a theme and a browser drawing a page with it are two different claims, and only the first has run.
- [x] An unknown theme key in the site payload falls back to `minimal` with a warning in the log, and the visitor still sees a complete page.
      - *The fallback was already correct and the warning did not exist at all* — an unresolvable key resolved to `minimal` with no line anywhere, which is how an operator who activated a theme this build does not ship is left with a site that mysteriously looks like Minimal and nothing in the log to say so. `resolveThemeOrWarn` keeps the identical resolution and adds the missing half: the key, the fallback and the keys this build ships, **once per key per process** rather than once per request — a key is a property of the installation, and a flood is how an operator learns to ignore the one line that would have told them. `the fallback is reported, not silent` reads **stderr**, where `console.warn` actually writes; the first version of this probe read stdout and failed while the themes resolved correctly, which is exactly how a correct renderer gets "fixed" by removing its warning. Both halves asserted by `probe-site-theme.cjs`: `ghost` (a real site on `no-such-theme`) falls back to `minimal` **and** the warning names the key — 8/8, verified to fail first against the old tree.
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
