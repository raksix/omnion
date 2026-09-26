# REQ-075 — Appearance & Themes Screen

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + themes
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The WordPress-style appearance experience.

- Appearance menu with: Themes, Customize, Widgets/Blocks, Theme Upload.
- Theme gallery (cards with preview image, name, version, author, active badge).
- Preview → Install → Activate flow with per-site activation and one-click rollback.
- Upload a theme package (zip) with validation and error reporting.
- Active theme card showing version, update availability and "Customize" entry point.

## Implementation spec

### Scope (in / out)

**In**
- One Appearance section in the panel shell: a sidebar group whose landing page is `/appearance` and whose children are Themes, Active theme, Widgets / Blocks, and Theme upload; `Customize` is an entry that carries the selected theme key into REQ-062's settings screen. The section is a grouping and a wrapper — it renders the same components and calls the same theme endpoints as REQ-062, it never forks them.
- Landing page (`/appearance`): the active-theme card (preview image, name, version, author, activation time and actor, installed → available version from REQ-078's check, `Customize`, `Preview`, `Restore previous`), a widgets/areas summary, a pending-updates count, and a recent-changes list read from the audit trail (REQ-039) filtered to `themes.*` and `appearance.*` actions.
- Theme gallery (`/appearance/themes`): grid (default) and list layouts over the same cards — preview image, name, version, author, source tag (`Bundled` / `Uploaded`), colour modes, an `Active` badge for the site in the shell's site switcher, and the actions `Preview`, `Install` (uploaded packages), `Activate`, `Customize`, `Remove`. Filters: source (all / bundled / uploaded), status (active / installed / inactive / update available), free-text search. The list layout adds a selection column for bulk `Update` (queues work in REQ-078) and bulk `Remove` (uploaded, inactive themes only).
- Preview before activation: a device-switching preview (desktop 1440 / tablet 1024 / phone 390), light/dark/system toggle, page picker built from the site's published pages with a `Sample content` entry when the site has none, and a sticky bar carrying `Activate on <site>` and `Back to gallery`.
- Activate / rollback: activation is per site (the site comes from the switcher, never a free-text field), it keeps the previous theme and its published settings, and `Restore previous` restores theme key plus settings revision in one step with a confirmation naming exactly what changes.
- Theme upload (`/appearance/themes/upload`): drag-and-drop or file picker for a `.zip` package with a maximum size and a single top-level directory; then a report of pass/warn/fail rows (manifest present and schema-valid, version parseable, slots known to the renderer, block types present in the REQ-063 registry, assets inside the allow-list, no executable files), the manifest summary, and `Install`. A failing report blocks install; a warning requires an acknowledgement checkbox. Installed packages are inactive until activated and carry an `Uploaded` tag in the gallery.
- Widgets / Blocks (`/appearance/widgets`): site-scoped widget areas with an ordered list (drag to reorder, `Active` toggle, block count, backing slot, last edited by/at) and an area editor that embeds the REQ-063 block editor in an area-scoped canvas. Blocks live in the slot's layout storage (REQ-062 `theme_layouts`, versions from REQ-076) so the appearance surface and the builder never hold two copies of the same blocks.
- Permission-aware rendering: any of `appearance.read`, `themes.read`, `themes.customize`, `themes.install` reveals the section; without the action's own permission the corresponding control is replaced by a short "needs <permission>" note instead of being hidden silently.
- Release notes and changelog for a pending theme update are read from REQ-078, never scraped or re-implemented here.

**Out**
- Theme settings (colour tokens, typography, branding, header/footer variants, modes) — REQ-062's Customize screen; `/appearance` links to it with the theme key pre-selected.
- Slot arrangement, variants and layout inheritance — REQ-076; the appearance screens link to the builder and never embed a second canvas.
- Package format, checksumming and validation *rules* — REQ-062 and REQ-044 own the format; this screen only presents their report.
- Update execution and rollback of the platform or of extensions — REQ-078 (the gallery shows availability and hands off).
- Menus and menu items — REQ-064. The Appearance menu links to the menus screen; navigation-as-a-slot-element belongs to REQ-083.
- Gallery listing for a marketplace catalogue — REQ-048.

### Screens (UI)

| Route | Screen |
|---|---|
| `/appearance` | Landing: active-theme card, areas summary, pending updates, recent changes |
| `/appearance/themes` | Theme gallery (grid + list) with filters, bulk actions and preview |
| `/appearance/themes/upload` | Package upload with the validation report |
| `/appearance/active` | Active-theme detail: version, update availability, activation history, rollback |
| `/appearance/widgets` | Widget areas list (order, active flag, block count, backing slot) |
| `/appearance/widgets/<key>` | One area: block canvas, settings, publish, reset to theme default |
| `/themes/<key>/customize` | Theme settings (REQ-062) — reached from here with the key preselected |
| `/appearance/builder/<slot>` | Theme Builder (REQ-076) — reached from the area editor's `Open in builder` |

- **Landing page.** One card, not a dashboard: the active-theme card leads with the preview image, then name, version, author, source, activation line ("Activated by Emre on 24 Sep, 14:02"), an `Update available: 2.4.0` strip when REQ-078 reports one (with `See what changes` opening the update detail), and the actions `Customize`, `Preview`, `Restore previous`. Below it: a widgets summary (`4 areas · 2 customised · 1 empty`), a pending-updates count for platform, plugins and themes, and the five most recent theme/appearance audit rows rendered as a mini timeline with a link to the full history in Activity (REQ-080).
- **Gallery.** Card: thumbnail (2:1), name, version, author, source tag, mode chips (`Light`, `Dark`), `Active` badge, action row. Hovering a card raises the preview; `Enter` on a focused card opens the preview. Grid is three columns at ≥1280 px, two at ≥900 px, one below; list layout shows a table with Thumbnail, Name, Version, Author, Source, Modes, Status, Last activated, Actions. Sort: name, version, last activated; default is active first, then bundled, then uploaded.
- **Bulk actions (list layout).** Selecting rows enables `Update selected` (each row keeps its own result; a run summary appears as a toast with a link to `/updates/history`) and `Remove selected`. `Remove` refuses any theme that is active on any site and reports per row (`skipped: active on Marketing site`). Destructive confirmations are typed for `Remove` (type the theme key) and a plain confirm for `Activate`.
- **Widgets.** List rows: handle, label, backing slot (`footer-columns`), state (`Active` / `Inactive`), block count, last edited. Drag to reorder with a drop indicator, order persisted on drop. `New area`: label (1–60 chars, required), key (`^[a-z][a-z0-9_]{1,39}$`, unique per site, auto-derived from the label), backing slot picker (theme-declared slots, plus `Custom (no theme default)`), active toggle. Area editor: the REQ-063 canvas with the area's blocks, inspector, `Save draft`, `Publish`, `Reset to theme default`, and `Open in builder` for variant work. Areas backed by a theme slot can be deactivated and reset but not deleted; custom areas can be deleted after a confirm.
- **Upload.** Drop zone, then a step list: `Reading package` → `Validating manifest` → `Checking slots and block types` → `Scanning assets` → `Ready to install`. Each finding is a row with severity, a message and, where useful, the offending path; a failing report shows `Install` disabled with the reason count. Successful install lands on the installed theme's card with an `Uploaded`, inactive badge and a hint that uploading never activates.
- **States.** No uploaded themes: gallery shows bundled cards plus an upload hint. No pages yet: preview offers `Sample content`. Loading: skeleton cards and skeleton area rows. Errors: an inline banner with the failing endpoint and `Retry`; a package that fails validation keeps the report on screen. Permission-limited: read-only rendering with a plain-language note. Offline or updates-disabled: the update strip reads `Update checks disabled by policy` with a link to REQ-078's policy screen.
- **Keyboard and mobile.** `g a` Appearance, `g t` Themes, `g w` Widgets, `⌘K` lists the appearance commands (`Activate theme…`, `Upload theme…`, `New widget area…`), `⌘/` opens the shortcut sheet, `Esc` closes the preview. Below 900 px: single-column gallery, card actions collapse into one sheet, the upload report becomes an accordion, and the area editor opens read-only with a note that editing needs a wider screen while `Preview` stays fully usable.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/appearance` | Landing payload: active theme, areas summary, pending updates, recent changes | `appearance.read` |
| GET | `/api/v1/themes` | Gallery data (bundled + installed, active flag) — REQ-062 route | `themes.read` |
| GET | `/api/v1/themes/{key}` | One theme: manifest, slots, modes, sample-content flag | `themes.read` |
| POST | `/api/v1/sites/{site_id}/theme` | Activate a theme for the site, keeping the previous choice | `themes.activate` |
| POST | `/api/v1/sites/{site_id}/theme/rollback` | Restore the previous theme and its published settings | `themes.activate` |
| POST | `/api/v1/themes/validate` | Dry-run validation of an uploaded package | `themes.install` |
| POST | `/api/v1/themes/install` | Install an uploaded package (multipart zip) | `themes.install` |
| DELETE | `/api/v1/themes/{key}` | Remove an uploaded theme (never bundled, never active) | `themes.install` |
| GET | `/api/v1/sites/{site_id}/widget-areas` | Areas with state, order, backing slot, block counts | `appearance.read` |
| POST | `/api/v1/sites/{site_id}/widget-areas` | Create an area (label, key, slot) | `appearance.widgets.manage` |
| GET · PUT | `/api/v1/sites/{site_id}/widget-areas/{key}` | Read · update label/slot/active state | `appearance.read` · `appearance.widgets.manage` |
| PUT | `/api/v1/sites/{site_id}/widget-areas/order` | Persist the area order | `appearance.widgets.manage` |
| POST | `/api/v1/sites/{site_id}/widget-areas/{key}/publish` | Publish the area's draft blocks (delegates to REQ-076) | `appearance.widgets.manage` |
| POST | `/api/v1/sites/{site_id}/widget-areas/{key}/reset` | Reset the area to the theme's default content | `appearance.widgets.manage` |
| DELETE | `/api/v1/sites/{site_id}/widget-areas/{key}` | Delete a custom area (slot-backed areas are deactivated instead) | `appearance.widgets.manage` |
| GET | `/api/v1/audit?action_prefix=themes.&action_prefix=appearance.` | Recent theme and appearance changes — REQ-039 route | `audit.read` |
| GET | `/api/v1/updates?kind=theme&key={key}` | Update availability for the active theme — REQ-078 route | `updates.read` |

Block payloads inside an area are read and written through REQ-062's `/sites/{site_id}/theme-layouts/{slot}` and REQ-076's `/theme-layouts/{slot}/publish`, so there is exactly one storage path. Validation errors are per area field: `area_label_required`, `area_key_format`, `area_key_taken`, `area_slot_unknown`, `area_remove_active_theme`, `package_manifest_invalid`, `package_slot_unknown`, `package_block_unknown`, `package_asset_rejected`.

### Data model

Migration: `0116_appearance_widget_areas.sql` (reserved band 0116–0127 for this group of requests; the ledger is append-only — take the next free number if one is already used).

```sql
widget_areas (
  id uuid primary key default gen_random_uuid(),
  site_id uuid not null references sites (id) on delete cascade,
  key text not null, label text not null, description text null,
  theme_slot text null,                      -- null = custom area with no theme default
  position integer not null default 0, is_active boolean not null default true,
  created_by uuid null references users (id) on delete set null,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  constraint widget_areas_key_format check (key ~ '^[a-z][a-z0-9_]{1,39}$'),
  constraint widget_areas_label_length check (length(btrim(label)) between 1 and 60),
  constraint widget_areas_site_key_key unique (site_id, key)
);
create index widget_areas_site_position_idx on widget_areas (site_id, position);
```

Blocks for an area live in the slot's layout: `theme_layouts` (REQ-062) holds the published payload, `theme_layout_versions` (REQ-076) holds drafts and history. A slot-backed area mirrors the theme manifest's declared slots; when the active theme changes, areas whose `theme_slot` no longer exists are flagged `orphaned` on read (computed, never stored) and the UI offers `Reset`, `Reassign to slot…`, or `Keep custom`.

New permission keys land in `crates/permissions/src/catalogue.rs` (single source of truth, seeded on boot): `appearance.read`, `appearance.widgets.manage`, and the theme keys this screen consumes — `themes.read`, `themes.activate`, `themes.customize`, `themes.export`, `themes.install`, `themes.update` — each with a category (`appearance` / `themes`) and a product-language description so REQ-068's catalogue screen lists them without extra work.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `appearance.widget_area.created` · `.updated` · `.removed` | Area metadata changes | `site_id`, `area_key`, `theme_slot`, `actor_user_id` |
| `appearance.widget_area.published` | Area draft published (also emitted by the builder path) | `site_id`, `area_key`, `revision_no` |
| `appearance.widget_areas.reordered` | Order persisted | `site_id`, `order[]` |
| `appearance.menu.rendered_without_permission` | A control was hidden for a caller lacking the key | never emitted to the bus — audit only, to keep the bus free of noise |

Consumed: `themes.theme.activated` (recompute area/slot mapping, surface orphaned areas), `themes.layout.saved` (refresh the area card's block count and last-edited line), `themes.package.installed` · `.removed` (gallery and upload report refresh), `media.deleted` (mark a branding asset missing instead of rendering a broken thumbnail), `updates.available` (light the update strip). Webhook relevance: none of these are integration-facing; they stay in the audit trail and the panel, and `appearance.*` events are internal by default (`internal` visibility) so an installation's outbound webhooks are not flooded by UI-housekeeping.

### Acceptance criteria

- [ ] `/appearance` renders the active-theme card for the site in the shell's switcher, and switching sites re-renders it with the other site's theme.
- [ ] The gallery lists every bundled theme with a distinct preview image, name, version and author, and marks exactly one theme `Active` per site.
- [ ] `Preview` opens the theme in the panel frame, device and mode switches change the rendered output, and `Activate on <site>` from the preview changes what a signed-out visitor sees within one refresh.
- [ ] `Restore previous` restores the previous theme *and* its published settings revision, verified by comparing the public page before and after.
- [ ] Uploading a valid package installs it inactive with an `Uploaded` tag; uploading a package with an unknown slot, an unknown block type or a file outside the asset allow-list shows a failing report and installs nothing.
- [ ] Zip-slip entries (`../` in a path) are rejected with a named finding, and the extractor writes nothing outside the theme directory.
- [ ] A read-only caller without `themes.activate` sees the gallery and the active-theme card with the activation control replaced by a `needs themes.activate` note, not a hidden button.
- [ ] `New area` rejects a duplicate key, a label over 60 characters and an unknown slot with field-level messages; the created area appears in order and is editable immediately.
- [ ] Reordering areas persists across a reload and the public site renders the areas in the new order.
- [ ] Area blocks saved as a draft are absent from the public render until `Publish`, and `Reset to theme default` restores the theme's shipped content.
- [ ] A slot-backed area cannot be deleted (only deactivated), a custom area can, and both rules hold for a caller holding `appearance.widgets.manage`.
- [ ] Changing the active theme flags areas whose slot no longer exists as `orphaned`, and `Reassign to slot…` clears the flag without losing the blocks.
- [ ] Bulk `Remove selected` skips themes active on any site and reports the per-row reason.
- [ ] Recent changes on the landing page list real theme/appearance audit rows with actor and time, and clicking one opens the Activity detail (REQ-080).
- [ ] The update strip reflects REQ-078's check result, and the screen never performs an update itself.
- [ ] The gallery, the area editor and the upload report are usable at 390 px without horizontal scroll, with the area editor read-only and the preview still working.
- [ ] The walkthrough covers `/appearance`, `/appearance/themes`, `/appearance/themes/upload`, `/appearance/active` and `/appearance/widgets` with zero high findings, and the visual check sees real preview images rather than placeholders.

### QA plan

Add `{ path: "/appearance", name: "appearance" }`, `/appearance/themes`, `/appearance/themes/upload` and `/appearance/widgets` to the `routes` array in `scripts/qa/walkthrough.cjs`, and enter the section from the sidebar. The walkthrough must: open `/appearance` and read the active-theme card; filter the gallery by `Uploaded` and by `Update available`, switch to list layout, select an inactive uploaded theme and remove it through the typed confirmation; open a bundled theme's preview, switch to phone and dark mode, activate it, then hit `Restore previous` and confirm the theme key returns to the earlier value; visit the upload screen and upload a fixture package built by the QA script (one valid, one with a bad manifest) and read both reports; open `/appearance/widgets`, create an area (including one deliberately invalid key to capture the inline error), drag it above another, toggle it inactive, open its editor, add a block, save the draft, publish, and reset it to the theme default. Visual check: the active-theme card is legible with a real thumbnail and a real activation line, gallery cards differ from each other, the upload report shows pass/warn/fail rows with readable messages, the widgets list shows handles, counts and state chips, and the public site reflects the activated theme, the published area and the restored state. The mobile pass must show the single-column gallery and the read-only area editor note instead of a broken canvas.

### Slices

1. **Appearance shell, landing page and gallery.** Sidebar group, `/appearance` with the active-theme card and recent changes, gallery grid/list with filters, the preview hand-off, activate and `Restore previous`, permission-aware rendering, walkthrough routes. *Done when:* acceptance 1–4, 7, 14 pass and the section is in the walkthrough inventory.
2. **Upload flow.** `/appearance/themes/upload` with drag-and-drop, the validation report (pass/warn/fail rows, zip-slip and asset checks surfaced), install-inactive behaviour, remove rules and bulk remove reporting. *Done when:* acceptance 5–6, 13 pass.
3. **Widgets / Blocks.** Migration `0116_appearance_widget_areas.sql`, areas list with ordering and state, area create/update/delete rules, the area editor on the REQ-063 canvas, publish and reset through the shared layout storage, orphan detection on theme change. *Done when:* acceptance 8–12 pass (depends on REQ-076 slice 1 for layout versions).
4. **States, mobile and polish.** Empty/loading/error/permission-limited states, offline and updates-disabled messaging, keyboard shortcuts and the `⌘K` entries, 390 px behaviour, and the event payloads verified by a subscribed endpoint. *Done when:* acceptance 15–17 pass, the walkthrough is green, and no new high findings appear.

### Risks / notes

- Two galleries are the failure mode this REQ must avoid: the appearance gallery and REQ-062's `/themes` share one card component and one endpoint. If they diverge, "the same theme shows differently in two places" becomes a permanent bug class.
- Widget areas deliberately own metadata only. If a second blocks column appears here, drafts, diffs and rendering will drift from the builder's; the shared layout storage is the contract.
- Theme changes invalidate slot mapping; orphaned areas are computed on read and shown as a decision (reset, reassign, keep) rather than silently dropped, because dropping a site's footer content on a theme switch is data loss in the user's eyes even when it is recoverable.
- Uploaded packages remain untrusted input: the report is a presentation of REQ-062's validation, not a second, weaker validator. Any check that exists only here is a bug.
- Installing must never imply activating, and removing a theme that is active on any site must stay impossible — including through bulk actions, which are the usual place where such rules leak.
- The landing page reads from three sources (themes, updates, audit); it degrades gracefully when one is unavailable (a service-down strip on that card) instead of failing the whole screen.
- Timezone, locale and copy: dates render in the installation's timezone with the viewer's locale, and Turkish sample copy belongs to theme sample content, never to labels or messages on these screens.
