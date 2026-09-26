# REQ-020 — Globalization

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/localization`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Beyond multi-language:

```text
Language
Locale
Timezone
Currency
Number format
Date format
Regional content
```

Example — the same site can serve different regions and currencies:

```text
Türkiye
EUR
TRY
USD
```

## Implementation spec

Multi-language already has a skeleton: `crates/content/src/translations.rs` and the `translations` table (resource, language, field, value — rows, never `title_tr` columns). This request turns
that skeleton into a working globalization layer:
an installation-level language registry, per-site locale and format settings, a translation coverage matrix and editor in the panel, locale-aware public routing with correct HTML and `hreflang`,
timezone-correct scheduling and RTL support for the panel shell and public themes.

### Scope (in / out)

**In**

- **New crate `crates/localization`**: language registry, BCP-47 validation, site locale settings, format helpers (number, date, time, currency, relative time), timezone resolution and RTL
  direction lookup.
- **Language registry** (installation level): install/enable/disable languages, native and English names, direction, fallback order.
- **Site locale settings**: default locale, enabled locales, timezone, currency, number format, date format, first day of week, currency display style, URL strategy (path prefix `/tr/`).
- **Translation coverage matrix** and a side-by-side translation editor with per-field drafts, state transitions (draft → ready → published), the "missing / outdated" computation, and CSV export
  of pending strings.
- **Locale-aware public surface**: `?locale=` and path-prefix resolution, localized slugs, `lang` and `dir` attributes, `hreflang` alternates (including `x-default`), per-locale fallback with a
  panel-visible marker, and locale-scoped caching keys.
- **Region-aware content**: the same page can carry per-locale overrides of title, body, summary and slug, and a site can present different currencies for different regions (display only).
- **Timezone correctness**: scheduled publishing is stored in UTC and displayed in the site timezone, and the editor shows the site-local time next to the UTC time.

**Out**

- **Machine translation.** No provider calls in this request; the AI translation adapter belongs to REQ-001 (AI Hub) and the Translation Memory to REQ-064. The editor ships no "Translate with AI"
  button rather than a disabled one.
- **Currency conversion.** No FX rates; currency is a display format for values that already carry an amount, and pricing itself belongs to the commerce module.
- **Non-Gregorian calendars, address formats, tax/legal localizations** (the business suite owns those), and subdomain-per-locale routing (path prefix only; documented as a later option).
- Panel UI translation of *third-party* plugin strings — plugin locales load through the plugin manifest (REQ-044), not through this request.

### Screens (UI)

- **`/settings/localization` — Languages.** Table columns: Language (English name), Native name, Code, Direction (ltr / rtl badge), Enabled (toggle), Sites using it, Coverage (average across
  sites), Actions. `Add language` opens a searchable picker of common codes plus a custom field validated as `^[a-z]{2,3}(-[A-Z][a-z]{3})?$`; adding fills English and native names from the built-in
  table and lets the operator correct them. Fallback order is a drag-reorder list with a cycle guard (the chain must terminate and must include the site default). Disabling a language used by
  content is refused with a message naming the affected sites;
  deleting is only offered for languages with zero translation rows.
- **`/sites/[id]/settings/localization` — Site locale settings.** Fields: Default locale (select from enabled languages), Enabled locales (checkbox list showing per-locale coverage badge),
  Timezone (searchable IANA list), Currency (ISO 4217 searchable, default from the installation), Number format (Auto / Custom pattern) with a **live preview line** (`1.234.567,89` vs
  `1,234,567.89`), Date format (Auto / Custom) with a live preview (`31.12.2026`, `2026-12-31`, `Dec 31, 2026`), Time format (24 h / 12 h), First day of week, Currency display (Symbol / Code /
  Name) with the preview updating (`₺1.234,56`, `TRY 1.234,56`, `1.234,56 Turkish lira`), URL strategy (Path prefix — selected; Subdomain — visibly not available yet and explained, not a dead
  control). Saving validates:
  default locale must be enabled, enabled list must contain the default, timezone must resolve, currency must be a known code.
- **`/content/translations` — Coverage matrix.** Header stats: overall coverage %, missing, outdated, locales count. Table: rows are pages (sticky first column, title + slug), columns are the
  site's enabled locales, each cell a state button — `missing` (dashed outline), `draft` (grey badge), `ready` (amber badge), `published` (green badge), `outdated` (amber badge with a dot, meaning
  the default-locale revision changed after the translation was published). Clicking a cell opens the editor for that page and locale. Filters: site, locale, state, `missing only`, text search.
  Bulk:
  `Mark for translation` on selected rows creates a pending list for that locale plus `Export missing (CSV)`.
- **Translation editor (`/pages/[id]/translations/[locale]`).** Two columns: source (default-locale revision, rendered read-only) and target (Locale switcher, fields Title, Slug, Summary, Body
  with a plain-text area until the block editor arrives, `Copy from source` per field, per-field character counters, per-field autosave after 2 s of idle with a visible saved/unsaved indicator).
  Footer:
state selector (draft → ready → published) with `Publish translation` gated by `content.pages.publish`, `Preview this locale` (hands off to REQ-018 with the locale set), and a note explaining
that missing fields fall back to the default locale.
- **Panel language and direction.** The header user menu gains a `Panel language` switcher (English, Türkçe, العربية as the RTL demo).
Selecting an RTL language flips the shell (`dir="rtl"`, mirrored sidebar, mirrored icons where direction matters) while content previews keep the *site's* direction, which is shown next to the
panel direction so the two are never confused.
- **States.** Loading: matrix skeleton with the header stats shimmering. Empty: a site with one locale shows an explainer card with `Enable more languages`. Error: a failed save keeps the field
  marked dirty with the error message inline; a matrix load failure shows a retry banner. Mobile:
  the matrix scrolls horizontally with a sticky first column, the editor stacks source above target with a toggle, and the site settings form becomes a single scrolling page.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/localization/languages` | Installed languages, enable state, fallback order | `localization.read` |
