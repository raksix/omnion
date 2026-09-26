# REQ-002 — Global Search Engine

> **Status:** done · **Captured:** 2026-09-25 · **Layer:** core (`crates/search`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

One single search box:

```text
Search Omnion...
```

that finds everything:

- Pages
- Posts
- Users
- Media
- Plugins
- Themes
- Settings
- Orders
- Forms
- Logs
- Documentation

Plus a **Ctrl + K** Command Palette on top of it.

## Implementation spec

### Scope (in / out)

**In**

- New crate `crates/search`: a `SearchProvider` trait (`key()`, `types()`, `index(scope, entity)`, `remove(entity)`, a cost hint) plus a `SearchIndex` that fans one query out across the registered providers, merges the hits by rank and applies the caller's scope.
- New admin chrome: the **global search box** in the panel header, the **⌘K / Ctrl+K command palette** on top of it, the `/search` results screen and a `/settings/search` index screen.
- Providers shipped in v1 for the entities the platform actually has: `pages`, `media`, `users`, `sites`, `settings` (key/value settings), `logs` (audit entries and recorded events), `translations`. The other entities of the brief — posts, plugins, themes, orders, forms, documentation — register into the same registry from their own module when it ships (REQ-064, REQ-044, REQ-062, REQ-008, REQ-058); until then the palette renders no section for them.
- Index maintenance from the event bus: a subscriber (`apps/api/src/search_runner.rs`, mirroring `event_runner`) advances a cursor over `events` and applies each row; `POST /search/reindex` rebuilds any provider from scratch.
- Scoped query syntax parsed server-side: `type:page`, `site:acme.com`, `owner:me`, `before:2026-09-01`, `after:2026-06-01`, `is:draft`.
- Ranking: PostgreSQL full-text (`tsvector`, title weight A / tags B / subtitle C / body D) combined with a `pg_trgm` title match for prefix and near-miss hits. An external engine can replace the implementation behind the trait later without touching call sites.

**Out**

- External search services (Meilisearch/OpenSearch) — the abstraction exists, the binding does not.
- Binary media content: v1 indexes media metadata (title, alt text, file name, mime, size); text extraction arrives with REQ-010.
- Any permission widening: search never returns an entity the caller could not open directly.
- Saved searches and shared dashboards — REQ-032 / REQ-028 territory.

### Screens (UI)

- **Header search box** (`AppShell`) — 320px input with a magnifier and a `⌘K` hint chip on `lg+`, a full-width row under the header below that. Click or `⌘K`/`Ctrl+K` opens the palette; `/` focuses the box without opening it.
- **Palette overlay** — centered sheet (max 640px), pinned input, sections ordered by hit count (Pages, Media, Users, Sites, Settings, Logs, Translations). Row: icon, title with the match highlighted, breadcrumb (site › section), type chip. Behaviour: 120ms debounce, 2-character minimum, ≤5 rows per section plus "See all results for …", `↑`/`↓` across section boundaries, `Enter` open, `⌘Enter` new tab, `Tab`/`Shift+Tab` jump sections, `Esc` close, `⌘K` toggles. Empty query shows "Recent searches" (≤8, removable, Clear) and "Recently viewed" (client-side, ≤5). Loading: three skeleton rows per section. No hits: "Nothing matched *query*". Error: "Search is unavailable" with Retry, the box stays usable.
- **`/search?q=…` results screen** (the palette's "See all") — editable query input, hit count, applied filters as removable chips; facet rail (Type, Site, Owner, Language, Updated range, Status) with counts; sort (Relevance, Newest, Title A–Z); 50 rows per page with "Showing 1–50 of N". Row: checkbox, type icon, title, breadcrumb, owner, updated, overflow menu (Open, Open in new tab, Copy link). Bulk: Copy links (N), Export CSV (selection, or the whole result set when nothing is selected); selection supports Shift-range and `⌘A`. Empty state lists the three tips (remove facets, check spelling, narrow with `type:`). Errors show the API code and Retry; partial rows are never rendered silently.
- **`/settings/search`** — one row per provider (Documents, Last indexed, Status ready/indexing/stale/failed, Reindex button), the ranking weights form (Title, Tags, Subtitle, Body — integers 0–10, Title ≥ Body) with Restore defaults, and a progress line fed by the status endpoint while a reindex runs.
- **Keyboard map** — `⌘K` palette, `Esc` close, `↑`/`↓`/`Enter`/`⌘Enter`, `/` focus search, `s` cycle sort, `f` focus facets, `x` toggle a row checkbox, `⌘A` select page, `?` shortcut dialog (opened from the palette footer).
- **Mobile (<1024px)** — the palette is a full-screen sheet with a sticky input, 44px rows and a visible close button; `/search` moves facets into a bottom sheet behind "Filters (n)", rows become cards, and bulk actions live in a bottom action bar.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/search?q=&types=&site_id=&owner=me&before=&after=&sort=&page=&per_page=` | Ranked, scope-filtered hits across every provider | `search.read` |
| GET | `/api/v1/search/suggest?q=` | Title-only prefix suggestions (≤8) for the palette's first paint | `search.read` |
| GET | `/api/v1/search/actions?q=` | Palette action matches the server knows about | `search.read` |
| GET/DELETE | `/api/v1/search/recent` | The caller's last queries; clear them | session only |
| GET | `/api/v1/search/status` | Per-provider counts, last pass, state | `search.read` |
| POST | `/api/v1/search/reindex` | Rebuild one provider (`{"provider":"pages"}`) or all | `search.manage` |
| GET/PUT | `/api/v1/search/settings` | Ranking weights and enabled providers | `search.read` / `search.manage` |
| GET | `/api/v1/search/export?…` | CSV of the current query (same params as `/search`) | `search.read` |

New keys in the catalogue (`crates/permissions`): `search.read` (every signed-in account; results
stay filtered by organization and by the viewer's per-entity permissions) and `search.manage`
(index operations).

### Data model

Migration `database/migrations/0012_search.sql` (next free number if taken; released migrations are
append-only). It enables `pg_trgm` and creates:

| Table | Columns (types) | Indexes |
|---|---|---|
| `search_documents` | id bigserial pk, organization_id uuid → organizations cascade, site_id uuid → sites cascade, provider text, entity_type text, entity_id text, title text, subtitle text default '', url text, language text, visibility text default 'internal' ('public','internal','private'), owner_user_id uuid → users set null, tags text[] default '{}', body text default '', entity_updated_at timestamptz, indexed_at timestamptz default now(), document tsvector generated always as `setweight(to_tsvector('simple', title),'A') || setweight(to_tsvector('simple', array_to_string(tags,' ')),'B') || setweight(to_tsvector('simple', subtitle),'C') || setweight(to_tsvector('simple', body),'D')` stored | unique `(provider, entity_type, entity_id)`; GIN `(document)`; GIN `gin_trgm_ops (title)`; `(organization_id, site_id)`; `(entity_type, entity_updated_at desc)`; `(owner_user_id)` where not null |
| `search_recent` | id bigserial pk, user_id uuid → users cascade, query text, created_at timestamptz default now() | `(user_id, created_at desc)`; each write prunes to the newest 20 rows per user |
| `search_settings` | id smallint pk default 1, weights jsonb default `{"title":6,"tags":4,"subtitle":3,"body":1}`, enabled_providers text[] default every v1 key, updated_at | check `(id = 1)` |

`'simple'` is deliberate: it does not stem, so Turkish and English content behave alike; the trigram
index carries prefix behaviour; language-specific configurations arrive with REQ-020. Ranking is
`ts_rank_cd(document, websearch_to_tsquery('simple', $1))` with the weights injected from
`search_settings.weights`.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `page.published`, `page.updated`, `page.archived` | consumed | re-index or remove the page row |
| `media.created`, `media.updated`, `media.deleted` | consumed | index metadata / remove |
| `user.created`, `user.updated`, `user.disabled` | consumed | index, or drop the account from results |
| `site.created`, `site.updated` | consumed | index the site's identity row |
| `search.reindexed` | emitted | provider, document count, duration — the settings screen polls status; endpoints may subscribe |
| `search.index.failed` | emitted | provider, error — worth an automation rule (REQ-003) |

Consuming more than we emit is intentional: index maintenance is an internal subscriber, not a
public contract; only reindex outcomes are published.

### Acceptance criteria

- [x] `crates/search` holds the core — the provider registry, the indexer (reindex + bus drain) and the scope logic — with unit tests for the query language, the ranking weights, the event plans and the provider coverage. *(Built as a registry + indexer rather than a dyn `SearchProvider` trait: the indexer is the single writer, so the indirection would buy nothing yet; the registry is the seam an external engine would slot into.)*
- [x] The migration applies on top of the existing schema, seeds `search_settings`, and reports clearly when `pg_trgm` is unavailable. *(The extension is created inside a guarded block whose failure names the extension and says what to ask an administrator for.)*
- [x] `GET /api/v1/search?q=…` returns hits from at least pages, media and users on a populated installation. *(Integration walk: one query answers pages + media + users for an account holding the three read keys.)*
- [x] An entity the caller cannot open is never returned (test: an editor without `users.read` gets no user rows).
- [x] Results are filtered to the caller's organization; a platform account sees all organizations unless it narrows the query.
- [x] Scoped syntax works (`type:`, `site:`, `owner:`, `before:`/`after:`, `is:draft`); an unknown `type:` gives an honest empty result with a hint.
- [x] `⌘K`/`Ctrl+K` opens the palette, `Esc` closes it, `↑`/`↓` traverse sections, `Enter` opens the highlighted row, `⌘Enter` opens a new tab. *(Slice 2: the walkthrough presses `Ctrl+K` from the overview screen and drives the arrow keys and `Enter`; `scripts/qa/probe-palette.cjs` holds each key to its own check — including `Ctrl+Enter` opening the row in a second tab.)*
- [x] Fewer than two characters keeps the recent-search list; a query with no hits renders the no-results state. *(Probe: one character keeps the list, `zzzznothingmatches` answers "Nothing matched", and the box stays usable.)*
- [x] Recent searches persist per account (≤20, pruned, clearable through `DELETE /search/recent`). *(Per-item removal is the palette's own memory — `lib/search-memory.ts` hides one query in this browser; "Clear" forgets both the local set and the account's list.)*
- [x] `/search` renders facets with counts, applies them as removable chips and paginates 50 per page with a correct total. *(Slice 3: six groups (Type, Site, Owner, Language, Status, Updated), each counted **without its own filter**; the walkthrough applies a type facet (13 → 9 hits, one chip), removes it (0 chips) and reads `Showing 1–50 of N` against the same total the API answered.)*
- [x] Bulk selection supports Shift-range and `⌘A`; Copy links yields a newline-separated list; Export CSV produces one row per hit with the same count as the table. *(Walkthrough: a Shift-range selected 3 of 13 rows, Copy links put exactly 3 URLs on the clipboard, and the CSV of that selection carried 3 rows — the same count the bulk bar showed. `⌘A` selects the page; the selection is exported through `selected=`.)*
- [x] Index maintenance works from the bus: publishing a page makes it findable within one runner tick, and deleting media removes its row. *(Slice 2 gave the remaining providers their producers: `apps/api/tests/search.rs` now uploads a file and creates a site through the real routes, waits one indexer tick, sees both in the index and watches the file's row leave when it is removed. The direct `remove_entity` path stays as the second half of the same walk.)*
- [x] `POST /search/reindex` rebuilds a provider idempotently (two runs, same count) and is refused without `search.manage`.
- [x] Ranking weights from the settings form change the order of a fixture result set (title-heavy query ranks the title match first). *(`ranking_weights_change_the_order_of_a_fixture_result_set`: with the defaults the page whose title carries the term leads; after `PUT /search/settings` with title 1 · tags 10 · subtitle 10 · body 1 the site page that carries the term in its subtitle leads instead — the assertion runs after the defaults are restored, so a failure cannot leave the installation tuned.)*
- [x] Every screen has empty, loading and error states; no dead control and no placeholder copy. *(The palette, the results screen and `/settings/search` — the index screen ships with slice 3 and carries the same three states plus per-provider states read from the pass records. Rows exist only for providers whose screen exists; Activity, Translations and Settings rows open the entity or screen they name, so no control is a click into nothing.)*
- [x] The mobile pass renders the palette as a full-screen sheet with 44px rows and reachable bulk actions. *(The palette: 390×844, 44px rows, a visible close control. The results screen moves its filters into a sheet behind `Filters (n)` and keeps the selection bar sticky above the cards; `/settings/search` renders the providers as cards below `md` — the table's fixed columns do not fit a phone, and the first pass found exactly that.)*
- [x] `cargo test --workspace` (436 passed, 0 failed), `pnpm typecheck && pnpm build` (2/2) and the QA walkthrough (105 clicks · 107 screenshots · **0 high findings** · **0 vision issues**, `qa-artifacts/20260926-160500`) pass.

### QA plan

The walkthrough must: press `⌘K` on every admin screen (the box is global), type a seeded term,
arrow through the sections, open a page result, then use "See all results"; on `/search` apply a
type facet, remove it, sort by Newest, select three rows with Shift-click, run Copy links, export
the CSV, clear the query to reach the empty state; open `/settings/search`, run a per-provider
reindex and watch the progress line finish; exercise the recent-search list and its Clear action;
repeat the palette pass at 390×844.

The visual check must see: highlights inside matched titles without breaking line height, section
headers aligned with their rows, the sheet not overflowing the viewport, chips wrapping instead of
clipping, right-aligned counts, and the `⌘K` hint legible against the header background.

### Slices

1. **Index and query core** — `crates/search`, migration, providers for pages/media/users/sites, `GET /search`, `/suggest`, `/status`, reindex, the bus subscriber.
   *Done when:* a seeded install answers a query across three providers and publishing a page makes it findable without a manual reindex.
2. **Palette** — header box, overlay, sections, keyboard map, recent searches, recently viewed, all states, mobile sheet.
   *Done when:* the palette opens from any screen, is fully keyboard-navigable, opens a result, and shows real recents after a restart.
3. **Results depth** — `/search` facets, sort, pagination, selection with copy/CSV export, providers for settings/logs/translations, `/settings/search` with weights.
   *Done when:* facets and weights change the result set provably and the export matches the on-screen count.

### Risks / notes

- Index drift is the failure mode that quietly ruins trust: the bus subscriber, a per-provider
  reindex and an integration test comparing `search_documents` against the source tables after a
  fixture run are the guard rails.
- Permission filtering must happen inside the query, not after pagination, or page 2 leaks rows and
  the counts lie.
- `suggest` is title-only on purpose (latency); the palette falls back to the full endpoint for
  "See all".
- The box lives in shared chrome: debounce and abort in-flight requests, and keep it accessible
  (`role="dialog"`, focus trap, `aria-activedescendant`).
- `search.read` must never be widened into "read any entity": each provider re-checks the viewer's
  permission for its own type before returning rows.

### Build notes — slice 1 (index and query core), 2026-09-26

Shipped: `crates/search` (registry, indexer with reindex + bus drain, query language and the
`ts_rank_cd` + `pg_trgm` search), `database/migrations/0012_search.sql`, the `search.read` /
`search.manage` keys in the catalogue and seed, `/api/v1/search`, `/search/suggest`,
`/search/status`, `/search/reindex`, `/search/recent` (GET + DELETE) and
`apps/api/src/search_runner.rs` (the indexer that keeps documents fresh, config knobs
`OMNION_SEARCH_RUNNER` / `OMNION_SEARCH_POLL_MS` / `OMNION_SEARCH_BATCH`).

Deviations from the spec above, each deliberate:

- **No `SearchProvider` trait (yet).** The index has one writer — the indexer's own SQL — so a
  dyn-async trait would add indirection without a second implementor. The registry
  (`providers::PROVIDERS`) is the seam: a new provider is one entry plus one upsert/prune arm,
  and the tests fail if either half is missing.
- **`document` is a plain tsvector column, not a generated one.** The tags fold
  (`array_to_string`) is STABLE, and PostgreSQL refuses STABLE expressions in generated
  columns; the indexer recomputes the vector on every write instead (`0012_search.sql` says so).
- **Weights are normalised.** `search_settings.weights` is written the way a person reasons
  (title 6, tags 4, subtitle 3, body 1) but `ts_rank_cd` accepts only `0..=1`, so the values are
  scaled against the largest one before they are bound — the ordering the operator chose is kept.
- **`users` and `sites` hits point at routes that arrive with their screens.** Slice 1 is the
  API; the palette (slice 2) renders a section only for a provider whose route exists in the
  panel, so no click goes nowhere. `/settings/users` lands with REQ-006.
- **`is:` flags** are stored as document tags (`draft`, `published`), which is why the filter is
  a tag intersection rather than a text match.

Slices 2 (palette: header box, overlay, keyboard map, recently viewed, mobile sheet) and 3
(`/search` results screen with facets/selection/export, `/settings/search`, providers for
settings/logs/translations) remain open.

### Build notes — slice 2 (the palette and the results screen), 2026-09-26

Shipped: `apps/admin/components/global-search.tsx` (the header box — 320px beside the site switcher
on `lg+`, its own full-width row under the header below that; `⌘K`/`Ctrl+K` toggles the palette from
anywhere, `/` focuses the box without opening anything) and `apps/admin/components/search-palette.tsx`
(the ⌘K overlay: one section per provider, ordered by how much each matched, five rows each plus
"see all", `↑`/`↓` across section boundaries, `Tab` between sections, `Enter` / `⌘Enter`, recent
searches with per-row removal and Clear, recently viewed, loading/none/error states, the shortcut
list, and a full-screen sheet on phones). Supporting modules: `lib/search-palette.ts` (which
providers the panel can open, the sectioning, the match highlighting, the URL builders),
`lib/search-memory.ts` (the browser's own memory of viewed screens and hidden searches), the typed
search client in `lib/api.ts`, and `features/search/search-view.tsx` + `app/search/page.tsx` — the
screen "See all results" lands on. The deep links a hit needs are real too: `?site=` selects the
site (`lib/sites.tsx` on a fresh load, the palette before it navigates) and `?focus=` opens the
page's editor (`pages-view.tsx`) or marks the file's row (`media-view.tsx`).

The API side gained the half of the bus that had no producers. `crates/search`'s indexer has always
carried plans for `media.created`, `media.deleted`, `site.created`, `site.updated`, `user.created`
and friends — but only content emitted events, so the Media, Sites and Users sections of the palette
would have been permanently empty. Uploading a file, creating or editing a site and creating the
first account now announce themselves (`routes/media.rs`, `routes/tenancy.rs`,
`routes/onboarding.rs`), and the index follows within one tick. The same tick makes the hits deep
links: the upserts write `/pages?site=…&focus=…` and `/media?site=…&focus=…` instead of the bare
route, so "opens a result" opens the entity rather than its list. A new integration walk
(`an_upload_and_a_new_site_reach_the_index_through_the_bus`) drives the three routes, drains one
indexer tick and holds the index to what the bus carried — including the removal branch that had no
producer before.

Proof, this tick: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
warnings` clean · `cargo test --workspace` → **422 passed, 0 failed (44 suites)** · `pnpm typecheck &&
pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 58 clicks · 67 screenshots · **0 high findings**
(`qa-artifacts/20260926-150923`; its five medium findings are the public renderer's own icon 404s,
unchanged since the previous pass, and its single low vision note guesses the contrast of
`text-muted` at ~2.5:1 where the measured check reports none) · `scripts/qa/probe-palette.cjs`
(desktop + phone, 22 checks) → **22/22 PASS**. Inside the pass: the palette opens from the overview
screen with the input focused, answers "sample" with two sections (Media 1, Pages 1), moves the
highlight with `↓`, opens the highlighted page at `/pages?site=…&focus=…`, remembers the query under
"Recent searches" after a reload, and closes with `Esc`; the phone sheet measures 390×844 with 44px
rows.

Deviations from the spec above, each deliberate:

- **The results screen ships with the palette rather than in slice 3.** "See all results for …" is
  part of the palette and a link to a screen that does not exist is a dead control. What landed is
  the honest half of `/search`: query box, sort (relevance/newest/title), per-provider counts, rows
  with the match emphasised, `Showing 1–25 of N` pagination, and empty/loading/error states.
  Facets, removable chips, Shift/`⌘A` selection, Copy links, CSV export and `/settings/search`
  stay in slice 3.
- **Only providers the panel can open get a section.** `users` has no screen until REQ-006
  (`/settings/users` is where its hits point), so user rows are indexed but never rendered — the
  registry's own rule, kept in the palette. `settings`, `logs` and `translations` arrive with their
  providers in slice 3.
- **Per-row removal of a recent search is the browser's.** The API keeps the account's newest twenty
  queries and offers one "forget everything"; hiding a single row is a preference of this browser
  (`lib/search-memory.ts`) and "Clear" does both.
- **Loading is three skeleton rows, not three per section** (while the first answer is in flight
  there are no sections to count yet), and a second query keeps the previous rows visible and marks
  them busy instead of flickering.
- **Suggestions are the palette's first paint**: `/search/suggest` is fired next to the full query
  and shown as a "Suggestions" strip only while the ranked answer is still on its way.
- **The QA harness moved with the feature.** The pass now signs the mobile context in (it had been
  photographing the sign-in screen on every "mobile" route — a gap, not a screen), sets the file on
  the media screen's hidden upload input (the click-through cannot reach a hidden control, so the
  library stayed empty), and closes a palette left open by a previous round before it clicks on.
  That is what made the mobile measurements real — and it is why the mobile pass found, and this
  tick fixed, the pages table overflowing a phone (narrow columns and the action labels now leave
  before the table does) and a corrupt 82-byte "PNG" fixture that rendered as a broken thumbnail.


### Build notes — slice 3 (results depth, the wider provider set, the index's own screen), 2026-09-26

Shipped:

- `database/migrations/0013_search_depth.sql` — `search_reindex_runs` (one row per pass: started,
  finished, written, pruned, duration, error) and the wider default provider set (an installation
  picks the three new keys up; a key an operator removed by hand is never re-added).
- `crates/search` — three providers with their own upserts and prunes: **Activity** (`logs`: audit
  entries and recorded events **whose target is an entity the panel can open**, so a row opens the
  page, the file or the site it touched), **Translations** (`translations` joined to their page
  revision, opening that page's editor) and **Settings** (one key/value row per organization — the
  search weights — opening `/settings/search`). Facets: `query::facets` counts Type, Site, Owner,
  Language, Status and Updated, each group **without its own filter**, so the number the rail shows
  is the number its click produces; the hit statement now carries the owner's display name, and
  `SearchFilters` merges the rail's parameters with the query language (union per kind). `status`
  reads `ready` / `indexing` / `stale` / `failed` / `empty` from the pass records, and the settings
  (weights + enabled providers) read, validate and write with the server owning the defaults.
- `apps/api` — `/search` accepts `types`, `site_id`, `owner`, `language`, `status`, `updated`,
  `before`, `after` and `facets=true`; `GET /search/export` answers CSV (the whole result set, or
  `selected=`), with `X-Export-Rows` / `X-Export-Truncated`; `GET`/`PUT /search/settings` read and
  write the ranking (`search.read` / `search.manage`). An unusable filter value is named — `400`
  with the parameter's own code — never dropped.
- `apps/admin` — `/search` grew the facet rail, chips, 50 rows a page, Shift/`⌘A` selection,
  Copy links, CSV export, the keyboard map and (below `lg`) the filters in a sheet; `/settings/search`
  is new: documents, last indexed, state and last pass per provider with a Reindex button, the
  weights form (validated by the API's own rules), Restore defaults, and a progress line fed by the
  passes. The palette renders the three new providers, and its sections open the targets their rows
  name. The nav gained the index screen, so it is reachable from everywhere.
- QA: the walkthrough visits `/settings/search` on desktop and phone and runs a scripted depth
  phase — facet applied and removed, `s` sort, Shift-range, Copy links, one CSV export of the
  selection, the shortcut dialog, a per-provider reindex (progress line + the pass's numbers) and
  the weights form's refusal and save.

Acceptance evidence, this tick: `cargo test --workspace` → **436 passed, 0 failed** (five new walks:
facets under a filter, the export matching the result set and a selection, the ranking flip, the
settings' refusals, and Activity/Settings reachable with their own screens) · `pnpm typecheck &&
pnpm build` → 2/2 (the new route is in the build output) · `bash scripts/qa/run.sh` → 105 clicks ·
107 screenshots · **0 high findings**, **0 vision issues** (`qa-artifacts/20260926-160500`); the
depth phase's own numbers: 6 facet groups, a type facet narrowed 13 → 9 with one chip, removing it
left 0, `s` moved the sort to `newest`, a Shift-range selected 3 of 13 rows, Copy links put 3 links
on the clipboard, the exported CSV carried 3 rows, the settings screen reported 7 providers with
their states, and a reindex answered "Indexed 1 document in 15 ms" with the row's last pass reading
`1 written · 0 pruned · 15 ms`.

Deviations from the spec above, each deliberate:

- **The facets ride the search request** (`facets=true`) instead of a separate endpoint: same
  parameters, same `WHERE` clause, one round trip for the screen that renders them — and the
  palette's hot path (which never asks) does not pay for six extra counts.
- **`/search/actions` is not built.** The palette's actions are the panel's own (the screens, a new
  page), it renders them without a round trip, and no client calls that endpoint; shipping one with
  no caller would be exactly the dead surface this project forbids.
- **`logs` indexes what a click can open.** An audit entry about a role, an AI provider or an
  organization has no screen yet (REQ-006/REQ-012/REQ-039 own those), so only entries and events
  whose target resolves to a page, a file or a site are indexed — the rest join the provider the day
  their screens exist. This is the same rule as the palette's "no section for a screen that is not
  there", applied to rows.
- **`settings` is the key/value the panel actually has**: the search weights, one row per
  organization. A general settings store arrives with its own REQ and only adds a row to the same
  statement.
- **The export caps at 5 000 rows** and says so (`X-Export-Truncated`); the screen reports the count
  it received, so a capped file cannot pass for the whole result set.
- **The selection bar is sticky at every width** rather than a phone-only bottom bar (it does both
  jobs), and below `lg` the filters move into a sheet behind `Filters (n)`.
- **The vision pass's one low note was a misread**: it read the overview's `Connect a domain` row as
  an unchecked circle labelled Done, while the DOM and the API agree the step is done
  (`checklist[domain].done = true`, check icon rendered); the second pass reported 0 issues across
  all 14 screens.