| POST | `/api/v1/localization/languages` | Install a language (code, names, direction) | `localization.manage` |
| PATCH | `/api/v1/localization/languages/{code}` | Rename, enable/disable, set direction, reorder fallbacks | `localization.manage` |
| DELETE | `/api/v1/localization/languages/{code}` | Remove a language with zero translations | `localization.manage` |
| GET | `/api/v1/sites/{id}/localization` | Site locale settings and resolved formats | `sites.read` |
| PUT | `/api/v1/sites/{id}/localization` | Update default locale, enabled locales, timezone, currency, formats, URL strategy | `sites.update` |
| GET | `/api/v1/sites/{id}/translations/coverage` | Matrix rows with per-locale state, `limit`/`cursor`, filters | `content.pages.read` |
| GET | `/api/v1/pages/{id}/translations` | All locales of one page | `content.pages.read` |
| PUT | `/api/v1/pages/{id}/translations/{locale}` | Upsert title, slug, summary, body for one locale | `content.pages.update` |
| POST | `/api/v1/pages/{id}/translations/{locale}/publish` | Publish that locale's translation | `content.pages.publish` |
| POST | `/api/v1/pages/{id}/translations/{locale}/unpublish` | Withdraw a published translation | `content.pages.publish` |
| DELETE | `/api/v1/pages/{id}/translations/{locale}` | Delete one locale's translation | `content.pages.delete` |
| GET | `/api/v1/public/locales` | Public locale list of the addressed site: default, enabled, prefixes, direction | public (no permission) |
| GET | `/api/v1/public/pages/{slug}` | Existing endpoint extended with `locale`, localized slug resolution and `alternates` | public (no permission) |

`localization.read` and `localization.manage` are added to the permission catalogue (Owner and Administrator by default; Editor receives `localization.read`). Permission checks run before
validation, and a site outside the caller's organization answers `404`.

### Data model

Migration `0015_globalization.sql` (number is a placeholder — renumber to the next free slot):

- `languages` — `code text primary key check (code ~ '^[a-z]{2,3}(-[A-Z][a-z]{3})?$')`, `name_en text not null`, `name_native text not null`, `direction text not null default 'ltr' check (direction in ('ltr','rtl'))`,
  `enabled boolean not null default true`, `fallback_rank integer not null default 100 check (fallback_rank >= 0)`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`.
  Index `(enabled, fallback_rank)`. Seeded with `en` (rank 0).
- `site_localization` — `site_id uuid primary key references sites(id) on delete cascade`, `default_locale text not null references languages(code)`, `enabled_locales text[] not null check (cardinality(enabled_locales) between 1 and 24)`,
  `timezone text not null default 'UTC' check (timezone ~ '^[A-Za-z][A-Za-z_+-]*(/[A-Za-z][A-Za-z_+-]*)*$')`, `currency text not null default 'USD' check (currency ~ '^[A-Z]{3}$')`, `number_format text`,
  `date_format text`, `time_format text not null default '24h' check (time_format in ('24h','12h'))`, `first_day_of_week smallint not null default 1 check (first_day_of_week between 0 and 6)`,
  `currency_display text not null default 'symbol' check (currency_display in ('symbol','code', 'name'))`, `url_strategy text not null default 'path_prefix' check (url_strategy in ('path_prefix'))`,
  `updated_at timestamptz not null default now()`, plus the row-level invariant `check (default_locale = any (enabled_locales))`. Every site gets a row in the same migration (defaults:
  `en`, site's existing language or `en`, `UTC`, `USD`).
- `translations` (existing table, 0004) gains `state text not null default 'draft' check (state in ('draft','ready','published'))`, `published_at timestamptz` and `published_by uuid references users(id) on delete set null`;
  the existing unique `(resource_type, resource_id, language, field)` becomes the upsert key. New indexes:
  `(organization_id, language, state)` for coverage queries and `(resource_type, resource_id, language)` for page-level reads.
- Coverage is computed, not stored: `missing` = no rows for that locale; `outdated` = the translation's `published_at` is older than the page's published revision `published_at`.
  A materialized count per site/locale is cached for 60 seconds in Redis to keep the matrix fast on large sites, and the matrix itself is cursor-paginated.
- Localized slugs reuse `field = 'slug'` rows, so `pages.slug` stays the default-locale address and no schema change is needed; a unique partial guarantee is enforced in application code plus a
  unique index on `(resource_type, resource_id, language, field)` (already present).

### Events

- **Emitted:** `content.translation.updated`, `content.translation.published`, `content.translation.unpublished`, `content.translation.deleted` (each with page id, locale, fields touched),
  `localization.language.enabled`, `localization.language.disabled`, `site.localization.updated` (with the changed keys, never whole settings blobs).
- **Consumed:** `page.published` marks translations for the default locale as a comparison baseline for the `outdated` state; nothing else.
- **Webhook relevance:** headless frontends and CDNs subscribe to `content.translation.*` to rebuild one locale (`/tr/…`) instead of the whole site, and to `site.localization.updated` to
  invalidate locale-aware caches. Payloads carry ids, locale codes and field names — not translated text.

### Acceptance criteria

- [ ] `crates/localization` builds with unit tests covering BCP-47 validation, RTL lookup, number, date and currency formatting for `tr-TR`, `de-DE` and `en-US`, and fallback resolution.
- [ ] Installing `tr` and `ar` in `/settings/localization` enables them, and `ar` reports `direction = rtl`.
- [ ] Setting the site default to `tr` with enabled locales `[tr, en]` persists, and a default that is not in the enabled list is refused with a field-level error.
- [ ] The format previews update live and show `1.234.567,89` and `31.12.2026` for `tr-TR` and `1,234,567.89` / `12/31/2026` for `en-US`.
- [ ] Enabling a locale that has no content does not break the public site: missing translations fall back to the default locale, and the panel marks the fallback in the editor.
- [ ] `PUT /api/v1/pages/{id}/translations/tr` saves title, slug, summary and body, and a second read returns exactly those values.
- [ ] The coverage matrix shows `missing` before the save, `draft` after it, `ready` after the state change and `published` after publishing; editing the default-locale revision afterwards flips
  the cell to `outdated`.
- [ ] The matrix filters (`site`, `locale`, `state`, `missing only`, search) each narrow the rows and the header stats match the filtered counts.
- [ ] `Export missing (CSV)` downloads a file with one row per page/locale pair and opens cleanly in a spreadsheet.
- [ ] Publishing a translation records `content.translation.published` and delivers it to a subscribed endpoint.
- [ ] `GET /api/v1/public/pages/{slug}?locale=tr-TR` returns the Turkish title, body and localized slug, plus `alternates` containing one entry per enabled locale and an `x-default`.
- [ ] Opening the localized slug on the public renderer (path prefix `/tr/{slug}`) resolves the same page, sets `lang="tr"` and a correct `dir`, and emits `hreflang` alternates in the head.
- [ ] An unknown locale parameter falls back to the site default instead of answering `404`, and the response says which locale actually served.
- [ ] Scheduled publishing displays the site-timezone time in the editor next to the UTC time and fires at the correct UTC instant (proven with a short-interval test).
- [ ] Selecting `Türkçe` and then `العربية` in the panel language switcher flips the shell to RTL (sidebar mirrored, logical properties correct) while a preview keeps the site's own direction.
- [ ] Disabling a language that has translations is refused with a message naming the affected sites; deleting a language with zero translations succeeds.
- [ ] QA walkthrough covers `/settings/localization`, `/sites/[id]/settings/localization`, `/content/translations` and `/pages/[id]/translations/[locale]` with zero high findings.

### QA plan

The walkthrough installs `tr` and `ar`, sets a site's default locale to `tr` with `[tr, en]` enabled, changes the number, date and currency previews, then opens the coverage matrix (all cells
`missing`), translates one seeded page into Turkish in the editor (title, slug and body), marks it ready and publishes it, and verifies the cell turns green. It then edits the default-locale
revision and watches the same cell become `outdated`, exports the missing CSV, and opens the public page with `?locale=tr-TR` and on the `/tr/` path to confirm the localized title, the `lang`
attribute and the `hreflang` links. It ends on mobile width (390 px) with the matrix scrolled horizontally and the editor stacked. The visual check must see:
real Turkish strings rendered in the panel examples (never raw keys), a matrix with distinguishable state styling, previews that visibly change when the format selectors change, RTL layout that
mirrors correctly, and no disabled controls or placeholder-only screens.

### Slices

1. **Localization crate + settings screens.** `crates/localization` (validation, formats, direction, fallback), migration, `languages` and `site_localization` endpoints, `/settings/localization`
and the site localization form with live previews.
*Done line:* languages can be installed and a site's locale, timezone, currency and formats save with live previews that match the chosen locale.
2. **Public locale awareness.** `?locale=`, path-prefix routing in `apps/web`, localized slug resolution, `lang`/`dir`/`hreflang`, fallback behaviour and locale-aware cache keys.
*Done line:* the same page renders in Turkish on `/tr/{slug}` with correct `lang` and `hreflang`, and an unknown locale falls back with an explicit note in the response.
3. **Translation matrix + editor.** Coverage endpoint with caching, `/content/translations`, editor with per-field autosave, state transitions, publish/unpublish, CSV export, translation events.
*Done line:* a page can be translated and published for one locale end to end, and the matrix shows `missing → draft → ready → published → outdated` as the flow proceeds.
4. **Direction + panel language.** Panel locale switcher, RTL shell mirroring, logical CSS properties in `@omnion/ui`, site direction respected inside previews.
*Done line:* switching the panel to an RTL language mirrors the shell without breaking the sidebar, tables or dialogs, and content previews keep the site's direction.

### Risks / notes

- The matrix is the most expensive screen here: always cursor-paginated, counts cached with a short TTL, and a large site must never load every page to render the header stats.
- Fallback must terminate: enabled locales must contain the default, and the fallback chain is validated for cycles on every write; a request for a locale with no data falls back exactly one
  level at a time and reports which locale served.
- Timezone data is authoritative on the server (`time`/ICU-backed); the panel never computes publication instants client-side, and scheduled publishing stores UTC only.
- Currency is a display concern in this request; the moment pricing or payments appear (commerce module), amounts must carry their own currency and the site currency becomes a default, not a
  truth.
- RTL scope is honest: the panel shell and the public theme's layout primitives support RTL; themes that hard-code left/right get a documented checklist and a visual QA item rather than a
  promise.
- No machine translation ships here — the editor's help text says translations are written by hand or imported, and the AI adapter is tracked by REQ-001.
- Locale-aware caching must include the locale (and the resolved locale) in the cache key, otherwise a CDN happily serves Turkish pages to English visitors.
